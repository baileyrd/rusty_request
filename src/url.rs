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

    /// Resolves a `Location` header value against `self` (the request
    /// URL that produced it), per the RFC 3986 §5.3 "Component
    /// Recomposition" algorithm -- simplified, since `Location` is
    /// never a fragment-only or same-document reference in practice.
    /// Handles absolute URLs (`http://...`), protocol-relative
    /// (`//host/path`), absolute-path (`/path`), and relative-path
    /// references (`path`, `../path`, `?query`), the last resolved via
    /// RFC 3986 §5.2's merge + dot-segment removal.
    pub(crate) fn resolve_redirect(&self, location: &str) -> Result<Url> {
        let location = location.trim();
        if location.is_empty() {
            return Err(Error::InvalidResponse(
                "redirect response had an empty Location header".to_string(),
            ));
        }

        if let Some(idx) = location.find("://") {
            let scheme_candidate = &location[..idx];
            let looks_like_scheme = !scheme_candidate.is_empty()
                && scheme_candidate
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
            if looks_like_scheme {
                return Url::parse(location);
            }
        }

        if let Some(rest) = location.strip_prefix("//") {
            return Url::parse(&format!("{}://{}", self.scheme, rest));
        }

        if let Some(path_and_query) = location.strip_prefix('/') {
            let (path, query) = split_path_query(&format!("/{path_and_query}"));
            return Ok(Url {
                scheme: self.scheme.clone(),
                host: self.host.clone(),
                port: self.port,
                path,
                query,
            });
        }

        let (rel_path, rel_query) = split_path_query(location);
        let merged = merge_paths(&self.path, &rel_path);
        Ok(Url {
            scheme: self.scheme.clone(),
            host: self.host.clone(),
            port: self.port,
            path: remove_dot_segments(&merged),
            query: rel_query,
        })
    }
}

fn split_path_query(s: &str) -> (String, Option<String>) {
    match s.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (s.to_string(), None),
    }
}

/// RFC 3986 §5.3's merge step: replaces everything in `base_path` from
/// the last `/` onward with `rel_path`. An empty `rel_path` (a
/// query-only or same-path reference, e.g. `Location: ?x=1`) leaves
/// `base_path` untouched.
fn merge_paths(base_path: &str, rel_path: &str) -> String {
    if rel_path.is_empty() {
        return base_path.to_string();
    }
    match base_path.rfind('/') {
        Some(idx) => format!("{}{}", &base_path[..=idx], rel_path),
        None => format!("/{rel_path}"),
    }
}

/// RFC 3986 §5.2.4, simplified: resolves `.`/`..` segments against a
/// path that's already known to start with `/` (every `Url::path` and
/// every merge-step output does). `..` past the root is ignored rather
/// than erroring, matching how browsers handle it.
fn remove_dot_segments(path: &str) -> String {
    let mut output: Vec<&str> = vec![""];
    let trailing_slash = path.ends_with('/');
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if output.len() > 1 {
                    output.pop();
                }
            }
            other => output.push(other),
        }
    }
    let mut result = output.join("/");
    if result.is_empty() {
        result = "/".to_string();
    }
    if trailing_slash && !result.ends_with('/') {
        result.push('/');
    }
    result
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

    #[test]
    fn resolves_absolute_location() {
        let base = Url::parse("http://example.com/a/b").unwrap();
        let resolved = base.resolve_redirect("http://other.com/c").unwrap();
        assert_eq!(resolved.host, "other.com");
        assert_eq!(resolved.path, "/c");
    }

    #[test]
    fn resolves_protocol_relative_location() {
        let base = Url::parse("http://example.com/a").unwrap();
        let resolved = base.resolve_redirect("//other.com/c").unwrap();
        assert_eq!(resolved.scheme, "http");
        assert_eq!(resolved.host, "other.com");
        assert_eq!(resolved.path, "/c");
    }

    #[test]
    fn resolves_absolute_path_location() {
        let base = Url::parse("http://example.com:8080/a/b?old=1").unwrap();
        let resolved = base.resolve_redirect("/c/d").unwrap();
        assert_eq!(resolved.host, "example.com");
        assert_eq!(resolved.port, 8080);
        assert_eq!(resolved.path, "/c/d");
        assert_eq!(resolved.query, None);
    }

    #[test]
    fn resolves_relative_path_location_against_directory() {
        let base = Url::parse("http://example.com/a/b/c").unwrap();
        let resolved = base.resolve_redirect("d").unwrap();
        assert_eq!(resolved.path, "/a/b/d");
    }

    #[test]
    fn resolves_relative_path_with_dot_segments() {
        let base = Url::parse("http://example.com/a/b/c").unwrap();
        let resolved = base.resolve_redirect("../x").unwrap();
        assert_eq!(resolved.path, "/a/x");
    }

    #[test]
    fn resolves_query_only_location_keeping_path() {
        let base = Url::parse("http://example.com/a/b?old=1").unwrap();
        let resolved = base.resolve_redirect("?new=2").unwrap();
        assert_eq!(resolved.path, "/a/b");
        assert_eq!(resolved.query.as_deref(), Some("new=2"));
    }

    #[test]
    fn rejects_empty_location() {
        let base = Url::parse("http://example.com/a").unwrap();
        assert!(base.resolve_redirect("").is_err());
    }
}
