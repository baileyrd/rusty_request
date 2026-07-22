//! `rusty_request` -- an async HTTP client in the spirit of Python's
//! `requests`, built on [`rusty_tokio`] (our own from-scratch async
//! runtime), [`rusty_tls`](https://github.com/baileyrd/rusty_tls) (the
//! ecosystem's one shared TLS implementation and trust policy), and
//! [`rusty_http`](https://github.com/baileyrd/rusty_http) (the
//! ecosystem's one shared HTTP/1.1 message layer and `Url` type). URL
//! parsing, HTTP/1.1 request/response framing, and the RFC 6265 cookie
//! jar all come from `rusty_http` now -- this crate's own connection
//! pooling, retry/redirect policy, proxy routing, and JSON are still
//! original code; no `hyper`, no `reqwest`, no `serde`, no `url` crate.
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
//! - Automatic redirect following (301/302/303/307/308, capped at 30
//!   hops by default via `.max_redirects(n)`/`.no_redirects()` on
//!   either `RequestBuilder` or `ClientBuilder`), with the
//!   method/body-preservation rules RFC 9110 §15.4 and browsers/
//!   `requests` actually use, and `Authorization` stripped on any
//!   cross-origin hop.
//! - Cookies: every `Client` stores `Set-Cookie` responses (RFC 6265 --
//!   domain/path scoping, `Expires`/`Max-Age` expiry, `Secure`) and
//!   attaches matching cookies to later requests through the same
//!   `Client`, including across redirect hops -- the same behavior
//!   `requests.Session` gives by default. `ClientBuilder::no_cookie_store`
//!   opts out.
//! - `http://` and `https://`. TLS is [`rusty_tls`](https://github.com/baileyrd/rusty_tls)'s,
//!   not hand-rolled: verified by default against the system trust store,
//!   SNI from the URL host, TLS 1.2/1.3. No ALPN (this crate is
//!   HTTP/1.1-only), no client certificates, no custom trust policy --
//!   see the README and issue tracker for tracked follow-ups.
//! - Connection pooling: every `Client` reuses idle connections per
//!   origin when the server allows it (HTTP/1.1's keep-alive default),
//!   bounded by a per-origin idle cap and timeout, with a stale pooled
//!   connection transparently retried once on a fresh connection.
//!   `ClientBuilder::no_pool` opts out.
//! - Retries: opt-in via `ClientBuilder::retry`/`RequestBuilder::retry`
//!   with a [`RetryPolicy`] -- connection errors and a configurable set
//!   of statuses (429/500/502/503/504 by default), fixed or exponential
//!   (with jitter) backoff, `Retry-After` respected and capped, and only
//!   idempotent methods retried unless explicitly opted into for
//!   POST/PATCH too. Disabled by default.
//! - Multipart file uploads: `RequestBuilder::multipart(Multipart)` --
//!   hand-rolled `multipart/form-data` encoding (RFC 7578), one or more
//!   named text/file parts, no dependency. Fully buffered in memory, like
//!   every other request body today.
//! - Streaming bodies: `Body::streaming(len, open)` for a request body
//!   produced incrementally rather than fully buffered upfront (raw
//!   passthrough with `Content-Length` if `len` is known, chunked
//!   `Transfer-Encoding` if not); `RequestBuilder::send_streaming()` for
//!   a [`StreamingResponse`] whose body is pulled incrementally via
//!   `.chunk()` instead of requiring the whole thing in memory first.
//!   Two first-pass scope boundaries, both documented on
//!   `send_streaming`: it ignores any configured `RetryPolicy`, and its
//!   connection is never pooled afterward.
//! - Proxy support: `ClientBuilder::proxy(url)` (or
//!   `.proxy_from_env()` for `HTTP_PROXY`/`http_proxy`, with an httpoxy-
//!   style mitigation -- see `src/proxy.rs`) routes requests through an
//!   HTTP forward proxy: `http://` requests are forwarded in
//!   absolute-form (`Host` still naming the origin); `https://` requests
//!   tunnel through the proxy via `CONNECT`, so the proxy never sees
//!   the encrypted traffic. `NO_PROXY`-style bypass rules via
//!   `.proxy_bypass(hosts)`/`.proxy_from_env()`'s `NO_PROXY`/`no_proxy`.
//!
//! Everything else is deliberately deferred -- see the README's backlog
//! section and the repository's issue tracker.
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
mod json;
mod multipart;
mod pool;
mod proxy;
mod rand;
mod request;
mod response;
mod retry;
mod stream;
mod streaming;

pub use body::Body;
pub use client::{Client, ClientBuilder, RequestBuilder};
pub use error::{Error, Result};
pub use json::Value as Json;
pub use multipart::Multipart;
pub use proxy::Proxy;
pub use request::Request;
pub use response::Response;
pub use retry::{Backoff, RetryPolicy};
pub use rusty_http::{HeaderMap, Method, StatusCode, Url};
pub use streaming::StreamingResponse;

/// `GET url` via a fresh, default [`Client`]. For repeated requests
/// (shared headers, a shared timeout, connection reuse, cookies),
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
