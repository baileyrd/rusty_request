//! A minimal, case-insensitive multi-map for HTTP headers. Order of
//! insertion is preserved (useful for predictable wire output and
//! tests); lookups are case-insensitive per RFC 7230 §3.2.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderMap {
    entries: Vec<(String, String)>,
}

impl HeaderMap {
    pub fn new() -> Self {
        HeaderMap::default()
    }

    /// Sets `name` to `value`, replacing any existing entries with the
    /// same name (case-insensitive).
    pub fn insert(&mut self, name: &str, value: &str) -> Result<()> {
        validate_name(name)?;
        validate_value(value)?;
        self.entries.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.entries.push((name.to_string(), value.to_string()));
        Ok(())
    }

    /// Adds `name: value` without removing any existing entries for the
    /// same name -- for headers that legitimately repeat.
    pub fn append(&mut self, name: &str, value: &str) -> Result<()> {
        validate_name(name)?;
        validate_value(value)?;
        self.entries.push((name.to_string(), value.to_string()));
        Ok(())
    }

    /// The first value for `name`, case-insensitive.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b':' && b != b'\r' && b != b'\n')
    {
        return Err(Error::InvalidHeader(format!(
            "invalid header name `{name}`"
        )));
    }
    Ok(())
}

fn validate_value(value: &str) -> Result<()> {
    // Reject bare CR/LF -- allowing them would let a caller smuggle
    // extra headers or a second request into the stream.
    if value.bytes().any(|b| b == b'\r' || b == b'\n') {
        return Err(Error::InvalidHeader(format!(
            "header value must not contain CR/LF: `{value}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_is_case_insensitive_and_replaces() {
        let mut h = HeaderMap::new();
        h.insert("Content-Type", "text/plain").unwrap();
        h.insert("content-type", "application/json").unwrap();
        assert_eq!(h.get("CONTENT-TYPE"), Some("application/json"));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn append_keeps_both() {
        let mut h = HeaderMap::new();
        h.append("X-A", "1").unwrap();
        h.append("X-A", "2").unwrap();
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn rejects_crlf_injection_in_value() {
        let mut h = HeaderMap::new();
        assert!(h.insert("X-A", "evil\r\nX-Injected: yes").is_err());
    }

    #[test]
    fn rejects_colon_in_name() {
        let mut h = HeaderMap::new();
        assert!(h.insert("X-A:", "1").is_err());
    }
}
