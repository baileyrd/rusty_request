use crate::body::Body;
use crate::cookie::CookieJar;
use crate::error::{Error, Result};
use crate::header::HeaderMap;
use crate::http1;
use crate::json;
use crate::method::Method;
use crate::multipart::Multipart;
use crate::pool::{ConnectionPool, PoolKey};
use crate::request::Request;
use crate::response::Response;
use crate::retry::RetryPolicy;
use crate::url::{percent_encode, Url};
use rusty_tokio::io::TcpStream;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_REDIRECTS: usize = 30;
const DEFAULT_MAX_IDLE_PER_HOST: usize = 8;
const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const USER_AGENT: &str = concat!("rusty_request/", env!("CARGO_PKG_VERSION"));

/// How a redirect (3xx `Location`) response should be handled.
/// `Follow(n)` chases up to `n` hops before returning
/// [`Error::TooManyRedirects`]; `None` returns the raw 3xx response
/// immediately, matching `requests`' `allow_redirects=False`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectPolicy {
    Follow(usize),
    None,
}

/// A reusable request configuration: default headers, a default
/// timeout, and (by default) a shared cookie jar and connection pool,
/// applied to every request built from it. Cheap to `Clone` -- every
/// clone shares the same underlying cookie jar and pool (both
/// `Arc`-backed), matching Python `requests.Session` semantics: it's
/// the same logical session, not an independent copy.
#[derive(Debug, Clone)]
pub struct Client {
    default_headers: HeaderMap,
    timeout: Option<Duration>,
    redirect_policy: RedirectPolicy,
    cookie_jar: Option<Arc<Mutex<CookieJar>>>,
    pool: Option<Arc<ConnectionPool>>,
    retry_policy: Option<RetryPolicy>,
}

impl Client {
    pub fn new() -> Self {
        ClientBuilder::new().build()
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    pub fn request(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        let url = Url::parse(url)?;
        Ok(RequestBuilder {
            method,
            url,
            headers: self.default_headers.clone(),
            body: Body::Empty,
            timeout: self.timeout,
            redirect_policy: self.redirect_policy,
            cookie_jar: self.cookie_jar.clone(),
            pool: self.pool.clone(),
            retry_policy: self.retry_policy.clone(),
        })
    }

    pub fn get(&self, url: &str) -> Result<RequestBuilder> {
        self.request(Method::Get, url)
    }

    pub fn post(&self, url: &str) -> Result<RequestBuilder> {
        self.request(Method::Post, url)
    }

    pub fn put(&self, url: &str) -> Result<RequestBuilder> {
        self.request(Method::Put, url)
    }

    pub fn patch(&self, url: &str) -> Result<RequestBuilder> {
        self.request(Method::Patch, url)
    }

    pub fn delete(&self, url: &str) -> Result<RequestBuilder> {
        self.request(Method::Delete, url)
    }

    pub fn head(&self, url: &str) -> Result<RequestBuilder> {
        self.request(Method::Head, url)
    }
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

pub struct ClientBuilder {
    default_headers: HeaderMap,
    timeout: Option<Duration>,
    redirect_policy: RedirectPolicy,
    cookie_jar: Option<Arc<Mutex<CookieJar>>>,
    pool_max_idle_per_host: usize,
    pool_idle_timeout: Duration,
    pool_enabled: bool,
    retry_policy: Option<RetryPolicy>,
}

impl ClientBuilder {
    pub fn new() -> Self {
        let mut default_headers = HeaderMap::new();
        default_headers
            .insert("User-Agent", USER_AGENT)
            .expect("constant User-Agent value is always valid");
        ClientBuilder {
            default_headers,
            timeout: Some(DEFAULT_TIMEOUT),
            redirect_policy: RedirectPolicy::Follow(DEFAULT_MAX_REDIRECTS),
            cookie_jar: Some(Arc::new(Mutex::new(CookieJar::new()))),
            pool_max_idle_per_host: DEFAULT_MAX_IDLE_PER_HOST,
            pool_idle_timeout: DEFAULT_POOL_IDLE_TIMEOUT,
            pool_enabled: true,
            retry_policy: None,
        }
    }

    /// Sets a header sent on every request built from the resulting
    /// [`Client`] (e.g. `Authorization`), unless a specific request
    /// overrides it with its own `.header(...)` call.
    pub fn default_header(mut self, name: &str, value: &str) -> Result<Self> {
        self.default_headers.insert(name, value)?;
        Ok(self)
    }

    /// Sets `Authorization: Basic <base64(username:password)>` (RFC
    /// 7617) on every request built from the resulting [`Client`],
    /// unless a specific request overrides it with its own
    /// `.basic_auth(...)`/`.header("Authorization", ...)` call.
    pub fn basic_auth(mut self, username: &str, password: &str) -> Result<Self> {
        self.default_headers
            .insert("Authorization", &basic_auth_header(username, password))?;
        Ok(self)
    }

    /// Sets `Authorization: Bearer <token>` on every request built from
    /// the resulting [`Client`], unless a specific request overrides it.
    pub fn bearer_auth(mut self, token: &str) -> Result<Self> {
        self.default_headers
            .insert("Authorization", &format!("Bearer {token}"))?;
        Ok(self)
    }

    /// The per-request timeout applied unless a request sets its own.
    /// Default is 30 seconds; see [`ClientBuilder::no_timeout`] to
    /// disable it entirely.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn no_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Caps how many redirect hops are followed before a request fails
    /// with [`Error::TooManyRedirects`] (default 30, matching
    /// `requests`). Set to `0` to require the very first response to be
    /// non-redirect. See also [`ClientBuilder::no_redirects`] to skip
    /// following entirely.
    pub fn max_redirects(mut self, max: usize) -> Self {
        self.redirect_policy = RedirectPolicy::Follow(max);
        self
    }

    /// Never follows redirects -- every request built from the
    /// resulting [`Client`] returns the raw 3xx response instead,
    /// matching `requests`' `allow_redirects=False`.
    pub fn no_redirects(mut self) -> Self {
        self.redirect_policy = RedirectPolicy::None;
        self
    }

    /// Disables cookie storage entirely -- by default every [`Client`]
    /// stores `Set-Cookie` responses and attaches matching cookies to
    /// later requests (RFC 6265), the same as `requests.Session`. Call
    /// this for a client that should never carry state between requests.
    pub fn no_cookie_store(mut self) -> Self {
        self.cookie_jar = None;
        self
    }

    /// Caps how many idle connections are kept per origin
    /// (scheme+host+port) for reuse (default 8). Excess idle
    /// connections are simply closed rather than pooled. See also
    /// [`ClientBuilder::no_pool`] to disable connection reuse entirely.
    pub fn pool_max_idle_per_host(mut self, max: usize) -> Self {
        self.pool_max_idle_per_host = max;
        self
    }

    /// How long an idle pooled connection may sit before it's no longer
    /// offered for reuse (default 90 seconds). This is a client-side
    /// bound only -- the server may close its end sooner, which is
    /// handled by transparently retrying once on a fresh connection
    /// (see the crate README).
    pub fn pool_idle_timeout(mut self, timeout: Duration) -> Self {
        self.pool_idle_timeout = timeout;
        self
    }

    /// Disables connection pooling entirely -- every request opens a
    /// fresh TCP connection and sends `Connection: close`, matching
    /// this crate's original (pre-pooling) behavior.
    pub fn no_pool(mut self) -> Self {
        self.pool_enabled = false;
        self
    }

    /// Enables automatic retries for requests built from the resulting
    /// [`Client`], governed by `policy`. Disabled by default -- see
    /// [`crate::RetryPolicy`] for what a retry does and doesn't cover.
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = Some(policy);
        self
    }

    /// Disables automatic retries -- the default; only useful to
    /// override a previously-set policy earlier in the same builder
    /// chain.
    pub fn no_retry(mut self) -> Self {
        self.retry_policy = None;
        self
    }

    pub fn build(self) -> Client {
        Client {
            default_headers: self.default_headers,
            timeout: self.timeout,
            redirect_policy: self.redirect_policy,
            cookie_jar: self.cookie_jar,
            pool: self.pool_enabled.then(|| {
                Arc::new(ConnectionPool::new(
                    self.pool_max_idle_per_host,
                    self.pool_idle_timeout,
                ))
            }),
            retry_policy: self.retry_policy,
        }
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        ClientBuilder::new()
    }
}

pub struct RequestBuilder {
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Body,
    timeout: Option<Duration>,
    redirect_policy: RedirectPolicy,
    cookie_jar: Option<Arc<Mutex<CookieJar>>>,
    pool: Option<Arc<ConnectionPool>>,
    retry_policy: Option<RetryPolicy>,
}

impl RequestBuilder {
    pub fn header(mut self, name: &str, value: &str) -> Result<Self> {
        self.headers.insert(name, value)?;
        Ok(self)
    }

    /// Sets `Authorization: Basic <base64(username:password)>` (RFC
    /// 7617) on this request, overriding any `Authorization` set at the
    /// `Client` level.
    ///
    /// The URL parser deliberately rejects `user:pass@host` userinfo
    /// syntax (see `src/url.rs`) -- this and [`RequestBuilder::bearer_auth`]
    /// are the supported way to set credentials for now.
    pub fn basic_auth(mut self, username: &str, password: &str) -> Result<Self> {
        self.headers
            .insert("Authorization", &basic_auth_header(username, password))?;
        Ok(self)
    }

    /// Sets `Authorization: Bearer <token>` on this request, overriding
    /// any `Authorization` set at the `Client` level.
    pub fn bearer_auth(mut self, token: &str) -> Result<Self> {
        self.headers
            .insert("Authorization", &format!("Bearer {token}"))?;
        Ok(self)
    }

    /// Appends query parameters, percent-encoding each key/value and
    /// merging with any query string already present in the URL.
    pub fn query<I, K, V>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.url = self.url.with_query_pairs(pairs);
        self
    }

    pub fn body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Serializes `value` as the request body and sets
    /// `Content-Type: application/json`.
    pub fn json(mut self, value: &json::Value) -> Result<Self> {
        self.headers.insert("Content-Type", "application/json")?;
        self.body = Body::from(value.to_json_string());
        Ok(self)
    }

    /// Encodes `pairs` as `application/x-www-form-urlencoded` and uses
    /// that as the request body.
    pub fn form<I, K, V>(mut self, pairs: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut encoded = String::new();
        for (k, v) in pairs {
            if !encoded.is_empty() {
                encoded.push('&');
            }
            encoded.push_str(&percent_encode(k.as_ref()));
            encoded.push('=');
            encoded.push_str(&percent_encode(v.as_ref()));
        }
        self.headers
            .insert("Content-Type", "application/x-www-form-urlencoded")?;
        self.body = Body::from(encoded);
        Ok(self)
    }

    /// Encodes `form` as a `multipart/form-data` body (RFC 7578) and
    /// sets `Content-Type` to the matching `boundary=...` value.
    pub fn multipart(mut self, form: Multipart) -> Result<Self> {
        let (body, content_type) = form.encode();
        self.headers.insert("Content-Type", &content_type)?;
        self.body = Body::from(body);
        Ok(self)
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Caps how many redirect hops this request follows before failing
    /// with [`Error::TooManyRedirects`], overriding the `Client`'s
    /// default. See [`ClientBuilder::max_redirects`].
    pub fn max_redirects(mut self, max: usize) -> Self {
        self.redirect_policy = RedirectPolicy::Follow(max);
        self
    }

    /// Never follows redirects for this request -- returns the raw 3xx
    /// response instead, overriding the `Client`'s default. See
    /// [`ClientBuilder::no_redirects`].
    pub fn no_redirects(mut self) -> Self {
        self.redirect_policy = RedirectPolicy::None;
        self
    }

    /// Enables automatic retries for this request, governed by `policy`,
    /// overriding the `Client`'s default. See [`RetryPolicy`].
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = Some(policy);
        self
    }

    /// Disables automatic retries for this request, overriding the
    /// `Client`'s default.
    pub fn no_retry(mut self) -> Self {
        self.retry_policy = None;
        self
    }

    pub async fn send(self) -> Result<Response> {
        let RequestBuilder {
            method,
            url,
            headers,
            body,
            timeout: request_timeout,
            redirect_policy,
            cookie_jar,
            pool,
            retry_policy,
        } = self;

        let fut = send_with_retries(
            method,
            url,
            headers,
            body,
            redirect_policy,
            cookie_jar,
            pool,
            retry_policy,
        );
        match request_timeout {
            Some(d) => match rusty_tokio::time::timeout(d, fut).await {
                Ok(inner) => inner,
                Err(_elapsed) => Err(Error::Timeout),
            },
            None => fut.await,
        }
    }
}

/// Sends the request, retrying per `retry_policy` (if any) on top of
/// following redirects. The overall timeout (if any) wraps every attempt
/// and every backoff sleep, same as it already wraps a whole redirect
/// chain -- otherwise retries could let a request run well past its
/// configured deadline.
#[allow(clippy::too_many_arguments)]
async fn send_with_retries(
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Body,
    redirect_policy: RedirectPolicy,
    cookie_jar: Option<Arc<Mutex<CookieJar>>>,
    pool: Option<Arc<ConnectionPool>>,
    retry_policy: Option<RetryPolicy>,
) -> Result<Response> {
    let Some(policy) = retry_policy else {
        return send_with_redirects(
            method,
            url,
            headers,
            body,
            redirect_policy,
            cookie_jar,
            pool,
        )
        .await;
    };

    let mut attempt = 0usize;
    loop {
        let result = send_with_redirects(
            method,
            url.clone(),
            headers.clone(),
            body.clone(),
            redirect_policy,
            cookie_jar.clone(),
            pool.clone(),
        )
        .await;

        let (should_retry, retry_after) = match &result {
            Ok(response) => (
                policy.should_retry_status(response.status().as_u16()),
                policy.retry_after(response.headers()),
            ),
            Err(e) => (policy.should_retry_error(e), None),
        };

        if !should_retry || !policy.allows_method(method) || attempt >= policy.max_retries() {
            return result;
        }

        rusty_tokio::time::sleep(policy.delay_for(attempt, retry_after)).await;
        attempt += 1;
    }
}

/// Sends the request, following redirects per `policy`. The overall
/// timeout (if any) wraps this whole chain, not each individual hop --
/// otherwise a slow redirect chain could add hops to dodge it.
async fn send_with_redirects(
    mut method: Method,
    mut url: Url,
    headers: HeaderMap,
    mut body: Body,
    policy: RedirectPolicy,
    cookie_jar: Option<Arc<Mutex<CookieJar>>>,
    pool: Option<Arc<ConnectionPool>>,
) -> Result<Response> {
    let mut hop_headers = headers;
    let mut hop = 0usize;

    loop {
        if url.scheme != "http" {
            return Err(Error::UnsupportedScheme(url.scheme));
        }

        let mut wire_headers = hop_headers.clone();
        if !wire_headers.contains("Accept") {
            wire_headers.insert("Accept", "*/*")?;
        }
        // Attach any cookies the jar has for this origin/path, merging
        // with (rather than overriding) a caller-set `Cookie` header.
        // Looked up fresh every hop since the jar's contents can change
        // between hops (a redirect response can itself set cookies).
        if let Some(jar) = &cookie_jar {
            let jar_cookies = jar.lock().unwrap().cookie_header_for(&url);
            if let Some(jar_cookies) = jar_cookies {
                let merged = match wire_headers.get("Cookie") {
                    Some(existing) => format!("{existing}; {jar_cookies}"),
                    None => jar_cookies,
                };
                wire_headers.insert("Cookie", &merged)?;
            }
        }
        // With pooling disabled, say so honestly on the wire too. With
        // it enabled, HTTP/1.1's persistent-by-default behavior applies
        // -- no explicit header needed, and a caller's own `Connection`
        // header (if any) is left alone.
        if pool.is_none() {
            wire_headers.insert("Connection", "close")?;
        }
        // Always computed from the real body, never trusted from a
        // caller-supplied header.
        wire_headers.insert("Content-Length", &body.as_bytes().len().to_string())?;

        let request = Request {
            method,
            url: url.clone(),
            headers: wire_headers,
            body: body.clone(),
            timeout: None,
        };
        let response = send_one_hop(pool.as_deref(), &request).await?;

        // Store cookies from every hop's response, not just the final
        // one -- an intermediate redirect can set cookies too, and they
        // should be available both to later hops of this same chain and
        // to future requests through this `Client`.
        if let Some(jar) = &cookie_jar {
            let set_cookie_values: Vec<&str> = response
                .headers()
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                .map(|(_, value)| value)
                .collect();
            if !set_cookie_values.is_empty() {
                jar.lock()
                    .unwrap()
                    .store_from_response(&url, set_cookie_values.into_iter());
            }
        }

        if !is_redirect_status(response.status().as_u16()) {
            return Ok(response);
        }
        let RedirectPolicy::Follow(max) = policy else {
            return Ok(response);
        };
        if hop >= max {
            return Err(Error::TooManyRedirects(max));
        }

        let location = response
            .headers()
            .get("location")
            .ok_or_else(|| {
                Error::InvalidResponse(format!(
                    "{} redirect response had no Location header",
                    response.status()
                ))
            })?
            .to_string();
        let next_url = url.resolve_redirect(&location)?;

        // A redirect to a different host/port must not carry credentials
        // meant for the original origin along with it (the same class of
        // leak `requests` itself fixed after CVE-2018-18074).
        let cross_origin =
            !next_url.host.eq_ignore_ascii_case(&url.host) || next_url.port != url.port;
        if cross_origin {
            hop_headers.remove("Authorization");
        }

        (method, body) = redirect_method_and_body(response.status().as_u16(), method, body);
        url = next_url;
        hop += 1;
    }
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Per RFC 9110 §15.4: 303 always downgrades to a bodyless GET; 307/308
/// always preserve the original method and body; 301/302 are spec-loose
/// but conventionally (browsers, `requests`) downgrade to a bodyless GET
/// for any method other than GET/HEAD, which are left as-is.
fn redirect_method_and_body(status: u16, method: Method, body: Body) -> (Method, Body) {
    match status {
        303 => (Method::Get, Body::Empty),
        307 | 308 => (method, body),
        _ => {
            if matches!(method, Method::Get | Method::Head) {
                (method, body)
            } else {
                (Method::Get, Body::Empty)
            }
        }
    }
}

fn basic_auth_header(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        crate::base64::encode(format!("{username}:{password}").as_bytes())
    )
}

fn pool_key(url: &Url) -> PoolKey {
    (url.scheme.clone(), url.host.to_ascii_lowercase(), url.port)
}

async fn attempt(stream: &TcpStream, request: &Request) -> Result<http1::RawResponse> {
    http1::send_request(
        stream,
        request.method,
        &request.url.request_target(),
        &request.url.host_header(),
        &request.headers,
        request.body.as_bytes(),
    )
    .await
}

/// Sends one request, reusing a pooled connection for this origin when
/// one's available. A pooled connection can be stale -- the server may
/// have closed it after its own idle timeout, a race no client can
/// fully avoid -- so any failure on a *pooled* attempt is treated as
/// exactly that and retried once on a fresh connection (the same
/// one-retry convention curl and `reqwest` use), rather than surfaced
/// to the caller as a confusing I/O error. A failure on the fresh
/// attempt is real and does propagate.
async fn send_one_hop(pool: Option<&ConnectionPool>, request: &Request) -> Result<Response> {
    let key = pool_key(&request.url);

    if let Some(pool) = pool {
        if let Some(stream) = pool.take(&key) {
            if let Ok(raw) = attempt(&stream, request).await {
                if raw.keep_alive {
                    pool.put(key, stream);
                }
                return Ok(Response::new(
                    raw.status,
                    raw.headers,
                    request.url.clone(),
                    raw.body,
                ));
            }
        }
    }

    let addrs = resolve(request.url.host.clone(), request.url.port).await?;
    let stream = connect(&addrs).await?;
    let raw = attempt(&stream, request).await?;
    if raw.keep_alive {
        if let Some(pool) = pool {
            pool.put(key, stream);
        }
    }
    Ok(Response::new(
        raw.status,
        raw.headers,
        request.url.clone(),
        raw.body,
    ))
}

/// DNS resolution is a blocking OS call (`getaddrinfo` under the hood);
/// running it via [`rusty_tokio::spawn_blocking`] keeps it off the async
/// worker threads rather than stalling the reactor.
async fn resolve(host: String, port: u16) -> Result<Vec<SocketAddr>> {
    let handle = rusty_tokio::spawn_blocking(move || {
        (host.as_str(), port)
            .to_socket_addrs()
            .map(|it| it.collect::<Vec<_>>())
    });
    let resolved = handle
        .await
        .map_err(|e| {
            Error::Io(std::io::Error::other(format!(
                "DNS resolution task did not complete: {e}"
            )))
        })?
        .map_err(Error::Io)?;
    if resolved.is_empty() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "DNS resolution returned no addresses",
        )));
    }
    Ok(resolved)
}

/// Tries each resolved address in order (the same "happy eyeballs"-free
/// sequential fallback `std::net::TcpStream::connect` itself uses),
/// returning the first that connects.
async fn connect(addrs: &[SocketAddr]) -> Result<TcpStream> {
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect(*addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(Error::Io(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no addresses to connect to",
        )
    })))
}
