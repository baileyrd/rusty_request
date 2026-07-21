//! A minimal `http(s)://host[:port]/path?query` URL parser -- just
//! enough to build an HTTP/1.1 request line and `Host` header. Not a
//! general-purpose URI implementation (no userinfo, no IPv6 zone ids,
//! no RFC 3986 edge cases): those are out of scope for this MVP and can
//! be added if a real caller needs them.

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Always starts with `/`.
    pub path: String,
    /// Raw, already-encoded query string, without the leading `?`.
    pub query: Option<String>,
}

impl Url {
    pub fn parse(input: &str) -> Result<Url> {
        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| Error::InvalidUrl(input.to_string()))?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            other => {
                return Err(Error::InvalidUrl(format!(
                    "unrecognized scheme `{other}` in `{input}`"
                )))
            }
        };

        // Fragments never reach the wire; drop them before anything else.
        let rest = rest.split('#').next().unwrap_or("");

        if rest.contains('@') {
            return Err(Error::InvalidUrl(format!(
                "userinfo (user:pass@host) is not supported in `{input}`"
            )));
        }

        let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if authority.is_empty() {
            return Err(Error::InvalidUrl(format!("missing host in `{input}`")));
        }
        let path_and_query = &rest[authority_end..];

        let (host, port) = parse_authority(authority, default_port, input)?;

        let (path, query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q.to_string())),
            None => (path_and_query, None),
        };
        let path = if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        };

        Ok(Url {
            scheme,
            host,
            port,
            path,
            query,
        })
    }

    /// The `Host` header value: bare host, plus `:port` only when it
    /// differs from the scheme's default (matching how browsers/
    /// `requests` build this header).
    pub fn host_header(&self) -> String {
        let default_port = if self.scheme == "https" { 443 } else { 80 };
        if self.port == default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The request-target for the HTTP/1.1 request line: `path?query`.
    pub fn request_target(&self) -> String {
        match &self.query {
            Some(q) if !q.is_empty() => format!("{}?{}", self.path, q),
            _ => self.path.clone(),
        }
    }

    /// Returns a copy with `pairs` appended to the query string,
    /// percent-encoding each key/value.
    pub fn with_query_pairs<I, K, V>(&self, pairs: I) -> Url
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut query = self.query.clone().unwrap_or_default();
        for (k, v) in pairs {
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(&percent_encode(k.as_ref()));
            query.push('=');
            query.push_str(&percent_encode(v.as_ref()));
        }
        Url {
            query: if query.is_empty() { None } else { Some(query) },
            ..self.clone()
        }
    }
}

fn parse_authority(authority: &str, default_port: u16, original: &str) -> Result<(String, u16)> {
    match authority.rsplit_once(':') {
        Some((host, port_str)) if !host.is_empty() => {
            let port: u16 = port_str
                .parse()
                .map_err(|_| Error::InvalidUrl(format!("bad port in `{original}`")))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// Percent-encodes everything except RFC 3986 "unreserved" characters
/// (`A-Za-z0-9-_.~`) -- enough for building query-string keys/values.
/// Not a full RFC 3986 implementation (doesn't distinguish "reserved but
/// safe in this component" characters like `!*'()`), which is fine for
/// query params built from arbitrary caller strings.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_host() {
        let u = Url::parse("http://example.com").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 80);
        assert_eq!(u.path, "/");
        assert_eq!(u.query, None);
    }

    #[test]
    fn parses_host_port_path_query() {
        let u = Url::parse("http://example.com:8080/a/b?x=1&y=2").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query.as_deref(), Some("x=1&y=2"));
    }

    #[test]
    fn strips_fragment() {
        let u = Url::parse("http://example.com/a#section").unwrap();
        assert_eq!(u.path, "/a");
    }

    #[test]
    fn host_header_omits_default_port() {
        let u = Url::parse("http://example.com:80/").unwrap();
        assert_eq!(u.host_header(), "example.com");
        let u = Url::parse("http://example.com:8080/").unwrap();
        assert_eq!(u.host_header(), "example.com:8080");
    }

    #[test]
    fn rejects_missing_scheme_separator() {
        assert!(Url::parse("example.com/a").is_err());
    }

    #[test]
    fn rejects_unsupported_scheme() {
        assert!(Url::parse("ftp://example.com").is_err());
    }

    #[test]
    fn query_pairs_are_percent_encoded_and_appended() {
        let u = Url::parse("http://example.com/search?existing=1").unwrap();
        let u = u.with_query_pairs([("q", "hello world"), ("n", "1")]);
        assert_eq!(u.query.as_deref(), Some("existing=1&q=hello%20world&n=1"));
    }
}
