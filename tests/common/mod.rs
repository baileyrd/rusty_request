//! Shared test-only HTTP/1.1 server: just enough of a peer to exercise
//! `rusty_request`'s client against real sockets (via `rusty_tokio`,
//! the same runtime the client itself is built on) without needing
//! network access to a real host in CI/sandboxes. Each accepted
//! connection serves requests in a loop (rather than exactly one) so
//! connection-reuse behavior can be exercised and observed via
//! [`TestServer::connections_accepted`].

use rusty_tokio::io::{TcpListener, TcpStream};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub struct TestRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl TestRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct TestServer {
    pub addr: SocketAddr,
    connections: Arc<AtomicUsize>,
}

impl TestServer {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// How many distinct TCP connections this server has accepted so
    /// far -- lower than the number of requests made means the client
    /// reused a connection.
    pub fn connections_accepted(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// Starts a background accept loop on an ephemeral port. Must be called
/// from within a running `rusty_tokio::Runtime` (i.e. inside
/// `rt.block_on(...)`). The server, and every per-connection task it
/// spawns, is torn down when the owning `Runtime` is dropped at the end
/// of the test.
pub fn start_test_server<F>(handler: F) -> TestServer
where
    F: Fn(&TestRequest) -> Vec<u8> + Send + Sync + 'static,
{
    let listener =
        TcpListener::bind("127.0.0.1:0".parse().unwrap()).expect("failed to bind test server");
    let addr = listener.local_addr().expect("failed to read local_addr");
    let handler = Arc::new(handler);
    let connections = Arc::new(AtomicUsize::new(0));
    let connections_for_task = connections.clone();

    rusty_tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            connections_for_task.fetch_add(1, Ordering::SeqCst);
            let handler = handler.clone();
            rusty_tokio::spawn(async move {
                // Serve requests on this connection until the client
                // closes it, or this connection's own framing rules
                // require closing it first -- not just one request --
                // so a reused connection is actually served rather than
                // immediately dropped out from under it.
                loop {
                    let req = match read_request(&stream).await {
                        Ok(req) => req,
                        Err(_) => break,
                    };
                    let response = handler(&req);
                    let must_close = response_requires_close(&response);
                    if stream.write_all(&response).await.is_err() || must_close {
                        break;
                    }
                }
            });
        }
    });

    TestServer { addr, connections }
}

pub async fn read_request(stream: &TcpStream) -> std::io::Result<TestRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before headers completed",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(TestRequest {
        method,
        target,
        headers,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether this test server must close the connection after sending
/// `response`, mirroring the same HTTP/1.1 framing rules the client
/// itself follows: an explicit `Connection: close`, or -- since a
/// close-delimited body is *defined* by "read until the peer closes" --
/// no `Content-Length` and no chunked `Transfer-Encoding` at all.
fn response_requires_close(response: &[u8]) -> bool {
    let header_end = find_subslice(response, b"\r\n\r\n").unwrap_or(response.len());
    let head = String::from_utf8_lossy(&response[..header_end]);

    let mut has_content_length = false;
    let mut has_chunked = false;
    let mut says_close = false;
    for line in head.split("\r\n").skip(1) {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        if k.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        } else if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
            has_chunked = true;
        } else if k.eq_ignore_ascii_case("connection")
            && v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("close"))
        {
            says_close = true;
        }
    }
    says_close || !(has_content_length || has_chunked)
}

/// Builds a raw HTTP/1.1 response: status line + headers + body, with
/// `Content-Length` computed automatically.
pub fn http_response(status: u16, reason: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body);
    out
}

/// Runs `future` to completion on a fresh single-test `rusty_tokio`
/// runtime. Dropping the runtime at the end of this call tears down any
/// background tasks (e.g. a `TestServer`'s accept loop) started during
/// the test.
pub fn run<F: std::future::Future>(future: F) -> F::Output {
    rusty_tokio::Runtime::new()
        .expect("failed to build test runtime")
        .block_on(future)
}

/// Builds a chunked-transfer-encoded response from `chunks`.
pub fn http_chunked_response(
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    chunks: &[&[u8]],
) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
    for chunk in chunks {
        out.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}
