//! A simple per-[`crate::Client`] idle-connection pool: hands back a
//! `rusty_tokio` [`TcpStream`] for reuse on a subsequent request to the
//! same (scheme, host, port) origin, when the previous response left it
//! in a reusable state (see `http1::RawResponse::keep_alive`).
//!
//! No pipelining -- a pooled connection is only ever handed to one
//! in-flight request at a time, exactly matching `http1::read_body`'s
//! own "never more than one response in flight" assumption.

use rusty_tokio::io::TcpStream;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) type PoolKey = (String, String, u16);

struct Idle {
    stream: TcpStream,
    since: Instant,
}

pub(crate) struct ConnectionPool {
    idle: Mutex<HashMap<PoolKey, Vec<Idle>>>,
    max_idle_per_origin: usize,
    idle_timeout: Duration,
}

impl ConnectionPool {
    pub(crate) fn new(max_idle_per_origin: usize, idle_timeout: Duration) -> Self {
        ConnectionPool {
            idle: Mutex::new(HashMap::new()),
            max_idle_per_origin,
            idle_timeout,
        }
    }

    /// Takes the most recently idled connection for `key`, if one
    /// exists and hasn't sat idle past the configured timeout. LIFO,
    /// not FIFO: the most recently returned connection is the one least
    /// likely to have been closed by the peer's own idle timeout in the
    /// meantime.
    pub(crate) fn take(&self, key: &PoolKey) -> Option<TcpStream> {
        let mut idle = self.idle.lock().unwrap();
        let entries = idle.get_mut(key)?;
        let now = Instant::now();
        while let Some(entry) = entries.pop() {
            if now.duration_since(entry.since) < self.idle_timeout {
                return Some(entry.stream);
            }
            // Past the idle timeout -- drop it (closing the socket) and
            // keep looking at the next-most-recently-idled entry.
        }
        None
    }

    /// Returns a connection to the pool for reuse, unless the
    /// per-origin idle list is already at capacity or pooling is
    /// disabled (`max_idle_per_origin == 0`) -- either way, `stream` is
    /// simply dropped (closing it), bounding how many idle connections
    /// a `Client` can accumulate.
    pub(crate) fn put(&self, key: PoolKey, stream: TcpStream) {
        if self.max_idle_per_origin == 0 {
            return;
        }
        let mut idle = self.idle.lock().unwrap();
        let entries = idle.entry(key).or_default();
        if entries.len() < self.max_idle_per_origin {
            entries.push(Idle {
                stream,
                since: Instant::now(),
            });
        }
    }
}

/// Manual impl -- `rusty_tokio::io::TcpStream` doesn't implement
/// `Debug`, so `#[derive(Debug)]` isn't an option here.
impl std::fmt::Debug for ConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionPool")
            .field("max_idle_per_origin", &self.max_idle_per_origin)
            .field("idle_timeout", &self.idle_timeout)
            .finish_non_exhaustive()
    }
}
