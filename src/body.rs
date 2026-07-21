use rusty_tokio::io::{AsyncRead, ReadBuf};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// A request body: either fully buffered in memory (`Empty`/`Bytes`, as
/// before), or a [`StreamBody`] that produces its bytes incrementally --
/// see [`Body::streaming`].
#[derive(Clone, Default)]
pub enum Body {
    #[default]
    Empty,
    Bytes(Vec<u8>),
    Stream(StreamBody),
}

impl Body {
    /// A streaming body backed by `open`, an `AsyncRead` factory called
    /// fresh every time the body actually needs to go on the wire --
    /// the first attempt, and again for any redirect hop (307/308) that
    /// preserves the body. A single already-open reader can't be
    /// rewound or duplicated for a second hop, so this takes a way to
    /// *produce* one (e.g. reopening a file) instead.
    ///
    /// `len`, if known, is sent as `Content-Length` and the bytes are
    /// written as-is; `None` sends `Transfer-Encoding: chunked` instead,
    /// for a source whose total size isn't known upfront.
    pub fn streaming<F, R>(len: Option<u64>, open: F) -> Body
    where
        F: Fn() -> R + Send + Sync + 'static,
        R: AsyncRead + Send + Unpin + 'static,
    {
        Body::Stream(StreamBody {
            open_fn: Arc::new(move || StreamSource(Box::new(open()))),
            len,
        })
    }

    /// The body's bytes, if it's fully buffered (`Empty`/`Bytes`) --
    /// `&[]` for `Stream`, which has no single in-memory representation.
    /// Prefer [`Body::content_length`] or matching on the variant
    /// directly when a `Stream` body needs to be handled too.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Body::Empty => &[],
            Body::Bytes(b) => b,
            Body::Stream(_) => &[],
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Body::Empty => true,
            Body::Bytes(b) => b.is_empty(),
            Body::Stream(s) => s.len == Some(0),
        }
    }

    /// The body's length in bytes if known upfront (`Empty`/`Bytes`
    /// always know it; `Stream` only if constructed with a `Some(len)`).
    /// `None` means the wire framing must fall back to `Transfer-Encoding:
    /// chunked` instead of `Content-Length`.
    pub(crate) fn content_length(&self) -> Option<usize> {
        match self {
            Body::Empty => Some(0),
            Body::Bytes(b) => Some(b.len()),
            Body::Stream(s) => s.len.map(|n| n as usize),
        }
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Body::Empty => f.write_str("Empty"),
            Body::Bytes(b) => f.debug_tuple("Bytes").field(b).finish(),
            Body::Stream(s) => f.debug_struct("Stream").field("len", &s.len).finish(),
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(b: Vec<u8>) -> Self {
        if b.is_empty() {
            Body::Empty
        } else {
            Body::Bytes(b)
        }
    }
}

impl From<&[u8]> for Body {
    fn from(b: &[u8]) -> Self {
        Body::from(b.to_vec())
    }
}

impl From<String> for Body {
    fn from(s: String) -> Self {
        Body::from(s.into_bytes())
    }
}

impl From<&str> for Body {
    fn from(s: &str) -> Self {
        Body::from(s.as_bytes())
    }
}

/// A streaming [`Body`]'s payload: a reusable factory (`Arc` so cloning
/// a `Body` -- needed for retries and redirect-body-preservation --
/// stays cheap) plus the declared length, if any.
#[derive(Clone)]
pub struct StreamBody {
    open_fn: Arc<dyn Fn() -> StreamSource + Send + Sync>,
    len: Option<u64>,
}

impl StreamBody {
    pub(crate) fn open(&self) -> StreamSource {
        (self.open_fn)()
    }

    pub(crate) fn len(&self) -> Option<u64> {
        self.len
    }
}

/// Delegates to a boxed `dyn AsyncRead` by implementing the trait
/// itself. `rusty_tokio` has no blanket `impl AsyncRead for Box<dyn
/// AsyncRead>`, and this crate can't add one -- neither the trait nor
/// `Box`/`Pin` are local types, so the orphan rule blocks it -- but this
/// newtype *is* local, so delegating through it sidesteps that.
pub(crate) struct StreamSource(Box<dyn AsyncRead + Send + Unpin>);

impl AsyncRead for StreamSource {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_read(cx, buf)
    }
}
