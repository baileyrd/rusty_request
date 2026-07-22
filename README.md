# rusty_request

An async HTTP client for Rust -- a take on Python's `requests`, built on
our own from-scratch async runtime
([`rusty_tokio`](https://github.com/baileyrd/rusty_tokio)) instead of
`tokio`. URL parsing, HTTP/1.1 request/response framing, and the RFC
6265 cookie jar come from
[`rusty_http`](https://github.com/baileyrd/rusty_http), the rusty
ecosystem's one shared HTTP/1.1 message layer and `Url` type --
[`rusty_tail`](https://github.com/baileyrd/rusty_tail) is the other
consumer, using the same crate for its ts2021/DERP protocol upgrades and
its LocalAPI client/server. This crate's own connection pooling,
retry/redirect policy, proxy routing, and JSON are still original code:
no `hyper`, no `reqwest`, no `serde`, no `url` crate.

## TLS/HTTPS

`https://` is supported, via
[`rusty_tls`](https://github.com/baileyrd/rusty_tls) -- the ecosystem's
one shared TLS implementation and trust policy, not anything hand-rolled
here. Every `https://` request is verified by default against the system
trust store, with SNI from the URL host; this crate has no public API to
configure a different trust policy today. TLS 1.2/1.3, no ALPN (this
crate is HTTP/1.1-only), no client certificates.

## What's here (MVP)

- **Methods**: GET, POST, PUT, PATCH, DELETE, HEAD.
- **Client + top-level functions**: `rusty_request::get(url).await?` for
  one-off calls, or build a `Client` to share default headers, a
  timeout, cookies, and pooled connections across requests (mirrors
  `requests`' module-level functions + `Session` split). Cloning a
  `Client` shares the same underlying cookie jar and connection pool --
  it's the same logical session, not an independent copy.
- **Requests**: custom headers, query parameters (percent-encoded),
  string/bytes/JSON/form-urlencoded bodies, per-request or per-client
  timeouts.
- **Responses**: status code, headers, `.text()`, `.bytes()`, `.json()`,
  and a `requests`-style `.error_for_status()`.
- **Auth helpers**: `.basic_auth(user, pass)` (RFC 7617, via a small
  hand-rolled base64 encoder -- no `base64` crate) and
  `.bearer_auth(token)`, on both `RequestBuilder` (per-request) and
  `ClientBuilder` (applied to every request, overridable per-request).
  The URL parser still rejects `user:pass@host` userinfo syntax --
  these helpers are the supported way to set credentials for now.
- **Redirects**: 301/302/303/307/308 are followed automatically (capped
  at 30 hops by default; `.max_redirects(n)`/`.no_redirects()` on either
  `RequestBuilder` or `ClientBuilder` to change that). 303 always
  downgrades to a bodyless GET, 307/308 always preserve the original
  method and body, and 301/302 downgrade to a bodyless GET for any
  method other than GET/HEAD -- the same rules browsers and `requests`
  use, since the spec itself is looser. `Authorization` is stripped on
  any hop that changes host or port, so credentials never leak to a
  different origin.
- **Cookies**: every `Client` stores `Set-Cookie` responses and attaches
  matching cookies to later requests (RFC 6265 domain/path scoping,
  `Expires`/`Max-Age` expiry, `Secure`), including across redirect hops
  within one call -- the same default behavior `requests.Session` gives
  you. `Secure` cookies are never attached, since this crate is
  `http://`-only. `ClientBuilder::no_cookie_store()` disables cookie
  handling entirely for a client that shouldn't carry state. No
  public-suffix-list support -- the only cross-domain safety check is
  RFC 6265's own "a response may only set a `Domain` that's a suffix of
  the host that sent it," not full supercookie prevention. `Secure`
  cookies are attached only over `https://`.
- **HTTP/1.1 framing**: `Content-Length` and `Transfer-Encoding: chunked`
  response bodies, plus close-delimited (EOF-terminated) bodies as a
  fallback.
- **Connection pooling**: idle connections are kept per origin
  (scheme+host+port) and reused when the server allows it (HTTP/1.1's
  keep-alive default, unless it sends `Connection: close` or the
  response body was close-delimited) -- capped at 8 idle connections
  per origin and a 90-second idle timeout by default
  (`ClientBuilder::pool_max_idle_per_host`/`pool_idle_timeout`). A
  pooled connection the server already closed (a race no client can
  fully avoid) is retried once on a fresh connection rather than
  surfaced as an error, the same convention curl and `reqwest` use.
  `ClientBuilder::no_pool()` reverts to a fresh connection with
  `Connection: close` on every request.
- **JSON**: a small hand-rolled `Value` enum with a parser/serializer
  (`rusty_request::Json`) -- no `serde`. No derive-based mapping to
  arbitrary Rust structs; build/read `Value`s directly.
- **Retries**: opt-in via `.retry(RetryPolicy::new(max_retries))` on
  either `RequestBuilder` or `ClientBuilder` -- disabled by default.
  Retries connection errors and a configurable set of response statuses
  (429/500/502/503/504 by default), with fixed or exponential (jittered)
  backoff (`RetryPolicy::backoff`/`Backoff`), respects a `Retry-After`
  response header when present (capped at 60s by default so a server
  can't stall a caller indefinitely), and only retries idempotent
  methods (GET/HEAD/PUT/DELETE/OPTIONS) unless
  `RetryPolicy::retry_non_idempotent()` is set -- a retried POST/PATCH
  can otherwise duplicate a side effect the first attempt already
  caused. The `Client`/request-level `timeout` (if set) wraps every
  attempt and backoff sleep, not just the first one.
- **Multipart file uploads**: `.multipart(Multipart::new()...)` on
  `RequestBuilder` builds a `multipart/form-data` body (RFC 7578) --
  hand-rolled boundary generation and part framing, no dependency.
  `Multipart::text(name, value)` for plain fields,
  `Multipart::file(name, filename, bytes)` /
  `Multipart::file_with_content_type(...)` for file parts. Fully
  buffered in memory today, like every other request body.
- **Streaming bodies**: `Body::streaming(len, open)` builds a request
  body from an `AsyncRead` factory (`open` is called fresh for the
  first attempt, and again for any 307/308 redirect hop that preserves
  the body -- a single already-open reader can't be rewound or
  duplicated for a second hop). `len: Some(n)` sends `Content-Length`
  and streams the bytes as-is; `None` sends `Transfer-Encoding: chunked`
  for a source whose size isn't known upfront.
  `RequestBuilder::send_streaming()` mirrors this on the response side:
  returns as soon as the status/headers arrive, then pull the body
  incrementally via `StreamingResponse::chunk()` instead of buffering
  it all first -- redirects are still followed exactly like `.send()`.
  Two first-pass scope boundaries (documented on `send_streaming`): any
  configured `RetryPolicy` is ignored, and the connection used isn't
  returned to the pool afterward.
- **Proxy support**: `.proxy("http://host:port")` on `ClientBuilder`/
  `RequestBuilder` routes requests through an HTTP forward proxy instead
  of connecting to the origin directly -- reached over plain `http://`
  either way (an `https://` proxy URL is rejected). An `http://` request
  is forwarded in cleartext (absolute-form request-target, `Host` still
  naming the origin); an `https://` request instead opens a `CONNECT`
  tunnel to the origin and runs the TLS handshake and the real request
  through it, invisible to the proxy. `.proxy_from_env()` reads
  `HTTP_PROXY`/`http_proxy` and `NO_PROXY`/`no_proxy`, matching
  `requests`' convention -- with an ["httpoxy"](https://httpoxy.org)
  mitigation: `HTTP_PROXY` is ignored whenever `REQUEST_METHOD` is also
  set (the standard CGI-context signal), since a CGI/FastCGI handler can
  map an inbound `Proxy:` header onto that variable. `.proxy_bypass(hosts)`
  sets `NO_PROXY`-style bypass rules (exact host or subdomain match, or
  `*` for "never proxy anything"). A plain `http://` proxy connection is
  pooled under the proxy's own identity, so one persistent connection can
  carry requests for several different origins; a `CONNECT`-tunneled
  `https://` connection is pooled under a key specific to that (proxy,
  origin) pair instead, since the tunnel itself is a private,
  origin-specific channel once established.

## Example

```rust
use rusty_request::{Client, Json};

#[tokio::main] // substitute your rusty_tokio runtime entry point
async fn main() -> rusty_request::Result<()> {
    // One-off call:
    let resp = rusty_request::get("http://example.com/").await?;
    println!("{} {}", resp.status(), resp.text()?);

    // Reused client with default headers + JSON body:
    let client = Client::builder()
        .default_header("X-Api-Key", "secret")?
        .build();

    let mut body = Json::object();
    body.insert("name", "Ada");

    let resp = client
        .post("http://example.com/users")?
        .json(&body)?
        .send()
        .await?
        .error_for_status()?;

    let created = resp.json()?;
    println!("{:?}", created.get("id"));

    // Redirects are followed automatically; opt out per-request if you
    // want the raw 3xx instead:
    let raw_redirect = client
        .get("http://example.com/moved")?
        .no_redirects()
        .send()
        .await?;
    println!("{}", raw_redirect.status());

    Ok(())
}
```

## Backlog (deliberately out of scope for this MVP)

Tracked as issues in this repository:

- **A configurable TLS trust policy.** Every `https://` request uses
  `rusty_tls::TrustPolicy::System` today; there's no way to pin a
  private CA or opt into `DangerNoVerification` from this crate's own
  API.
- **HTTP/2 / ALPN.** This crate is HTTP/1.1-only.

## Testing

```
cargo build
cargo test           # unit tests (url/header/json parsing) plus
                      # integration tests against local hand-rolled
                      # HTTP/1.1 servers (tests/common) -- including a
                      # TLS one and a CONNECT-tunneling proxy one -- so
                      # nothing requires real network access
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
