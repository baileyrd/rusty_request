//! [`StreamingResponse`], returned by
//! [`crate::RequestBuilder::send_streaming`] -- a response whose body
//! hasn't been read yet, pulled incrementally instead of requiring it
//! all in memory at once.

use crate::error::Result;
use crate::stream::Conn;
use rusty_http::async_tokio::BodyReader;
use rusty_http::{HeaderMap, StatusCode, Url};

/// A response whose body hasn't been read yet. Unlike [`crate::Response`]
/// (always fully buffered), the body here is pulled incrementally via
/// [`StreamingResponse::chunk`] -- useful for a large download that
/// shouldn't have to sit fully in memory before the caller can start
/// processing it.
pub struct StreamingResponse {
    status: StatusCode,
    headers: HeaderMap,
    url: Url,
    body: BodyReader<Conn>,
}

impl StreamingResponse {
    pub(crate) fn new(
        status: StatusCode,
        headers: HeaderMap,
        url: Url,
        body: BodyReader<Conn>,
    ) -> Self {
        StreamingResponse {
            status,
            headers,
            url,
            body,
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The next chunk of response body data, or `None` once the body is
    /// fully consumed. Chunk boundaries are an implementation detail --
    /// not aligned to `Transfer-Encoding: chunked` framing on the wire
    /// even when the response used it -- so don't rely on chunk size or
    /// count, only on the concatenation of every chunk returned before
    /// `None`.
    ///
    /// Dropping a `StreamingResponse` before this returns `None` simply
    /// closes the underlying connection (it's never pooled either way --
    /// see [`crate::RequestBuilder::send_streaming`]'s docs) rather than
    /// leaving anything to clean up.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.body.next_chunk().await?)
    }
}
