//! Shared test-only HTTP/1.1 server: just enough of a peer to exercise
//! `rusty_request`'s client against real sockets (via `rusty_tokio`,
//! the same runtime the client itself is built on) without needing
//! network access to a real host in CI/sandboxes. Every "connection" is
//! request/response/close, matching the client's own no-keep-alive MVP
//! behavior.

use rusty_tokio::io::{TcpListener, TcpStream};
use std::net::SocketAddr;
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
}

impl TestServer {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
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

    rusty_tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let handler = handler.clone();
            rusty_tokio::spawn(async move {
                if let Ok(req) = read_request(&stream).await {
                    let response = handler(&req);
                    let _ = stream.write_all(&response).await;
                }
            });
        }
    });

    TestServer { addr }
}

async fn read_request(stream: &TcpStream) -> std::io::Result<TestRequest> {
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
