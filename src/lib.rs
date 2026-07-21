//! `rusty_request` -- an async, hand-rolled HTTP client in the spirit of
//! Python's `requests`, with a single dependency: [`rusty_tokio`], our
//! own from-scratch async runtime. Everything above the raw socket --
//! URL parsing, HTTP/1.1 request/response framing, and JSON -- is
//! original code in this crate; no `hyper`, no `reqwest`, no `serde`,
//! no `url` crate.
//!
//! # MVP scope
//!
//! - Methods: GET/POST/PUT/PATCH/DELETE/HEAD, custom headers, query
//!   params, string/bytes/JSON/form-urlencoded request bodies, a
//!   `Client` for shared default headers/timeout plus bare top-level
//!   convenience functions ([`get`], [`post`], ...) for one-off calls.
//! - Response: status, headers, `.text()`/`.bytes()`/`.json()`,
//!   `.error_for_status()`.
//! - Auth helpers: `.basic_auth(user, pass)`/`.bearer_auth(token)` on
//!   both `RequestBuilder` and `ClientBuilder` (RFC 7617 Basic auth
//!   uses a small hand-rolled base64 encoder -- no `base64` crate).
//! - `http://` only. **No HTTPS/TLS** -- hand-rolling TLS crypto is a
//!   serious security risk to improvise in an MVP; see the README and
//!   issue tracker for the tracked follow-up.
//! - A fresh TCP connection per request (`Connection: close`) -- no
//!   connection pooling/keep-alive yet.
//!
//! Everything else (redirects, a `Session`-style cookie jar, multipart
//! uploads, retries, streaming bodies, proxies, connection reuse) is
//! deliberately deferred -- see the README's backlog section and the
//! repository's issue tracker.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> rusty_request::Result<()> {
//! let resp = rusty_request::get("http://example.com/").await?;
//! println!("{}", resp.status());
//! println!("{}", resp.text()?);
//! # Ok(())
//! # }
//! ```

mod base64;
mod body;
mod client;
mod error;
mod header;
mod http1;
mod json;
mod method;
mod request;
mod response;
mod status;
mod url;

pub use body::Body;
pub use client::{Client, ClientBuilder, RequestBuilder};
pub use error::{Error, Result};
pub use header::HeaderMap;
pub use json::Value as Json;
pub use method::Method;
pub use request::Request;
pub use response::Response;
pub use status::StatusCode;
pub use url::Url;

/// `GET url` via a fresh, default [`Client`]. For repeated requests
/// (shared headers, a shared timeout, eventually connection reuse),
/// build a [`Client`] once and reuse it instead.
pub async fn get(url: &str) -> Result<Response> {
    Client::new().get(url)?.send().await
}

pub async fn post(url: &str) -> Result<Response> {
    Client::new().post(url)?.send().await
}

pub async fn put(url: &str) -> Result<Response> {
    Client::new().put(url)?.send().await
}

pub async fn patch(url: &str) -> Result<Response> {
    Client::new().patch(url)?.send().await
}

pub async fn delete(url: &str) -> Result<Response> {
    Client::new().delete(url)?.send().await
}

pub async fn head(url: &str) -> Result<Response> {
    Client::new().head(url)?.send().await
}
