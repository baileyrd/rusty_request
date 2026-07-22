use std::fmt;

/// Everything that can go wrong making a request, in one place -- the
/// same flattened shape Python's `requests.exceptions` gives callers
/// (one `RequestException`-rooted hierarchy) rather than making callers
/// match on io::Error/parse-error/etc. separately.
#[derive(Debug)]
pub enum Error {
    /// The URL couldn't be parsed at all.
    InvalidUrl(String),
    /// The URL's scheme isn't `http` or `https`.
    UnsupportedScheme(String),
    /// A header name or value contained bytes that can't go on the wire
    /// (e.g. a bare `\r` or `\n`, which would let a caller smuggle a
    /// second header/request into the stream).
    InvalidHeader(String),
    /// DNS resolution, connect, read, or write failed.
    Io(std::io::Error),
    /// Building the TLS layer itself failed -- before any network I/O --
    /// e.g. an invalid server name, or (see
    /// [`rusty_tls::TrustPolicy::System`]) a system with zero usable
    /// trust anchors. **Not** what a rejected certificate surfaces as:
    /// the handshake itself runs inside `AsyncRead`/`AsyncWrite`, which
    /// can only return [`Error::Io`] by contract, so a bad/expired/
    /// untrusted certificate or a hostname mismatch comes back wrapped
    /// there instead, carrying the original [`rusty_tls::Error`]'s
    /// message as its `io::Error` payload.
    Tls(rusty_tls::Error),
    /// A `CONNECT` tunnel through a configured proxy (required to reach
    /// an `https://` origin through an `http://` proxy) was rejected --
    /// the proxy responded to `CONNECT host:port` with a non-2xx status.
    ProxyConnectFailed(rusty_http::StatusCode),
    /// The peer's response didn't parse as HTTP/1.1, or its declared
    /// body framing was violated (e.g. the connection closed before the
    /// full body arrived).
    InvalidResponse(String),
    /// `.json()` was called but the body isn't valid JSON, or a JSON
    /// body was given whose structure doesn't match what was asked for.
    Json(String),
    /// The request did not complete within its configured timeout.
    Timeout,
    /// [`crate::Response::error_for_status`] was called on a response
    /// with a 4xx/5xx status.
    Status(rusty_http::StatusCode),
    /// A redirect chain exceeded the configured cap (see
    /// `RequestBuilder::max_redirects`/`ClientBuilder::max_redirects`)
    /// without settling on a non-redirect response.
    TooManyRedirects(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidUrl(s) => write!(f, "invalid url: {s}"),
            Error::UnsupportedScheme(s) => write!(
                f,
                "unsupported url scheme: {s} (only http:// and https:// are supported)"
            ),
            Error::InvalidHeader(s) => write!(f, "invalid header: {s}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Tls(e) => write!(f, "tls error: {e}"),
            Error::ProxyConnectFailed(status) => {
                write!(f, "proxy rejected CONNECT tunnel with status {status}")
            }
            Error::InvalidResponse(s) => write!(f, "invalid http response: {s}"),
            Error::Json(s) => write!(f, "json error: {s}"),
            Error::Timeout => write!(f, "request timed out"),
            Error::Status(s) => write!(f, "http error status {s}"),
            Error::TooManyRedirects(max) => {
                write!(f, "exceeded the maximum number of redirects ({max})")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Tls(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusty_tls::Error> for Error {
    fn from(e: rusty_tls::Error) -> Self {
        Error::Tls(e)
    }
}

impl From<rusty_http::Error> for Error {
    fn from(e: rusty_http::Error) -> Self {
        match e {
            rusty_http::Error::InvalidUrl(s) => Error::InvalidUrl(s),
            rusty_http::Error::UnsupportedScheme(s) => Error::UnsupportedScheme(s),
            rusty_http::Error::InvalidHeader(s) => Error::InvalidHeader(s),
            other => Error::InvalidResponse(other.to_string()),
        }
    }
}

/// `rusty_http`'s transport adapters report I/O failures and framing
/// violations through their own error type (the sans-IO core has no
/// `Io` variant of its own) -- both map onto this crate's existing
/// flattened `Error`.
impl From<rusty_http::TransportError> for Error {
    fn from(e: rusty_http::TransportError) -> Self {
        match e {
            rusty_http::TransportError::Io(io) => Error::Io(io),
            rusty_http::TransportError::Http(http) => http.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
