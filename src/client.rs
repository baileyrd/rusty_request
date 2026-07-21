use crate::body::Body;
use crate::error::{Error, Result};
use crate::header::HeaderMap;
use crate::http1;
use crate::json;
use crate::method::Method;
use crate::request::Request;
use crate::response::Response;
use crate::url::{percent_encode, Url};
use rusty_tokio::io::TcpStream;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("rusty_request/", env!("CARGO_PKG_VERSION"));

/// A reusable request configuration: default headers applied to every
/// request built from it, plus a default timeout. Cheap to `Clone`
/// (headers aside, everything else is `Copy`) -- unlike Python
/// `requests.Session`, this does **not** yet persist cookies or reuse
/// TCP connections across requests (see the backlog).
#[derive(Debug, Clone)]
pub struct Client {
    default_headers: HeaderMap,
    timeout: Option<Duration>,
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

    pub fn build(self) -> Client {
        Client {
            default_headers: self.default_headers,
            timeout: self.timeout,
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

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub async fn send(self) -> Result<Response> {
        let RequestBuilder {
            method,
            url,
            mut headers,
            body,
            timeout: request_timeout,
        } = self;

        if url.scheme != "http" {
            return Err(Error::UnsupportedScheme(url.scheme));
        }

        if !headers.contains("Accept") {
            headers.insert("Accept", "*/*")?;
        }
        // No connection reuse in this MVP (see the backlog), so every
        // request is honest about that on the wire too.
        headers.insert("Connection", "close")?;
        // Always computed from the real body, never trusted from a
        // caller-supplied header.
        headers.insert("Content-Length", &body.as_bytes().len().to_string())?;

        let request = Request {
            method,
            url: url.clone(),
            headers,
            body,
            timeout: request_timeout,
        };

        let fut = send_over_new_connection(&request);
        match request_timeout {
            Some(d) => match rusty_tokio::time::timeout(d, fut).await {
                Ok(inner) => inner,
                Err(_elapsed) => Err(Error::Timeout),
            },
            None => fut.await,
        }
    }
}

fn basic_auth_header(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        crate::base64::encode(format!("{username}:{password}").as_bytes())
    )
}

async fn send_over_new_connection(request: &Request) -> Result<Response> {
    let addrs = resolve(request.url.host.clone(), request.url.port).await?;
    let stream = connect(&addrs).await?;
    let raw = http1::send_request(
        &stream,
        request.method,
        &request.url.request_target(),
        &request.url.host_header(),
        &request.headers,
        request.body.as_bytes(),
    )
    .await?;
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
