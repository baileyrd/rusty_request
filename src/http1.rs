//! HTTP/1.1 wire framing: request-head serialization and response
//! parsing (status line, headers, and body -- `Content-Length`,
//! `Transfer-Encoding: chunked`, or close-delimited as a last resort)
//! over a `rusty_tokio` [`TcpStream`]. Close-delimited bodies (no
//! `Content-Length`, no chunked encoding -- legal in HTTP/1.0-flavored
//! responses) are read to EOF rather than treated as an error; doing so
//! also means the stream can never be pooled afterward (see
//! `RawResponse::keep_alive` and `crate::pool`), since reaching EOF is
//! exactly the peer having already closed it.
//!
//! Two response-reading shapes share the same head-parsing and framing
//! logic: [`send_request`] (used by [`crate::RequestBuilder::send`])
//! eagerly drains the whole body into a `Vec` before returning, exactly
//! as before; [`send_request_streaming`] (used by
//! [`crate::RequestBuilder::send_streaming`]) returns as soon as the
//! status line and headers are parsed, leaving the body to be pulled
//! incrementally via [`StreamingBody::next_chunk`]. Both need to own the
//! `TcpStream` (not just borrow it) -- the streaming path so it can keep
//! reading from it after this module's functions return, and the
//! buffered path so it can hand the same connection back to the caller
//! afterward for pooling (`RawResponse::stream`).

use crate::body::Body;
use crate::error::{Error, Result};
use crate::header::HeaderMap;
use crate::method::Method;
use crate::status::StatusCode;
use rusty_tokio::io::{AsyncReadExt, TcpStream};

/// A line (status line, header line, chunk-size line, ...) that never
/// arrived can't grow the read buffer forever -- this bounds it.
const MAX_LINE_LEN: usize = 8 * 1024 * 1024;

/// How much of a chunk/content-length-remaining/close-delimited body
/// [`StreamingBody::next_chunk`] hands back per call, at most -- also
/// the write-side buffer size used to relay a streaming request body.
const CHUNK_SIZE: usize = 8192;

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
    /// Handed back so the caller can pool it (when `keep_alive`) --
    /// reading the response moved ownership of the connection into this
    /// module, so it has to come back out somehow.
    pub stream: TcpStream,
}

pub(crate) struct StreamingRawResponse {
    pub status: StatusCode,
    #[allow(dead_code)] // kept for parity with RawResponse / future use
    pub reason: String,
    pub headers: HeaderMap,
    pub body: StreamingBody,
}

pub(crate) async fn send_request(
    stream: TcpStream,
    method: Method,
    request_target: &str,
    host_header: &str,
    headers: &HeaderMap,
    body: &Body,
) -> Result<RawResponse> {
    write_request(&stream, method, request_target, host_header, headers, body).await?;
    let (status, reason, resp_headers, mut reader) = read_head(stream).await?;
    let framing = framing_for(&resp_headers, method, status)?;
    let (body, framing_reusable) = drain_body(&mut reader, framing).await?;
    let header_says_close = connection_says_close(&resp_headers);

    Ok(RawResponse {
        status,
        reason,
        headers: resp_headers,
        body,
        keep_alive: framing_reusable && !header_says_close,
        stream: reader.into_stream(),
    })
}

/// Like [`send_request`], but returns as soon as the head is parsed,
/// leaving the body to be pulled via [`StreamingBody::next_chunk`]. The
/// connection is never handed back for pooling -- whether it's still
/// safe to reuse isn't known until the body has been fully drained, and
/// this first pass doesn't track that (see `Client::send_streaming`'s
/// docs); it's simply dropped (closing the socket) once the
/// `StreamingBody` -- or whatever's holding it -- goes out of scope.
pub(crate) async fn send_request_streaming(
    stream: TcpStream,
    method: Method,
    request_target: &str,
    host_header: &str,
    headers: &HeaderMap,
    body: &Body,
) -> Result<StreamingRawResponse> {
    write_request(&stream, method, request_target, host_header, headers, body).await?;
    let (status, reason, resp_headers, reader) = read_head(stream).await?;
    let framing = framing_for(&resp_headers, method, status)?;
    let done = matches!(framing, Framing::None);

    Ok(StreamingRawResponse {
        status,
        reason,
        headers: resp_headers,
        body: StreamingBody {
            reader,
            framing,
            chunked_state: ChunkedState::Size,
            done,
        },
    })
}

async fn write_request(
    stream: &TcpStream,
    method: Method,
    request_target: &str,
    host_header: &str,
    headers: &HeaderMap,
    body: &Body,
) -> Result<()> {
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

    match body {
        Body::Empty => {}
        Body::Bytes(b) => {
            if !b.is_empty() {
                stream.write_all(b).await?;
            }
        }
        Body::Stream(s) => write_stream_body(stream, s).await?,
    }
    Ok(())
}

/// Relays a streaming request body onto the wire: raw passthrough when
/// its length was declared upfront (`Content-Length` already covers
/// framing), or `Transfer-Encoding: chunked` framing when it wasn't.
async fn write_stream_body(stream: &TcpStream, body: &crate::body::StreamBody) -> Result<()> {
    let mut reader = body.open();
    let known_length = body.len().is_some();
    let mut buf = [0u8; CHUNK_SIZE];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if known_length {
            stream.write_all(&buf[..n]).await?;
        } else {
            stream.write_all(format!("{n:x}\r\n").as_bytes()).await?;
            stream.write_all(&buf[..n]).await?;
            stream.write_all(b"\r\n").await?;
        }
    }
    if !known_length {
        stream.write_all(b"0\r\n\r\n").await?;
    }
    Ok(())
}

/// Reads the status line and headers, returning them alongside the
/// buffered reader (still positioned right after the blank line that
/// ends the headers, owning whatever body bytes it's already buffered)
/// so a caller can continue reading the body from the same place --
/// either eagerly ([`drain_body`]) or incrementally ([`StreamingBody`]).
async fn read_head(stream: TcpStream) -> Result<(StatusCode, String, HeaderMap, BufReader)> {
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

    Ok((status, reason, resp_headers, reader))
}

/// A `Connection` header can list multiple tokens (`Connection:
/// keep-alive, Upgrade`); `close` anywhere in the list means the peer is
/// closing the connection after this response.
fn connection_says_close(headers: &HeaderMap) -> bool {
    headers
        .get("connection")
        .map(|v| {
            v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("close"))
        })
        .unwrap_or(false)
}

/// How a response body's end is determined, per RFC 7230 §3.3.3 --
/// shared by both the eager ([`drain_body`]) and incremental
/// ([`StreamingBody::next_chunk`]) readers.
#[derive(Debug, Clone, Copy)]
enum Framing {
    /// HEAD, 204, 304, or 1xx: never carries a body regardless of what
    /// the headers claim.
    None,
    ContentLength(usize),
    Chunked,
    /// No framing header at all: read to EOF. Legal for an
    /// HTTP/1.0-style response, but the stream is dead afterward either
    /// way -- reaching EOF means the peer already closed it.
    Close,
}

fn framing_for(headers: &HeaderMap, method: Method, status: StatusCode) -> Result<Framing> {
    if method == Method::Head
        || status.as_u16() == 204
        || status.as_u16() == 304
        || (100..200).contains(&status.as_u16())
    {
        return Ok(Framing::None);
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
        return Ok(Framing::Chunked);
    }

    if let Some(len) = headers.get("content-length") {
        let len: usize = len
            .trim()
            .parse()
            .map_err(|_| Error::InvalidResponse(format!("invalid Content-Length `{len}`")))?;
        return Ok(Framing::ContentLength(len));
    }

    Ok(Framing::Close)
}

async fn drain_body(reader: &mut BufReader, framing: Framing) -> Result<(Vec<u8>, bool)> {
    match framing {
        Framing::None => Ok((Vec::new(), true)),
        Framing::Chunked => Ok((read_chunked_body(reader).await?, true)),
        Framing::ContentLength(len) => Ok((reader.read_exact_n(len).await?, true)),
        Framing::Close => Ok((reader.read_to_end().await?, false)),
    }
}

async fn read_chunked_body(reader: &mut BufReader) -> Result<Vec<u8>> {
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

/// Which step of chunk framing [`StreamingBody::next_chunk`] is
/// currently in -- only meaningful while `framing` is `Framing::Chunked`.
#[derive(Debug, Clone, Copy)]
enum ChunkedState {
    /// Expecting a chunk-size line next.
    Size,
    /// `usize` bytes of the current chunk's data remain to be read.
    Data(usize),
    /// Just finished a chunk's data; expecting its trailing CRLF.
    DataTerminator,
    /// The zero-size chunk was seen; reading 0+ trailer header lines
    /// then the final blank line.
    Trailers,
}

/// A response body not yet (fully) read, pulled incrementally via
/// [`StreamingBody::next_chunk`]. Wraps the same [`BufReader`] state
/// [`drain_body`] uses, so it picks up exactly where header parsing left
/// off -- including any body bytes that already arrived in the same
/// packet as the headers.
pub(crate) struct StreamingBody {
    reader: BufReader,
    framing: Framing,
    chunked_state: ChunkedState,
    done: bool,
}

impl StreamingBody {
    /// The next chunk of body data, or `None` once the body is fully
    /// consumed. Chunk boundaries are an implementation detail (a
    /// `Framing::Chunked` chunk boundary on the wire, or just "however
    /// much one read/one already-buffered slice returned" otherwise) --
    /// never rely on chunk size or count.
    pub(crate) async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.framing {
                Framing::None => {
                    self.done = true;
                    return Ok(None);
                }
                Framing::Close => {
                    let data = self.reader.read_some(CHUNK_SIZE).await?;
                    if data.is_empty() {
                        self.done = true;
                        return Ok(None);
                    }
                    return Ok(Some(data));
                }
                Framing::ContentLength(remaining) => {
                    if remaining == 0 {
                        self.done = true;
                        return Ok(None);
                    }
                    let data = self.reader.read_some(remaining.min(CHUNK_SIZE)).await?;
                    if data.is_empty() {
                        return Err(Error::InvalidResponse(
                            "connection closed before the full body arrived".into(),
                        ));
                    }
                    self.framing = Framing::ContentLength(remaining - data.len());
                    return Ok(Some(data));
                }
                Framing::Chunked => match self.chunked_state {
                    ChunkedState::Size => {
                        let line = self.reader.read_line().await?.ok_or_else(|| {
                            Error::InvalidResponse("connection closed reading chunk size".into())
                        })?;
                        let size_str = line.split(';').next().unwrap_or("").trim();
                        let size = usize::from_str_radix(size_str, 16).map_err(|_| {
                            Error::InvalidResponse(format!("invalid chunk size `{size_str}`"))
                        })?;
                        self.chunked_state = if size == 0 {
                            ChunkedState::Trailers
                        } else {
                            ChunkedState::Data(size)
                        };
                    }
                    ChunkedState::Data(remaining) => {
                        let data = self.reader.read_some(remaining.min(CHUNK_SIZE)).await?;
                        if data.is_empty() {
                            return Err(Error::InvalidResponse(
                                "connection closed reading chunk data".into(),
                            ));
                        }
                        let left = remaining - data.len();
                        self.chunked_state = if left == 0 {
                            ChunkedState::DataTerminator
                        } else {
                            ChunkedState::Data(left)
                        };
                        return Ok(Some(data));
                    }
                    ChunkedState::DataTerminator => {
                        let line = self.reader.read_line().await?.ok_or_else(|| {
                            Error::InvalidResponse("connection closed after chunk data".into())
                        })?;
                        if !line.is_empty() {
                            return Err(Error::InvalidResponse(
                                "malformed chunk terminator".to_string(),
                            ));
                        }
                        self.chunked_state = ChunkedState::Size;
                    }
                    ChunkedState::Trailers => {
                        let line = self.reader.read_line().await?.ok_or_else(|| {
                            Error::InvalidResponse("connection closed reading chunk trailer".into())
                        })?;
                        if line.is_empty() {
                            self.framing = Framing::None;
                        }
                    }
                },
            }
        }
    }
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

/// A small buffered reader that owns its [`TcpStream`]. Needed because
/// header lines have to be read byte-by-byte-ish (scanning for `\n`)
/// while the socket itself has no internal buffering -- and because
/// scanning ahead for a line terminator can read past it into body
/// bytes that already arrived in the same packet, which then need to be
/// handed back to whichever body-reading routine runs next rather than
/// re-read from the socket.
///
/// Owning (rather than borrowing) the stream is what lets a
/// [`StreamingBody`] keep reading from the same connection across
/// multiple [`StreamingBody::next_chunk`] calls that happen after this
/// module's functions have already returned -- the eager path
/// ([`send_request`]) needs the stream back afterward too (to hand to
/// the caller for pooling), via [`BufReader::into_stream`].
///
/// This is only sound because this crate never pipelines: a caller
/// always awaits a full response before sending its next request on the
/// same stream, so a server never has a reason to send more than one
/// response's worth of bytes before that next request arrives. If that
/// ever changes, the "buffered leftover bytes carry forward inside this
/// same reader across a response" design here (and the streaming path's
/// "never pooled" simplification) would need revisiting.
struct BufReader {
    stream: TcpStream,
    buf: Vec<u8>,
    /// `buf[start..end]` is the unread, already-received data.
    start: usize,
    end: usize,
}

impl BufReader {
    fn new(stream: TcpStream) -> Self {
        BufReader {
            stream,
            buf: vec![0u8; 8192],
            start: 0,
            end: 0,
        }
    }

    fn into_stream(self) -> TcpStream {
        self.stream
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

    /// Returns up to `max` bytes, doing at most one socket read (none at
    /// all if already-buffered data can satisfy it) -- unlike
    /// [`BufReader::read_exact_n`], this deliberately doesn't loop to
    /// fill up to `max`, so a [`StreamingBody`] caller sees data as soon
    /// as it's available rather than however long it takes to
    /// accumulate a full `max`-sized batch. Empty return means EOF.
    async fn read_some(&mut self, max: usize) -> Result<Vec<u8>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        if self.start == self.end && !self.fill().await? {
            return Ok(Vec::new());
        }
        let take = max.min(self.end - self.start);
        let data = self.buf[self.start..self.start + take].to_vec();
        self.start += take;
        Ok(data)
    }
}
