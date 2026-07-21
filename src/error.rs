use std::fmt;

/// Everything that can go wrong making a request, in one place -- the
/// same flattened shape Python's `requests.exceptions` gives callers
/// (one `RequestException`-rooted hierarchy) rather than making callers
/// match on io::Error/parse-error/etc. separately.
#[derive(Debug)]
pub enum Error {
    /// The URL couldn't be parsed at all.
    InvalidUrl(String),
    /// The URL's scheme isn't supported yet. HTTPS is a known, tracked
    /// gap (see the crate README / issue tracker) -- hand-rolling TLS
    /// crypto is out of scope for a hand-rolled MVP.
    UnsupportedScheme(String),
    /// A header name or value contained bytes that can't go on the wire
    /// (e.g. a bare `\r` or `\n`, which would let a caller smuggle a
    /// second header/request into the stream).
    InvalidHeader(String),
    /// DNS resolution, connect, read, or write failed.
    Io(std::io::Error),
    /// The peer's response didn't parse as HTTP/1.1.
    InvalidResponse(String),
    /// `.json()` was called but the body isn't valid JSON, or a JSON
    /// body was given whose structure doesn't match what was asked for.
    Json(String),
    /// The request did not complete within its configured timeout.
    Timeout,
    /// [`crate::Response::error_for_status`] was called on a response
    /// with a 4xx/5xx status.
    Status(crate::status::StatusCode),
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
                "unsupported url scheme: {s} (only http:// is supported in this MVP)"
            ),
            Error::InvalidHeader(s) => write!(f, "invalid header: {s}"),
            Error::Io(e) => write!(f, "io error: {e}"),
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
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
