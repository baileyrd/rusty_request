//! A small, hand-rolled JSON value type, parser, and serializer -- no
//! `serde`/`serde_json`. Deliberately just enough for request bodies
//! and response payloads: build a [`Value`] with the `From` impls and
//! [`Value::object`]/[`Value::array`], parse response bytes with
//! [`Value::parse`], and read fields back with the `as_*`/[`Value::get`]
//! accessors. There is no derive-based mapping to/from arbitrary Rust
//! structs -- that would need its own procedural macro to do well, well
//! beyond what an HTTP client's `.json()` convenience needs.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    /// Order of insertion is preserved (a `Vec`, not a `HashMap`) so
    /// serialized output is deterministic -- handy for tests and for
    /// matching a server's expectations about field order.
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn object() -> Value {
        Value::Object(Vec::new())
    }

    pub fn array() -> Value {
        Value::Array(Vec::new())
    }

    /// Inserts `key: value` into an object, replacing any existing
    /// entry with the same key. Panics if `self` isn't `Value::Object`
    /// -- a programmer error (building a JSON body), not a runtime one.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> &mut Value {
        let Value::Object(entries) = self else {
            panic!("Value::insert called on a non-object Value");
        };
        let key = key.into();
        if let Some(existing) = entries.iter_mut().find(|(k, _)| *k == key) {
            existing.1 = value.into();
        } else {
            entries.push((key, value.into()));
        }
        self
    }

    pub fn push(&mut self, value: impl Into<Value>) -> &mut Value {
        let Value::Array(items) = self else {
            panic!("Value::push called on a non-array Value");
        };
        items.push(value.into());
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Compact serialization (no insignificant whitespace).
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Number(n) => write_number(*n, out),
            Value::String(s) => write_json_string(s, out),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_json(out);
                }
                out.push(']');
            }
            Value::Object(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }

    pub fn parse(input: &str) -> Result<Value, String> {
        let mut p = Parser {
            bytes: input.as_bytes(),
            pos: 0,
        };
        p.skip_ws();
        let value = p.parse_value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(format!("trailing data at byte {}", p.pos));
        }
        Ok(value)
    }
}

fn write_number(n: f64, out: &mut String) {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        let _ = write!(out, "{}", n as i64);
    } else {
        let _ = write!(out, "{n}");
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(v)
    }
}
macro_rules! impl_from_number {
    ($($t:ty),*) => {
        $(impl From<$t> for Value {
            fn from(n: $t) -> Self { Value::Number(n as f64) }
        })*
    };
}
impl_from_number!(i8, i16, i32, i64, u8, u16, u32, u64, usize, isize, f32, f64);

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => v.into(),
            None => Value::Null,
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!(
                "expected `{}` at byte {}, found {:?}",
                b as char,
                self.pos,
                self.peek().map(|c| c as char)
            ))
        }
    }

    fn literal(&mut self, lit: &str) -> Result<(), String> {
        let end = self.pos + lit.len();
        if self.bytes.get(self.pos..end) == Some(lit.as_bytes()) {
            self.pos = end;
            Ok(())
        } else {
            Err(format!("expected `{lit}` at byte {}", self.pos))
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Value::String(self.parse_string()?)),
            Some(b't') => {
                self.literal("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Value::Null)
            }
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            other => Err(format!(
                "unexpected {:?} at byte {}",
                other.map(|c| c as char),
                self.pos
            )),
        }
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut entries: Vec<(String, Value)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            entries.retain(|(k, _)| *k != key);
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                other => {
                    return Err(format!(
                        "expected `,` or `}}` at byte {}, found {:?}",
                        self.pos,
                        other.map(|c| c as char)
                    ))
                }
            }
        }
        Ok(Value::Object(entries))
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                other => {
                    return Err(format!(
                        "expected `,` or `]` at byte {}, found {:?}",
                        self.pos,
                        other.map(|c| c as char)
                    ))
                }
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err("unterminated string".to_string()),
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => {
                            s.push('"');
                            self.pos += 1;
                        }
                        Some(b'\\') => {
                            s.push('\\');
                            self.pos += 1;
                        }
                        Some(b'/') => {
                            s.push('/');
                            self.pos += 1;
                        }
                        Some(b'b') => {
                            s.push('\u{8}');
                            self.pos += 1;
                        }
                        Some(b'f') => {
                            s.push('\u{c}');
                            self.pos += 1;
                        }
                        Some(b'n') => {
                            s.push('\n');
                            self.pos += 1;
                        }
                        Some(b'r') => {
                            s.push('\r');
                            self.pos += 1;
                        }
                        Some(b't') => {
                            s.push('\t');
                            self.pos += 1;
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            let cp = self.parse_hex4()?;
                            let ch = if (0xD800..=0xDBFF).contains(&cp) {
                                // High surrogate: must be followed by a
                                // low surrogate to form one code point.
                                if self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u") {
                                    self.pos += 2;
                                    let low = self.parse_hex4()?;
                                    if !(0xDC00..=0xDFFF).contains(&low) {
                                        return Err("invalid low surrogate".to_string());
                                    }
                                    let c = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                                    char::from_u32(c).ok_or("invalid surrogate pair")?
                                } else {
                                    return Err("unpaired high surrogate".to_string());
                                }
                            } else {
                                char::from_u32(cp).ok_or("invalid unicode escape")?
                            };
                            s.push(ch);
                        }
                        other => {
                            return Err(format!("invalid escape {:?}", other.map(|c| c as char)))
                        }
                    }
                }
                Some(_) => {
                    // Copy one full UTF-8 char at a time so multi-byte
                    // sequences in the input survive intact.
                    let rest = std::str::from_utf8(&self.bytes[self.pos..])
                        .map_err(|e| format!("invalid utf-8: {e}"))?;
                    let ch = rest.chars().next().ok_or("unexpected end of string")?;
                    s.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        Ok(s)
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let hex = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or("truncated unicode escape")?;
        let hex = std::str::from_utf8(hex).map_err(|_| "invalid unicode escape")?;
        let cp = u32::from_str_radix(hex, 16).map_err(|_| "invalid unicode escape")?;
        self.pos += 4;
        Ok(cp)
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        s.parse::<f64>()
            .map(Value::Number)
            .map_err(|e| format!("invalid number `{s}`: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_object() {
        let mut v = Value::object();
        v.insert("name", "Ada");
        v.insert("age", 36);
        v.insert("active", true);
        let s = v.to_json_string();
        let parsed = Value::parse(&s).unwrap();
        assert_eq!(parsed.get("name").unwrap().as_str(), Some("Ada"));
        assert_eq!(parsed.get("age").unwrap().as_f64(), Some(36.0));
        assert_eq!(parsed.get("active").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn parses_nested_array_and_escapes() {
        let v = Value::parse(r#"{"items":[1,2.5,"a\nb","xéy"],"n":null}"#).unwrap();
        let items = v.get("items").unwrap().as_array().unwrap();
        assert_eq!(items[0].as_f64(), Some(1.0));
        assert_eq!(items[1].as_f64(), Some(2.5));
        assert_eq!(items[2].as_str(), Some("a\nb"));
        assert_eq!(items[3].as_str(), Some("x\u{e9}y"));
        assert!(v.get("n").unwrap().is_null());
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(Value::parse("{}garbage").is_err());
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(Value::parse(r#"{"a":"#).is_err());
    }

    #[test]
    fn integral_numbers_serialize_without_decimal_point() {
        let v = Value::from(42i64);
        assert_eq!(v.to_json_string(), "42");
    }
}
