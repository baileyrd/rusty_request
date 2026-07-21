//! HTTP/1.1 wire framing: request-head serialization and response
//! parsing (status line, headers, and body -- `Content-Length`,
//! `Transfer-Encoding: chunked`, or close-delimited as a last resort)
//! over a `rusty_tokio` [`TcpStream`]. Close-delimited bodies (no
//! `Content-Length`, no chunked encoding -- legal in HTTP/1.0-flavored
//! responses) are read to EOF rather than treated as an error; doing so
//! also means the stream can never be pooled afterward (see
//! `RawResponse::keep_alive` and `crate::pool`), since reaching EOF is
//! exactly the peer having already closed it.

use crate::error::{Error, Result};
use crate::header::HeaderMap;
use crate::method::Method;
use crate::status::StatusCode;
use rusty_tokio::io::TcpStream;

/// A line (status line, header line, chunk-size line, ...) that never
/// arrived can't grow the read buffer forever -- this bounds it.
const MAX_LINE_LEN: usize = 8 * 1024 * 1024;

pub(crate) struct RawResponse {
    pub status: StatusCode,
    #[allow(dead_code)] // not yet exposed on `Response`; kept for future use / debugging
    pub reason: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// Whether `stream` is still usable for another request after this
    /// response: the body framing left the stream in a known-clean
    /// state (i.e. wasn't read-to-EOF) *and* the response didn't send
    /// `Connection: close`.
    pub keep_alive: bool,
}

pub(crate) async fn send_request(
    stream: &TcpStream,
    method: Method,
    request_target: &str,
    host_header: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<RawResponse> {
    let mut head = String::new();
    head.push_str(method.as_str());
    head.push(' ');
    head.push_str(request_target);
    head.push_str(" HTTP/1.1\r\n");
    head.push_str("Host: ");
    head.push_str(host_header);
    head.push_str("\r\n");
    for (name, value) in headers.iter() {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }

    let mut reader = BufReader::new(stream);

    let status_line = reader
        .read_line()
        .await?
        .ok_or_else(|| Error::InvalidResponse("connection closed before any response".into()))?;
    let (status, reason) = parse_status_line(&status_line)?;

    let mut resp_headers = HeaderMap::new();
    loop {
        let line = reader.read_line().await?.ok_or_else(|| {
            Error::InvalidResponse("connection closed while reading headers".into())
        })?;
        if line.is_empty() {
            break;
        }
        let (name, value) = parse_header_line(&line)?;
        resp_headers.append(&name, &value)?;
    }

    let (body, framing_reusable) = read_body(&mut reader, &resp_headers, method, status).await?;

    // A `Connection` header can list multiple tokens
    // (`Connection: keep-alive, Upgrade`); `close` anywhere in the list
    // means the peer is closing the connection after this response.
    let header_says_close = resp_headers
        .get("connection")
        .map(|v| {
            v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("close"))
        })
        .unwrap_or(false);

    Ok(RawResponse {
        status,
        reason,
        headers: resp_headers,
        body,
        keep_alive: framing_reusable && !header_says_close,
    })
}

/// Reads the response body, returning it alongside whether `reader`'s
/// underlying stream is still in a known-clean state afterward (i.e.
/// safe to send another request on, framing-wise -- a caller still
/// needs to check the `Connection` header too, which this function
/// doesn't see).
///
/// This is only sound because this crate never pipelines: a caller
/// always awaits a full response before sending its next request on
/// the same stream, so a server never has a reason to send more than
/// one response's worth of bytes before that next request arrives. If
/// that ever changes, the "fresh `BufReader` per call, no carryover"
/// design this relies on would need to carry buffered-but-unconsumed
/// bytes forward between calls instead of discarding them.
async fn read_body(
    reader: &mut BufReader<'_>,
    headers: &HeaderMap,
    method: Method,
    status: StatusCode,
) -> Result<(Vec<u8>, bool)> {
    // RFC 7230 §3.3.3: these never carry a body regardless of what the
    // headers claim.
    if method == Method::Head
        || status.as_u16() == 204
        || status.as_u16() == 304
        || (100..200).contains(&status.as_u16())
    {
        return Ok((Vec::new(), true));
    }

    let is_chunked = headers
        .get("transfer-encoding")
        .map(|v| {
            v.split(',')
                .next_back()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("chunked")
        })
        .unwrap_or(false);

    if is_chunked {
        return Ok((read_chunked_body(reader).await?, true));
    }

    if let Some(len) = headers.get("content-length") {
        let len: usize = len
            .trim()
            .parse()
            .map_err(|_| Error::InvalidResponse(format!("invalid Content-Length `{len}`")))?;
        return Ok((reader.read_exact_n(len).await?, true));
    }

    // No framing header at all: close-delimited body, read to EOF.
    // Legal for an HTTP/1.0-style response, but the stream is dead
    // afterward either way -- reaching EOF means the peer already
    // closed it, so it can never be pooled.
    Ok((reader.read_to_end().await?, false))
}

async fn read_chunked_body(reader: &mut BufReader<'_>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line = reader
            .read_line()
            .await?
            .ok_or_else(|| Error::InvalidResponse("connection closed reading chunk size".into()))?;
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| Error::InvalidResponse(format!("invalid chunk size `{size_str}`")))?;
        if size == 0 {
            // Trailer section: zero or more header lines, then a blank
            // line. This MVP doesn't surface trailers, just consumes
            // them so the stream is left in a consistent state.
            loop {
                let trailer = reader.read_line().await?.ok_or_else(|| {
                    Error::InvalidResponse("connection closed reading chunk trailer".into())
                })?;
                if trailer.is_empty() {
                    break;
                }
            }
            break;
        }
        let chunk = reader.read_exact_n(size).await?;
        out.extend_from_slice(&chunk);
        let terminator = reader
            .read_line()
            .await?
            .ok_or_else(|| Error::InvalidResponse("connection closed after chunk data".into()))?;
        if !terminator.is_empty() {
            return Err(Error::InvalidResponse(
                "malformed chunk terminator".to_string(),
            ));
        }
    }
    Ok(out)
}

fn parse_status_line(line: &str) -> Result<(StatusCode, String)> {
    let mut parts = line.splitn(3, ' ');
    let version = parts
        .next()
        .ok_or_else(|| Error::InvalidResponse("empty status line".into()))?;
    if !version.starts_with("HTTP/1.") {
        return Err(Error::InvalidResponse(format!(
            "unsupported HTTP version `{version}`"
        )));
    }
    let code = parts
        .next()
        .ok_or_else(|| Error::InvalidResponse("missing status code".into()))?;
    let code: u16 = code
        .parse()
        .map_err(|_| Error::InvalidResponse(format!("invalid status code `{code}`")))?;
    let reason = parts.next().unwrap_or("").to_string();
    Ok((StatusCode::from_u16(code), reason))
}

fn parse_header_line(line: &str) -> Result<(String, String)> {
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| Error::InvalidResponse(format!("malformed header line `{line}`")))?;
    Ok((name.trim().to_string(), value.trim().to_string()))
}

/// A small buffered reader over a borrowed [`TcpStream`]. Needed because
/// header lines have to be read byte-by-byte-ish (scanning for `\n`)
/// while the socket itself has no internal buffering -- and because
/// scanning ahead for a line terminator can read past it into body
/// bytes that already arrived in the same packet, which then need to be
/// handed back to whichever body-reading routine runs next rather than
/// re-read from the socket.
struct BufReader<'s> {
    stream: &'s TcpStream,
    buf: Vec<u8>,
    /// `buf[start..end]` is the unread, already-received data.
    start: usize,
    end: usize,
}

impl<'s> BufReader<'s> {
    fn new(stream: &'s TcpStream) -> Self {
        BufReader {
            stream,
            buf: vec![0u8; 8192],
            start: 0,
            end: 0,
        }
    }

    /// Reads more data from the socket into the buffer. Returns `false`
    /// on EOF (zero-byte read).
    async fn fill(&mut self) -> Result<bool> {
        if self.start > 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        if self.end == self.buf.len() {
            if self.buf.len() >= MAX_LINE_LEN {
                return Err(Error::InvalidResponse(
                    "response line exceeded the maximum allowed length".into(),
                ));
            }
            self.buf.resize((self.buf.len() * 2).min(MAX_LINE_LEN), 0);
        }
        let n = self.stream.read(&mut self.buf[self.end..]).await?;
        self.end += n;
        Ok(n > 0)
    }

    /// Reads one `\n`-terminated line (a trailing `\r` is stripped),
    /// returning `None` only if the connection closed with nothing left
    /// to read. A connection that closes mid-line is a protocol error,
    /// not a clean EOF.
    async fn read_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(pos) = self.buf[self.start..self.end]
                .iter()
                .position(|&b| b == b'\n')
            {
                let line_end = self.start + pos;
                let mut line = &self.buf[self.start..line_end];
                if line.last() == Some(&b'\r') {
                    line = &line[..line.len() - 1];
                }
                let s = String::from_utf8(line.to_vec())
                    .map_err(|_| Error::InvalidResponse("non-UTF-8 response line".into()))?;
                self.start = line_end + 1;
                return Ok(Some(s));
            }
            if !self.fill().await? {
                if self.start < self.end {
                    return Err(Error::InvalidResponse(
                        "connection closed mid-line".to_string(),
                    ));
                }
                return Ok(None);
            }
        }
    }

    async fn read_exact_n(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n.min(1 << 20));
        while out.len() < n {
            if self.start == self.end && !self.fill().await? {
                return Err(Error::InvalidResponse(
                    "connection closed before the full body arrived".to_string(),
                ));
            }
            let take = (n - out.len()).min(self.end - self.start);
            out.extend_from_slice(&self.buf[self.start..self.start + take]);
            self.start += take;
        }
        Ok(out)
    }

    async fn read_to_end(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            if self.start < self.end {
                out.extend_from_slice(&self.buf[self.start..self.end]);
                self.start = self.end;
            }
            if !self.fill().await? {
                return Ok(out);
            }
        }
    }
}
