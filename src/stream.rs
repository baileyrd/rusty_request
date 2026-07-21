//! The connection type a request is actually sent over: a plain TCP
//! socket for `http://`, or the same socket wrapped in TLS (via
//! `rusty_tls`) for `https://`. An enum rather than making the send path
//! generic over the transport -- nothing else in this crate is generic,
//! and threading a type parameter through `pool`/`http1`/`client` for a
//! two-variant choice would be more machinery than the choice itself.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use rusty_tls::AsyncTlsStream;
use rusty_tokio::io::{AsyncRead, AsyncWrite, ReadBuf, TcpStream};

pub(crate) enum Conn {
    Plain(TcpStream),
    /// Boxed so the (larger) TLS variant doesn't grow every `Conn` --
    /// most requests in a mixed http/https workload are still plain TCP.
    Tls(Box<AsyncTlsStream<TcpStream>>),
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}
