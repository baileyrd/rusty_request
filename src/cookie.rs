//! A minimal RFC 6265 cookie jar: parses `Set-Cookie` response headers,
//! stores them scoped by domain/path, and builds the `Cookie` request
//! header for subsequent requests through the same [`crate::Client`].
//!
//! No public-suffix-list support -- a real "supercookie" defense (a
//! server setting `Domain=com` and poisoning every `.com` site) needs
//! one, and hand-rolling/vendoring the Public Suffix List is well
//! beyond this crate's scope. The one safety check implemented is RFC
//! 6265 §5.3's own, narrower rule: a response may only set a `Domain`
//! that is a suffix of the host that actually sent it.

use crate::url::Url;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
struct Cookie {
    name: String,
    value: String,
    /// Lowercase, no leading dot.
    domain: String,
    /// `true` if the response set no `Domain` attribute -- matches only
    /// the exact host that set it, not subdomains.
    host_only: bool,
    path: String,
    secure: bool,
    /// Parsed for RFC completeness (the issue asks for it explicitly)
    /// but nothing here reads it -- this crate has no JS-cookie-access
    /// surface for `HttpOnly` to gate.
    #[allow(dead_code)]
    http_only: bool,
    /// `None` = session cookie (kept for the process's lifetime).
    expires: Option<SystemTime>,
}

impl Cookie {
    /// Parses one `Set-Cookie` header value in the context of the
    /// request URL that returned it. Returns `None` for a malformed or
    /// (per RFC 6265 §5.3) disallowed cookie.
    fn parse(raw: &str, request_url: &Url) -> Option<Cookie> {
        let mut parts = raw.split(';');
        let (name, value) = parts.next()?.split_once('=')?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            return None;
        }

        let mut domain_attr: Option<String> = None;
        let mut path_attr: Option<String> = None;
        let mut max_age: Option<i64> = None;
        let mut expires_attr: Option<SystemTime> = None;
        let mut secure = false;
        let mut http_only = false;

        for attr in parts {
            let attr = attr.trim();
            if attr.is_empty() {
                continue;
            }
            let (key, val) = match attr.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                None => (attr, None),
            };
            match key.to_ascii_lowercase().as_str() {
                "domain" => {
                    domain_attr = val
                        .map(|v| v.trim_start_matches('.').to_ascii_lowercase())
                        .filter(|v| !v.is_empty());
                }
                "path" => {
                    path_attr = val.filter(|v| v.starts_with('/')).map(|v| v.to_string());
                }
                "max-age" => max_age = val.and_then(|v| v.parse().ok()),
                "expires" => expires_attr = val.and_then(parse_http_date),
                "secure" => secure = true,
                "httponly" => http_only = true,
                _ => {} // SameSite, Priority, etc. -- not in this pass's scope.
            }
        }

        let (domain, host_only) = match domain_attr {
            Some(d) => {
                let host = request_url.host.to_ascii_lowercase();
                let is_suffix = host == d || host.ends_with(&format!(".{d}"));
                if !is_suffix {
                    return None;
                }
                (d, false)
            }
            None => (request_url.host.to_ascii_lowercase(), true),
        };

        let path = path_attr.unwrap_or_else(|| default_path(&request_url.path));

        // RFC 6265 §5.3 step 3: Max-Age wins over Expires when both are
        // present; a non-positive Max-Age means "delete immediately".
        let expires = match max_age {
            Some(secs) if secs <= 0 => Some(SystemTime::UNIX_EPOCH),
            Some(secs) => Some(SystemTime::now() + Duration::from_secs(secs as u64)),
            None => expires_attr,
        };

        Some(Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain,
            host_only,
            path,
            secure,
            http_only,
            expires,
        })
    }

    fn is_expired(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|e| e <= now)
    }

    fn domain_matches(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        if self.host_only {
            host == self.domain
        } else {
            host == self.domain || host.ends_with(&format!(".{}", self.domain))
        }
    }

    fn path_matches(&self, path: &str) -> bool {
        if path == self.path {
            return true;
        }
        path.starts_with(&self.path)
            && (self.path.ends_with('/') || path.as_bytes().get(self.path.len()) == Some(&b'/'))
    }
}

/// RFC 6265 §5.1.4 default-path: the request path's directory (up to
/// but not including the last `/`), or `/` when that would be empty.
fn default_path(request_path: &str) -> String {
    match request_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => request_path[..idx].to_string(),
    }
}

#[derive(Debug, Default)]
pub(crate) struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    pub(crate) fn new() -> Self {
        CookieJar::default()
    }

    /// Parses and stores every `Set-Cookie` header value in the
    /// context of `request_url` -- a response may send several.
    pub(crate) fn store_from_response<'a>(
        &mut self,
        request_url: &Url,
        set_cookie_values: impl Iterator<Item = &'a str>,
    ) {
        for raw in set_cookie_values {
            let Some(cookie) = Cookie::parse(raw, request_url) else {
                continue;
            };
            self.cookies.retain(|c| {
                !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
            });
            if !cookie.is_expired(SystemTime::now()) {
                self.cookies.push(cookie);
            }
        }
    }

    /// The `Cookie` header value for a request to `url`, if any cookies
    /// match: domain/path match, not expired, and -- since this crate
    /// is `http://`-only today -- never a `Secure` cookie. Also prunes
    /// expired entries from the jar as a side effect.
    pub(crate) fn cookie_header_for(&mut self, url: &Url) -> Option<String> {
        let now = SystemTime::now();
        self.cookies.retain(|c| !c.is_expired(now));

        let mut matching: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| c.domain_matches(&url.host) && c.path_matches(&url.path))
            .filter(|c| !c.secure || url.scheme == "https")
            .collect();
        if matching.is_empty() {
            return None;
        }
        // RFC 6265 §5.4 step 2: more specific (longer) paths first.
        matching.sort_by_key(|c| std::cmp::Reverse(c.path.len()));

        Some(
            matching
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// Parses an RFC 7231 IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`),
/// the format every real server sends `Expires` in today. Older
/// `Set-Cookie`-specific date formats (RFC 850, asctime) aren't
/// handled -- `Max-Age` is the modern, simpler-to-parse attribute and
/// already takes precedence when both are present, so this is purely a
/// fallback for servers that only send `Expires`.
pub(crate) fn parse_http_date(s: &str) -> Option<SystemTime> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = month_number(parts[2])?;
    let year: i64 = parts[3].parse().ok()?;
    let mut time_parts = parts[4].split(':');
    let hour: u64 = time_parts.next()?.parse().ok()?;
    let minute: u64 = time_parts.next()?.parse().ok()?;
    let second: u64 = time_parts.next()?.parse().ok()?;

    let days = days_from_civil(year, month, day);
    if days < 0 {
        // A date before the epoch (e.g. the classic "delete this
        // cookie" trick using 1969 or earlier) is simply already
        // expired -- clamp rather than under/overflow the cast below.
        return Some(SystemTime::UNIX_EPOCH);
    }
    let total_seconds = (days as u64) * 86_400 + hour * 3600 + minute * 60 + second;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(total_seconds))
}

fn month_number(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(name))
        .map(|i| i as i64 + 1)
}

/// Howard Hinnant's `days_from_civil`: days since the Unix epoch
/// (1970-01-01) for a proleptic-Gregorian calendar date. A well-known,
/// already-verified-correct public-domain algorithm
/// (<http://howardhinnant.github.io/date_algorithms.html>) -- chosen
/// over hand-rolling a cumulative-days-per-month table because it
/// already *is* that table, done right (including century/400-year
/// leap-year rules a naive table easily gets wrong).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn stores_and_returns_a_simple_cookie() {
        let mut jar = CookieJar::new();
        jar.store_from_response(&url("http://example.com/a"), ["session=abc123"].into_iter());
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/a"))
                .as_deref(),
            Some("session=abc123")
        );
    }

    #[test]
    fn host_only_cookie_does_not_match_other_hosts() {
        let mut jar = CookieJar::new();
        jar.store_from_response(&url("http://example.com/"), ["a=1"].into_iter());
        assert_eq!(jar.cookie_header_for(&url("http://other.com/")), None);
        assert_eq!(jar.cookie_header_for(&url("http://sub.example.com/")), None);
    }

    #[test]
    fn domain_cookie_matches_subdomains() {
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url("http://example.com/"),
            ["a=1; Domain=example.com"].into_iter(),
        );
        assert_eq!(
            jar.cookie_header_for(&url("http://sub.example.com/"))
                .as_deref(),
            Some("a=1")
        );
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/"))
                .as_deref(),
            Some("a=1")
        );
    }

    #[test]
    fn rejects_domain_outside_setting_host() {
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url("http://example.com/"),
            ["a=1; Domain=evil.com"].into_iter(),
        );
        assert_eq!(jar.cookie_header_for(&url("http://example.com/")), None);
        assert_eq!(jar.cookie_header_for(&url("http://evil.com/")), None);
    }

    #[test]
    fn path_scoping_restricts_matches() {
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url("http://example.com/admin/login"),
            ["a=1; Path=/admin"].into_iter(),
        );
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/admin/x"))
                .as_deref(),
            Some("a=1")
        );
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/other")),
            None
        );
    }

    #[test]
    fn default_path_is_request_directory() {
        let mut jar = CookieJar::new();
        jar.store_from_response(&url("http://example.com/a/b/c"), ["x=1"].into_iter());
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/a/b/z"))
                .as_deref(),
            Some("x=1")
        );
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/a/other")),
            None
        );
    }

    #[test]
    fn secure_cookie_is_never_sent_over_plain_http() {
        let mut jar = CookieJar::new();
        jar.store_from_response(&url("http://example.com/"), ["a=1; Secure"].into_iter());
        assert_eq!(jar.cookie_header_for(&url("http://example.com/")), None);
    }

    #[test]
    fn max_age_zero_or_negative_deletes_immediately() {
        let mut jar = CookieJar::new();
        jar.store_from_response(&url("http://example.com/"), ["a=1; Max-Age=-1"].into_iter());
        assert_eq!(jar.cookie_header_for(&url("http://example.com/")), None);
    }

    #[test]
    fn max_age_in_the_future_is_kept() {
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url("http://example.com/"),
            ["a=1; Max-Age=3600"].into_iter(),
        );
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/"))
                .as_deref(),
            Some("a=1")
        );
    }

    #[test]
    fn expires_in_the_past_is_dropped() {
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url("http://example.com/"),
            ["a=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT"].into_iter(),
        );
        assert_eq!(jar.cookie_header_for(&url("http://example.com/")), None);
    }

    #[test]
    fn expires_in_the_future_is_kept() {
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url("http://example.com/"),
            ["a=1; Expires=Wed, 01 Jan 2099 00:00:00 GMT"].into_iter(),
        );
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/"))
                .as_deref(),
            Some("a=1")
        );
    }

    #[test]
    fn max_age_takes_precedence_over_expires() {
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url("http://example.com/"),
            ["a=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=3600"].into_iter(),
        );
        assert_eq!(
            jar.cookie_header_for(&url("http://example.com/"))
                .as_deref(),
            Some("a=1")
        );
    }

    #[test]
    fn same_name_domain_path_replaces_previous_value() {
        let mut jar = CookieJar::new();
        let req = url("http://example.com/");
        jar.store_from_response(&req, ["a=1"].into_iter());
        jar.store_from_response(&req, ["a=2"].into_iter());
        assert_eq!(jar.cookie_header_for(&req).as_deref(), Some("a=2"));
    }

    #[test]
    fn multiple_cookies_are_joined_with_semicolons() {
        let mut jar = CookieJar::new();
        let req = url("http://example.com/");
        jar.store_from_response(&req, ["a=1", "b=2"].into_iter());
        let header = jar.cookie_header_for(&req).unwrap();
        assert!(header.contains("a=1"));
        assert!(header.contains("b=2"));
    }

    #[test]
    fn ignores_malformed_set_cookie() {
        let mut jar = CookieJar::new();
        jar.store_from_response(
            &url("http://example.com/"),
            ["no equals sign here"].into_iter(),
        );
        assert_eq!(jar.cookie_header_for(&url("http://example.com/")), None);
    }

    #[test]
    fn days_from_civil_matches_hand_verified_epoch_offsets() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }
}
