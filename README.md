# rusty_request

An async, hand-rolled HTTP client for Rust -- a take on Python's
`requests`, built on our own from-scratch async runtime
([`rusty_tokio`](https://github.com/baileyrd/rusty_tokio)) instead of
`tokio`. `rusty_tokio` is the only dependency. Everything above the raw
socket -- URL parsing, HTTP/1.1 request/response framing, JSON -- is
original code in this crate: no `hyper`, no `reqwest`, no `serde`, no
`url` crate.

## Why no TLS/HTTPS

Hand-rolling real TLS (cipher suites, certificate validation, key
exchange) is a serious cryptography undertaking, and a real security
risk if done wrong -- not something to improvise into an HTTP client
MVP. [`rustils`](https://github.com/baileyrd/rustils) (the platform
layer `rusty_tokio` itself is built on) is explicit that it has "no TLS
concept anywhere in any slice" today. So this crate is **`http://`
only** for now; HTTPS is tracked as a real backlog item (see below),
not silently unsupported or half-implemented.

## What's here (MVP)

- **Methods**: GET, POST, PUT, PATCH, DELETE, HEAD.
- **Client + top-level functions**: `rusty_request::get(url).await?` for
  one-off calls, or build a `Client` to share default headers, a
  timeout, and cookies across requests (mirrors `requests`' module-level
  functions + `Session` split). Cloning a `Client` shares the same
  underlying cookie jar -- it's the same logical session, not an
  independent copy. Unlike `Session`, this does not yet reuse TCP
  connections; see the backlog.
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
  the host that sent it," not full supercookie prevention.
- **HTTP/1.1 framing**: `Content-Length` and `Transfer-Encoding: chunked`
  response bodies, plus close-delimited (EOF-terminated) bodies as a
  fallback. No connection reuse -- every request opens a fresh TCP
  connection and sends `Connection: close`.
- **JSON**: a small hand-rolled `Value` enum with a parser/serializer
  (`rusty_request::Json`) -- no `serde`. No derive-based mapping to
  arbitrary Rust structs; build/read `Value`s directly.

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

- **HTTPS/TLS support** -- needs a dedicated, carefully-reviewed effort
  (likely a `rustils` Security-surface addition, or FFI into an OS TLS
  library), not something bolted on here.
- **Multipart file uploads**.
- **Retry/backoff**.
- **Streaming request and response bodies** -- everything is fully
  buffered in memory today.
- **Proxy support**.
- **Connection pooling / keep-alive** -- every request currently opens a
  fresh TCP connection.

## Testing

```
cargo build
cargo test           # unit tests (url/header/json parsing) plus
                      # integration tests against a local hand-rolled
                      # HTTP/1.1 server (tests/common), so nothing
                      # requires real network access
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
