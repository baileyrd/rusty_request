use crate::body::Body;
use crate::error::{Error, Result};
use crate::json;
use crate::multipart::Multipart;
use crate::pool::{ConnectionPool, PoolKey};
use crate::proxy::{NoProxyRules, Proxy};
use crate::request::Request;
use crate::response::Response;
use crate::retry::RetryPolicy;
use crate::stream::Conn;
use crate::streaming::StreamingResponse;
use rusty_http::async_tokio::{AsyncTransport, BodyReader};
use rusty_http::body::{self, Framing};
use rusty_http::cookie::CookieJar;
use rusty_http::head::RequestHead;
use rusty_http::url::percent_encode;
use rusty_http::{HeaderMap, Method, StatusCode, Url, Version};
use rusty_tls::TrustPolicy;
use rusty_tokio::io::{AsyncReadExt, TcpStream};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bytes at a time a streaming request body ([`Body::Stream`]) is relayed
/// onto the wire per read -- also the max response head size a server
/// this crate deliberately connected to is trusted to send within (see
/// [`MAX_RESPONSE_HEAD_LEN`]'s own doc for why that's a larger bound
/// than `rusty_http`'s own untrusted-input default).
const CHUNK_SIZE: usize = 8192;

/// A generous head-size bound for a response this crate's own caller
/// chose to connect to -- much larger than `rusty_http::head::
/// DEFAULT_MAX_HEAD_LEN` (8 KiB, tuned for a core that also has to
/// parse untrusted, server-bound requests). A client reading its own
/// server's response is a more trusted context, matching this crate's
/// original (pre-migration) bound.
const MAX_RESPONSE_HEAD_LEN: usize = 8 * 1024 * 1024;

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
    proxy: Option<Proxy>,
    proxy_bypass: NoProxyRules,
    trust_policy: TrustPolicy,
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
            proxy: self.proxy.clone(),
            proxy_bypass: self.proxy_bypass.clone(),
            trust_policy: self.trust_policy.clone(),
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
    proxy: Option<Proxy>,
    proxy_bypass: NoProxyRules,
    trust_policy: TrustPolicy,
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
            proxy: None,
            proxy_bypass: NoProxyRules::default(),
            trust_policy: TrustPolicy::default(),
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

    /// Routes every plain `http://` request built from the resulting
    /// [`Client`] through the given HTTP forward proxy instead of
    /// connecting to the origin directly, unless a
    /// [`ClientBuilder::proxy_bypass`] rule matches the origin host.
    /// Disabled by default. See [`Proxy::http`] for the URL format and
    /// scope (no `https://` proxy, no `CONNECT` tunnel).
    pub fn proxy(mut self, proxy_url: &str) -> Result<Self> {
        self.proxy = Some(Proxy::http(proxy_url)?);
        Ok(self)
    }

    /// Reads a proxy configuration from the environment, matching
    /// `requests`' `HTTP_PROXY`/`NO_PROXY` convention: `HTTP_PROXY` (or
    /// lowercase `http_proxy`) sets the proxy exactly like
    /// [`ClientBuilder::proxy`], and `NO_PROXY`/`no_proxy` sets the
    /// bypass rules exactly like [`ClientBuilder::proxy_bypass`]. Either
    /// left unset simply leaves that part of the configuration alone.
    ///
    /// `HTTPS_PROXY` isn't read -- this crate is `http://`-only, so it
    /// could never apply to anything today. As a mitigation for
    /// ["httpoxy"](https://httpoxy.org) (CVE-2016-5385 and siblings) --
    /// a CGI/FastCGI handler that maps an inbound `Proxy:` request
    /// header onto the `HTTP_PROXY` environment variable, letting a
    /// remote client redirect this process's own outbound requests --
    /// the uppercase `HTTP_PROXY` is ignored whenever `REQUEST_METHOD`
    /// is also set (the standard signal that the process is running in
    /// a CGI context, the same check curl uses); the lowercase
    /// `http_proxy` is never attacker-reachable via a header and is
    /// always trusted.
    pub fn proxy_from_env(mut self) -> Result<Self> {
        if let Some(url) = env_http_proxy() {
            self.proxy = Some(Proxy::http(&url)?);
        }
        if let Some(spec) = env_no_proxy() {
            self.proxy_bypass = NoProxyRules::parse(&spec);
        }
        Ok(self)
    }

    /// Hosts that skip the configured proxy and connect directly (RFC-less
    /// but universal `NO_PROXY` convention): each entry matches a host
    /// exactly or as a dot-boundary suffix (`example.com` also matches
    /// `www.example.com`), except `*`, which bypasses the proxy for
    /// every host. Only meaningful alongside [`ClientBuilder::proxy`]/
    /// [`ClientBuilder::proxy_from_env`].
    pub fn proxy_bypass<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.proxy_bypass = NoProxyRules::from_hosts(hosts);
        self
    }

    /// Disables proxying entirely -- the default; only useful to
    /// override a previously-set proxy earlier in the same builder
    /// chain.
    pub fn no_proxy(mut self) -> Self {
        self.proxy = None;
        self
    }

    /// How every `https://` request built from the resulting [`Client`]
    /// decides whether to trust the server it connects to. Defaults to
    /// [`TrustPolicy::System`] (the OS trust store) -- the same behavior
    /// this crate always had, just now overridable. Use
    /// [`crate::pinned_anchors`] to pin a private CA, or
    /// [`TrustPolicy::DangerNoVerification`] to disable verification
    /// entirely (never for production use -- no protection against an
    /// active man-in-the-middle). Ignored for `http://` requests.
    pub fn trust_policy(mut self, policy: TrustPolicy) -> Self {
        self.trust_policy = policy;
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
            proxy: self.proxy,
            proxy_bypass: self.proxy_bypass,
            trust_policy: self.trust_policy,
        }
    }
}

/// httpoxy mitigation (see [`ClientBuilder::proxy_from_env`]'s docs):
/// `HTTP_PROXY` is only trusted outside a CGI context; `http_proxy`
/// always is.
fn env_http_proxy() -> Option<String> {
    let in_cgi_context = std::env::var_os("REQUEST_METHOD").is_some();
    if !in_cgi_context {
        if let Ok(v) = std::env::var("HTTP_PROXY") {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    std::env::var("http_proxy").ok().filter(|v| !v.is_empty())
}

fn env_no_proxy() -> Option<String> {
    std::env::var("NO_PROXY")
        .ok()
        .or_else(|| std::env::var("no_proxy").ok())
        .filter(|v| !v.is_empty())
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
    proxy: Option<Proxy>,
    proxy_bypass: NoProxyRules,
    trust_policy: TrustPolicy,
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

    /// Routes this request through the given HTTP forward proxy,
    /// overriding the `Client`'s default. See [`ClientBuilder::proxy`].
    pub fn proxy(mut self, proxy_url: &str) -> Result<Self> {
        self.proxy = Some(Proxy::http(proxy_url)?);
        Ok(self)
    }

    /// Bypass rules for this request, overriding the `Client`'s
    /// default. See [`ClientBuilder::proxy_bypass`].
    pub fn proxy_bypass<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.proxy_bypass = NoProxyRules::from_hosts(hosts);
        self
    }

    /// Disables proxying for this request, overriding the `Client`'s
    /// default.
    pub fn no_proxy(mut self) -> Self {
        self.proxy = None;
        self
    }

    /// How this request (if `https://`) decides whether to trust the
    /// server it connects to, overriding the `Client`'s default. See
    /// [`ClientBuilder::trust_policy`].
    pub fn trust_policy(mut self, policy: TrustPolicy) -> Self {
        self.trust_policy = policy;
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
            proxy,
            proxy_bypass,
            trust_policy,
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
            proxy,
            proxy_bypass,
            trust_policy,
        );
        match request_timeout {
            Some(d) => match rusty_tokio::time::timeout(d, fut).await {
                Ok(inner) => inner,
                Err(_elapsed) => Err(Error::Timeout),
            },
            None => fut.await,
        }
    }

    /// Like [`RequestBuilder::send`], but returns as soon as the status
    /// line and headers arrive instead of buffering the whole body
    /// first -- pull it incrementally via [`StreamingResponse::chunk`].
    /// Follows redirects exactly like `.send()` does (each redirect
    /// hop's own body is still read and discarded eagerly, same as
    /// before; only the final, returned response is left unread).
    ///
    /// Two differences from `.send()`, both deliberate first-pass
    /// scope boundaries (see the streaming-bodies issue): any
    /// configured [`RetryPolicy`] is ignored -- retrying a request whose
    /// body may itself be a single-use stream isn't supported yet -- and
    /// the connection used for the final hop is never returned to the
    /// pool afterward, since whether it's still safe to reuse isn't
    /// known until the body has been fully drained, which this first
    /// pass doesn't track. A pooled connection is still tried first
    /// for the request itself, same as `.send()`.
    ///
    /// The configured timeout, if any, only bounds getting to the
    /// `StreamingResponse` (through any redirects) -- not each
    /// subsequent `.chunk()` call, since those happen after this method
    /// has already returned.
    pub async fn send_streaming(self) -> Result<StreamingResponse> {
        let RequestBuilder {
            method,
            url,
            headers,
            body,
            timeout: request_timeout,
            redirect_policy,
            cookie_jar,
            pool,
            retry_policy: _,
            proxy,
            proxy_bypass,
            trust_policy,
        } = self;

        let fut = send_with_redirects_streaming(
            method,
            url,
            headers,
            body,
            redirect_policy,
            cookie_jar,
            pool,
            proxy,
            proxy_bypass,
            trust_policy,
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
    proxy: Option<Proxy>,
    proxy_bypass: NoProxyRules,
    trust_policy: TrustPolicy,
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
            proxy,
            proxy_bypass,
            trust_policy,
        )
        .await;
    };

    let mut attempt = 0usize;
    loop {
        let result = send_with_redirects(
            method.clone(),
            url.clone(),
            headers.clone(),
            body.clone(),
            redirect_policy,
            cookie_jar.clone(),
            pool.clone(),
            proxy.clone(),
            proxy_bypass.clone(),
            trust_policy.clone(),
        )
        .await;

        let (should_retry, retry_after) = match &result {
            Ok(response) => (
                policy.should_retry_status(response.status().as_u16()),
                policy.retry_after(response.headers()),
            ),
            Err(e) => (policy.should_retry_error(e), None),
        };

        if !should_retry || !policy.allows_method(&method) || attempt >= policy.max_retries() {
            return result;
        }

        rusty_tokio::time::sleep(policy.delay_for(attempt, retry_after)).await;
        attempt += 1;
    }
}

/// Sends the request, following redirects per `policy`. The overall
/// timeout (if any) wraps this whole chain, not each individual hop --
/// otherwise a slow redirect chain could add hops to dodge it.
#[allow(clippy::too_many_arguments)]
async fn send_with_redirects(
    mut method: Method,
    mut url: Url,
    headers: HeaderMap,
    mut body: Body,
    policy: RedirectPolicy,
    cookie_jar: Option<Arc<Mutex<CookieJar>>>,
    pool: Option<Arc<ConnectionPool>>,
    proxy: Option<Proxy>,
    proxy_bypass: NoProxyRules,
    trust_policy: TrustPolicy,
) -> Result<Response> {
    let mut hop_headers = headers;
    let mut hop = 0usize;

    loop {
        if url.scheme != "http" && url.scheme != "https" {
            return Err(Error::UnsupportedScheme(url.scheme));
        }
        let proxy_for_hop = active_proxy(&proxy, &proxy_bypass, &url);

        let wire_headers =
            build_wire_headers(&hop_headers, &cookie_jar, &url, pool.is_some(), &body)?;

        let request = Request {
            method: method.clone(),
            url: url.clone(),
            headers: wire_headers,
            body: body.clone(),
            timeout: None,
        };
        let response =
            send_one_hop(pool.as_deref(), &request, proxy_for_hop, &trust_policy).await?;

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

/// Whether `proxy` applies to a request to `url`, re-evaluated fresh
/// for every hop (a redirect can land on a host `bypass` matches even
/// if the original one didn't, or vice versa).
fn active_proxy<'a>(
    proxy: &'a Option<Proxy>,
    bypass: &NoProxyRules,
    url: &Url,
) -> Option<&'a Proxy> {
    proxy.as_ref().filter(|_| !bypass.bypasses(&url.host))
}

/// Builds the headers actually sent on the wire for one hop: a default
/// `Accept`, any jar cookies merged with a caller-set `Cookie` header,
/// `Connection: close` when pooling is disabled, and `Content-Length` or
/// `Transfer-Encoding: chunked` depending on whether `body`'s length is
/// known upfront. Shared by the buffered and streaming send paths.
fn build_wire_headers(
    hop_headers: &HeaderMap,
    cookie_jar: &Option<Arc<Mutex<CookieJar>>>,
    url: &Url,
    pooling_enabled: bool,
    body: &Body,
) -> Result<HeaderMap> {
    let mut wire_headers = hop_headers.clone();
    if !wire_headers.contains("Accept") {
        wire_headers.insert("Accept", "*/*")?;
    }
    // Attach any cookies the jar has for this origin/path, merging with
    // (rather than overriding) a caller-set `Cookie` header. Looked up
    // fresh every hop since the jar's contents can change between hops
    // (a redirect response can itself set cookies).
    if let Some(jar) = cookie_jar {
        let jar_cookies = jar.lock().unwrap().cookie_header_for(url);
        if let Some(jar_cookies) = jar_cookies {
            let merged = match wire_headers.get("Cookie") {
                Some(existing) => format!("{existing}; {jar_cookies}"),
                None => jar_cookies,
            };
            wire_headers.insert("Cookie", &merged)?;
        }
    }
    // With pooling disabled, say so honestly on the wire too. With it
    // enabled, HTTP/1.1's persistent-by-default behavior applies -- no
    // explicit header needed, and a caller's own `Connection` header
    // (if any) is left alone.
    if !pooling_enabled {
        wire_headers.insert("Connection", "close")?;
    }
    // Always computed from the real body, never trusted from a
    // caller-supplied header. A body whose length isn't known upfront
    // (a `Body::Stream` built without one) falls back to chunked
    // framing instead.
    match body.content_length() {
        Some(len) => {
            wire_headers.insert("Content-Length", &len.to_string())?;
        }
        None => {
            wire_headers.insert("Transfer-Encoding", "chunked")?;
        }
    }
    Ok(wire_headers)
}

/// Like [`send_with_redirects`], but leaves the final (non-redirect)
/// hop's body unread instead of buffering it -- see
/// [`RequestBuilder::send_streaming`]. Every intermediate redirect hop's
/// body is still fully drained (and discarded) before moving to the
/// next hop, the same as the buffered path does implicitly by always
/// reading eagerly -- there's no way to know a hop is a redirect (and
/// therefore not the one to hand back to the caller) until after its
/// status/headers have already arrived, so every hop uses the streaming
/// read path and only redirect hops get drained afterward.
#[allow(clippy::too_many_arguments)]
async fn send_with_redirects_streaming(
    mut method: Method,
    mut url: Url,
    headers: HeaderMap,
    mut body: Body,
    policy: RedirectPolicy,
    cookie_jar: Option<Arc<Mutex<CookieJar>>>,
    pool: Option<Arc<ConnectionPool>>,
    proxy: Option<Proxy>,
    proxy_bypass: NoProxyRules,
    trust_policy: TrustPolicy,
) -> Result<StreamingResponse> {
    let mut hop_headers = headers;
    let mut hop = 0usize;

    loop {
        if url.scheme != "http" && url.scheme != "https" {
            return Err(Error::UnsupportedScheme(url.scheme));
        }
        let proxy_for_hop = active_proxy(&proxy, &proxy_bypass, &url);

        let wire_headers =
            build_wire_headers(&hop_headers, &cookie_jar, &url, pool.is_some(), &body)?;

        let request = Request {
            method: method.clone(),
            url: url.clone(),
            headers: wire_headers,
            body: body.clone(),
            timeout: None,
        };
        let (status, resp_headers, mut streaming_body) =
            send_one_hop_streaming(pool.as_deref(), &request, proxy_for_hop, &trust_policy).await?;

        if let Some(jar) = &cookie_jar {
            let set_cookie_values: Vec<&str> = resp_headers
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

        if !is_redirect_status(status.as_u16()) {
            return Ok(StreamingResponse::new(
                status,
                resp_headers,
                url,
                streaming_body,
            ));
        }
        let RedirectPolicy::Follow(max) = policy else {
            return Ok(StreamingResponse::new(
                status,
                resp_headers,
                url,
                streaming_body,
            ));
        };
        if hop >= max {
            return Err(Error::TooManyRedirects(max));
        }

        // This hop won't be returned to the caller -- drain and discard
        // its body before moving to the next hop.
        while streaming_body.next_chunk().await?.is_some() {}

        let location = resp_headers
            .get("location")
            .ok_or_else(|| {
                Error::InvalidResponse(format!("{status} redirect response had no Location header"))
            })?
            .to_string();
        let next_url = url.resolve_redirect(&location)?;

        let cross_origin =
            !next_url.host.eq_ignore_ascii_case(&url.host) || next_url.port != url.port;
        if cross_origin {
            hop_headers.remove("Authorization");
        }

        (method, body) = redirect_method_and_body(status.as_u16(), method, body);
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

/// Where a hop's TCP connection actually goes, what request-target/pool
/// key that implies, and what (if any) `CONNECT`-tunnel/TLS setup the raw
/// socket needs before the request itself can be sent -- routed through
/// `proxy` when it applies to this hop (see [`active_proxy`]), straight
/// to the origin otherwise.
///
/// Three shapes, not two: a plain `http://` proxy hop forwards the
/// request in absolute-form (RFC 7230 §5.3.2) over a connection pooled
/// under the *proxy's* identity, since one persistent connection to the
/// proxy can carry requests for several different origins in turn (same
/// as a browser configured with an HTTP proxy). An `https://` hop
/// through a proxy is different in kind, not just in scheme: the proxy
/// can't read (let alone rewrite) encrypted traffic, so the client first
/// asks it to open an opaque `CONNECT` tunnel to the origin, then runs
/// the *entire* TLS handshake and request through that tunnel exactly as
/// if connected directly -- origin-form request-target, `Host` naming
/// the origin, no absolute-form. That makes a tunneled connection a
/// private, origin-specific channel that happens to be routed via the
/// proxy, not a shareable one, so it gets its own pool-key namespace
/// (see `Proxy::tunnel_pool_key`) rather than the proxy's shared key.
struct HopTarget {
    connect_host: String,
    connect_port: u16,
    /// Set only for an `https://` hop routed through a proxy: the
    /// origin's (host, port) to `CONNECT` the raw socket to before any
    /// TLS handshake begins.
    connect_tunnel: Option<(String, u16)>,
    /// Set whenever this hop needs a TLS handshake before the request is
    /// sent (the SNI/hostname-verification name to use) -- every
    /// `https://` hop, direct or tunneled; never set for `http://`.
    tls_server_name: Option<String>,
    pool_key: PoolKey,
    request_target: String,
}

fn hop_target(url: &Url, proxy: Option<&Proxy>) -> HopTarget {
    let is_https = url.scheme == "https";
    match proxy {
        Some(p) if is_https => HopTarget {
            connect_host: p.host.clone(),
            connect_port: p.port,
            connect_tunnel: Some((url.host.clone(), url.port)),
            tls_server_name: Some(url.host.clone()),
            pool_key: p.tunnel_pool_key(url),
            request_target: url.request_target(),
        },
        Some(p) => HopTarget {
            connect_host: p.host.clone(),
            connect_port: p.port,
            connect_tunnel: None,
            tls_server_name: None,
            pool_key: p.pool_key(),
            request_target: url.absolute_form(),
        },
        None => HopTarget {
            connect_host: url.host.clone(),
            connect_port: url.port,
            connect_tunnel: None,
            tls_server_name: is_https.then(|| url.host.clone()),
            pool_key: pool_key(url),
            request_target: url.request_target(),
        },
    }
}

/// Turns a freshly dialed, plain TCP connection into the [`Conn`]
/// `target` actually calls for: a `CONNECT`-tunnel handshake first, if
/// `target.connect_tunnel` says this hop needs one (`https://` routed
/// through a proxy), then a TLS handshake, if `target.tls_server_name`
/// says so (any `https://` hop, tunneled or direct) -- verified per
/// `trust_policy` (see [`ClientBuilder::trust_policy`]).
async fn establish(raw: TcpStream, target: &HopTarget, trust_policy: &TrustPolicy) -> Result<Conn> {
    if let Some((host, port)) = &target.connect_tunnel {
        let status = connect_tunnel(&raw, host, *port).await?;
        if !status.is_success() {
            return Err(Error::ProxyConnectFailed(status));
        }
    }
    match &target.tls_server_name {
        Some(name) => {
            let tls = rusty_tls::AsyncTlsStream::new(raw, name, trust_policy)?;
            Ok(Conn::Tls(Box::new(tls)))
        }
        None => Ok(Conn::Plain(raw)),
    }
}

/// A `CONNECT` response head longer than this is treated as a protocol
/// error rather than read indefinitely.
const MAX_CONNECT_RESPONSE_LEN: usize = 64 * 1024;

/// Sends `CONNECT host:port HTTP/1.1` on `stream` (a fresh, plain TCP
/// connection to a proxy) and reads the status line of its response --
/// `AsyncTransport::read_response_head` stops exactly at the blank line
/// that ends the head, so a successful (2xx) `CONNECT` response (which
/// never carries a body) never over-reads into what's actually the start
/// of the tunneled TLS handshake. A failing response might carry a body,
/// but this crate has no use for it beyond the status code, so it's left
/// undrained -- `t.into_inner()` (dropped, here) would discard it anyway.
async fn connect_tunnel(stream: &TcpStream, host: &str, port: u16) -> Result<StatusCode> {
    let authority = format!("{host}:{port}");
    let mut t = AsyncTransport::new(stream);
    let head = RequestHead {
        method: Method::Connect,
        target: authority.clone(),
        version: Version::Http11,
        headers: {
            let mut h = HeaderMap::new();
            h.insert("Host", &authority)?;
            h
        },
    };
    t.write_request_head(&head).await?;
    let resp_head = t.read_response_head(MAX_CONNECT_RESPONSE_LEN).await?;
    Ok(resp_head.status)
}

/// A response too small to have landed a real result yet; the wire
/// contents of one head+body request/response round trip.
struct RawResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
    /// Whether `stream` is still usable for another request after this
    /// response: the body framing left the stream in a known-clean
    /// state (i.e. wasn't read-to-EOF) *and* the response didn't send
    /// `Connection: close`.
    keep_alive: bool,
    /// Handed back so the caller can pool it (when `keep_alive`).
    stream: Conn,
}

/// Builds the `RequestHead` for one hop: `Host` first (matching every
/// real client/server's convention), then `request.headers` (the wire
/// headers `build_wire_headers` already assembled) in their original
/// order.
fn build_request_head(request: &Request, request_target: &str) -> Result<RequestHead> {
    let mut headers = HeaderMap::new();
    headers.insert("Host", &request.url.host_header())?;
    for (name, value) in request.headers.iter() {
        headers.append(name, value)?;
    }
    Ok(RequestHead {
        method: request.method.clone(),
        target: request_target.to_string(),
        version: Version::Http11,
        headers,
    })
}

/// Writes `body` onto the wire: raw passthrough for a fully-buffered
/// body (`Content-Length` already covers its framing), or relayed
/// through [`write_stream_body`] for a [`Body::Stream`].
async fn write_request_body(t: &mut AsyncTransport<Conn>, body: &Body) -> Result<()> {
    match body {
        Body::Empty => {}
        Body::Bytes(b) => {
            if !b.is_empty() {
                t.write_body(b).await?;
            }
        }
        Body::Stream(s) => write_stream_body(t, s).await?,
    }
    Ok(())
}

/// Relays a streaming request body onto the wire: raw passthrough when
/// its length was declared upfront (`Content-Length` already covers
/// framing), or `Transfer-Encoding: chunked` framing when it wasn't.
async fn write_stream_body(
    t: &mut AsyncTransport<Conn>,
    body: &crate::body::StreamBody,
) -> Result<()> {
    let mut reader = body.open();
    let known_length = body.len().is_some();
    let mut buf = [0u8; CHUNK_SIZE];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if known_length {
            t.write_body(&buf[..n]).await?;
        } else {
            t.write_chunk(&buf[..n]).await?;
        }
    }
    if !known_length {
        t.write_chunked_end().await?;
    }
    Ok(())
}

/// A `Connection` header can list multiple tokens (`Connection:
/// keep-alive, Upgrade`); `close` anywhere in the list means the peer is
/// closing the connection after this response.
fn connection_says_close(headers: &HeaderMap) -> bool {
    headers
        .get("connection")
        .map(|v| {
            v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("close"))
        })
        .unwrap_or(false)
}

async fn attempt(stream: Conn, request: &Request, request_target: &str) -> Result<RawResponse> {
    let mut t = AsyncTransport::new(stream);
    let head = build_request_head(request, request_target)?;
    t.write_request_head(&head).await?;
    write_request_body(&mut t, &request.body).await?;

    let resp_head = t.read_response_head(MAX_RESPONSE_HEAD_LEN).await?;
    let framing = body::response_framing(&resp_head.headers, &request.method, resp_head.status)?;
    let header_says_close = connection_says_close(&resp_head.headers);
    let keep_alive = !matches!(framing, Framing::Close) && !header_says_close;
    let body_bytes = t.read_body(framing).await?;

    Ok(RawResponse {
        status: resp_head.status,
        headers: resp_head.headers,
        body: body_bytes,
        keep_alive,
        stream: t.into_inner(),
    })
}

async fn attempt_streaming(
    stream: Conn,
    request: &Request,
    request_target: &str,
) -> Result<(StatusCode, HeaderMap, BodyReader<Conn>)> {
    let mut t = AsyncTransport::new(stream);
    let head = build_request_head(request, request_target)?;
    t.write_request_head(&head).await?;
    write_request_body(&mut t, &request.body).await?;

    let resp_head = t.read_response_head(MAX_RESPONSE_HEAD_LEN).await?;
    let framing = body::response_framing(&resp_head.headers, &request.method, resp_head.status)?;
    let reader = t.into_body_reader(framing);
    Ok((resp_head.status, resp_head.headers, reader))
}

/// Sends one request, reusing a pooled connection for this origin (or,
/// with `proxy` set, this proxy -- see [`hop_target`]) when one's
/// available. A pooled connection can be stale -- the server may have
/// closed it after its own idle timeout, a race no client can fully
/// avoid -- so any failure on a *pooled* attempt is treated as exactly
/// that and retried once on a fresh connection (the same one-retry
/// convention curl and `reqwest` use), rather than surfaced to the
/// caller as a confusing I/O error. A failure on the fresh attempt is
/// real and does propagate.
async fn send_one_hop(
    pool: Option<&ConnectionPool>,
    request: &Request,
    proxy: Option<&Proxy>,
    trust_policy: &TrustPolicy,
) -> Result<Response> {
    let target = hop_target(&request.url, proxy);

    if let Some(pool) = pool {
        if let Some(stream) = pool.take(&target.pool_key) {
            if let Ok(raw) = attempt(stream, request, &target.request_target).await {
                if raw.keep_alive {
                    pool.put(target.pool_key, raw.stream);
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

    let addrs = resolve(target.connect_host.clone(), target.connect_port).await?;
    let raw = connect(&addrs).await?;
    let stream = establish(raw, &target, trust_policy).await?;
    let raw = attempt(stream, request, &target.request_target).await?;
    if raw.keep_alive {
        if let Some(pool) = pool {
            pool.put(target.pool_key, raw.stream);
        }
    }
    Ok(Response::new(
        raw.status,
        raw.headers,
        request.url.clone(),
        raw.body,
    ))
}

/// Like [`send_one_hop`], but leaves the body unread -- see
/// [`attempt_streaming`]. The connection is never returned to the pool
/// afterward (deliberate; see [`RequestBuilder::send_streaming`]'s
/// docs), though a pooled one is still tried first and falls back to a
/// fresh connection on failure, same as the buffered path.
async fn send_one_hop_streaming(
    pool: Option<&ConnectionPool>,
    request: &Request,
    proxy: Option<&Proxy>,
    trust_policy: &TrustPolicy,
) -> Result<(StatusCode, HeaderMap, BodyReader<Conn>)> {
    let target = hop_target(&request.url, proxy);

    if let Some(pool) = pool {
        if let Some(stream) = pool.take(&target.pool_key) {
            if let Ok(result) = attempt_streaming(stream, request, &target.request_target).await {
                return Ok(result);
            }
        }
    }

    let addrs = resolve(target.connect_host.clone(), target.connect_port).await?;
    let raw = connect(&addrs).await?;
    let stream = establish(raw, &target, trust_policy).await?;
    attempt_streaming(stream, request, &target.request_target).await
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
