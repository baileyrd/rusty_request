//! Forward-proxy support: [`Proxy`] (an explicit `http://host[:port]`
//! proxy every request can be routed through) and [`NoProxyRules`]
//! (`NO_PROXY`-style bypass rules for specific hosts).
//!
//! An `http://` request is forwarded in cleartext (absolute-form
//! request-target, RFC 7230 §5.3.2, `Host` still naming the origin) --
//! the proxy can read and rewrite it. An `https://` request instead asks
//! the proxy to open an opaque `CONNECT` tunnel to the origin (see
//! `crate::client::establish`/`connect_tunnel`), since the proxy can't
//! (and shouldn't be able to) read encrypted traffic; the TLS handshake
//! and the real request both happen *through* that tunnel, invisible to
//! the proxy. Either way the proxy itself is reached over plain
//! `http://` -- [`Proxy::http`] rejects an `https://` proxy URL, since
//! this crate has no way to open the first (unencrypted) hop to such a
//! proxy in a way that would let it understand a `CONNECT` request.

use crate::error::{Error, Result};
use crate::pool::PoolKey;
use rusty_http::Url;

/// An HTTP forward proxy: requests go to `host:port` instead of the
/// request's own origin. An `http://` request uses the origin's full
/// URL as the absolute-form request-target (RFC 7230 §5.3.2) so the
/// proxy knows where to forward it, with `Host` still naming the origin;
/// an `https://` request instead tunnels through the proxy via `CONNECT`
/// (see the module docs).
#[derive(Debug, Clone)]
pub struct Proxy {
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Proxy {
    /// Parses `proxy_url` (`http://host[:port]`) as the proxy to route
    /// requests through. Rejects an `https://` proxy URL: reaching the
    /// proxy itself always happens over plain `http://`, whether the
    /// request being routed through it is `http://` or `https://` (see
    /// the module docs) -- a `CONNECT` tunnel changes how traffic to the
    /// *origin* is protected, not how the client talks to the proxy.
    pub fn http(proxy_url: &str) -> Result<Proxy> {
        let url = Url::parse(proxy_url)?;
        if url.scheme != "http" {
            return Err(Error::UnsupportedScheme(url.scheme));
        }
        Ok(Proxy {
            host: url.host,
            port: url.port,
        })
    }

    /// A distinct pool key namespace (`"proxy"` isn't a real URL scheme)
    /// so a pooled connection *to* a proxy, used for plain `http://`
    /// forwarding, can never collide with a pooled direct connection
    /// that coincidentally targets the same host:port.
    pub(crate) fn pool_key(&self) -> PoolKey {
        (
            "proxy".to_string(),
            self.host.to_ascii_lowercase(),
            self.port,
        )
    }

    /// The pool key for a `CONNECT`-tunneled `https://` connection
    /// through this proxy to `url`'s origin. Distinct from both
    /// [`Proxy::pool_key`] (this proxy's shared plain-forwarding key)
    /// and a direct connection's own key: once a tunnel is up it's a
    /// private, origin-specific channel -- physically routed via the
    /// proxy, but not shareable across origins the way plain forwarding
    /// is -- so reusing a pooled tunneled connection for a *different*
    /// origin than the one it was tunneled to would silently send that
    /// request to the wrong place.
    pub(crate) fn tunnel_pool_key(&self, url: &Url) -> PoolKey {
        (
            "connect-tunnel".to_string(),
            format!(
                "{}:{}->{}",
                self.host.to_ascii_lowercase(),
                self.port,
                url.host.to_ascii_lowercase()
            ),
            url.port,
        )
    }
}

/// `NO_PROXY`-style bypass rules: a request to a matching host skips
/// the configured [`Proxy`] and connects directly. Each entry matches
/// either exactly or as a dot-boundary suffix (`example.com` also
/// matches `www.example.com`, the same convention `crate::cookie`'s
/// `Domain` matching already uses) -- except the single entry `*`,
/// which bypasses the proxy for every host.
#[derive(Debug, Clone, Default)]
pub(crate) struct NoProxyRules {
    entries: Vec<String>,
}

impl NoProxyRules {
    pub(crate) fn from_hosts<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        NoProxyRules {
            entries: hosts
                .into_iter()
                .map(|h| {
                    h.as_ref()
                        .trim()
                        .trim_start_matches('.')
                        .to_ascii_lowercase()
                })
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }

    /// Parses a comma-separated `NO_PROXY`-style spec (whitespace around
    /// entries is trimmed).
    pub(crate) fn parse(spec: &str) -> Self {
        Self::from_hosts(spec.split(','))
    }

    pub(crate) fn bypasses(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.entries
            .iter()
            .any(|e| e == "*" || host == *e || host.ends_with(&format!(".{e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_proxy_parses_host_and_port() {
        let p = Proxy::http("http://proxy.example:8080").unwrap();
        assert_eq!(p.host, "proxy.example");
        assert_eq!(p.port, 8080);
    }

    #[test]
    fn http_proxy_rejects_https_scheme() {
        assert!(matches!(
            Proxy::http("https://proxy.example"),
            Err(Error::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn http_proxy_rejects_invalid_url() {
        assert!(Proxy::http("not a url").is_err());
    }

    #[test]
    fn distinct_proxies_get_distinct_pool_keys() {
        let a = Proxy::http("http://a.example:8080").unwrap();
        let b = Proxy::http("http://b.example:8080").unwrap();
        assert_ne!(a.pool_key(), b.pool_key());
    }

    #[test]
    fn tunnel_pool_key_differs_from_forwarding_key_and_by_origin() {
        let proxy = Proxy::http("http://proxy.example:8080").unwrap();
        let a = Url::parse("https://a.example/").unwrap();
        let b = Url::parse("https://b.example/").unwrap();

        // A tunnel to origin A must not collide with a tunnel to origin
        // B through the same proxy...
        assert_ne!(proxy.tunnel_pool_key(&a), proxy.tunnel_pool_key(&b));
        // ...nor with this proxy's own plain-forwarding key.
        assert_ne!(proxy.tunnel_pool_key(&a), proxy.pool_key());
    }

    #[test]
    fn no_proxy_rules_match_exact_host() {
        let rules = NoProxyRules::parse("example.com");
        assert!(rules.bypasses("example.com"));
        assert!(rules.bypasses("EXAMPLE.COM"));
        assert!(!rules.bypasses("other.com"));
    }

    #[test]
    fn no_proxy_rules_match_subdomains() {
        let rules = NoProxyRules::parse("example.com");
        assert!(rules.bypasses("www.example.com"));
        assert!(!rules.bypasses("notexample.com"));
    }

    #[test]
    fn no_proxy_rules_handle_leading_dot_and_whitespace() {
        let rules = NoProxyRules::parse(" .example.com , other.com ");
        assert!(rules.bypasses("example.com"));
        assert!(rules.bypasses("sub.other.com"));
    }

    #[test]
    fn no_proxy_star_bypasses_everything() {
        let rules = NoProxyRules::parse("*");
        assert!(rules.bypasses("anything.at.all"));
    }

    #[test]
    fn empty_no_proxy_rules_bypass_nothing() {
        let rules = NoProxyRules::default();
        assert!(!rules.bypasses("example.com"));
    }
}
