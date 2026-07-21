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
  one-off calls, or build a `Client` to share default headers and a
  timeout across requests (mirrors `requests`' module-level functions +
  `Session` split -- though unlike `Session`, this `Client` does not yet
  persist cookies or reuse connections; see the backlog).
- **Requests**: custom headers, query parameters (percent-encoded),
  string/bytes/JSON/form-urlencoded bodies, per-request or per-client
  timeouts.
- **Responses**: status code, headers, `.text()`, `.bytes()`, `.json()`,
  and a `requests`-style `.error_for_status()`.
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
    Ok(())
}
```

## Backlog (deliberately out of scope for this MVP)

Tracked as issues in this repository:

- **HTTPS/TLS support** -- needs a dedicated, carefully-reviewed effort
  (likely a `rustils` Security-surface addition, or FFI into an OS TLS
  library), not something bolted on here.
- **`Session`-style cookie jar** -- persisting cookies across requests
  made with the same `Client`.
- **Automatic redirect following** (3xx responses).
- **Multipart file uploads**.
- **Auth helpers** (HTTP Basic, Bearer tokens).
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
