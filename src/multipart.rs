//! Hand-rolled `multipart/form-data` encoding (RFC 7578) for
//! [`crate::RequestBuilder::multipart`] -- no dependency, just boundary
//! generation and part framing over the same fully-buffered [`crate::Body`]
//! every other request body uses today. Streaming large file uploads
//! without buffering the whole part in memory is left to the eventual
//! streaming-bodies work (see the backlog); this is a reasonable buffered
//! first pass in the meantime.

const DEFAULT_FILE_CONTENT_TYPE: &str = "application/octet-stream";

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    value: Vec<u8>,
}

/// A `multipart/form-data` body under construction: zero or more named
/// parts, each either a plain text field or a file (with its own
/// filename and content type). Pass the finished form to
/// [`crate::RequestBuilder::multipart`].
#[derive(Default)]
pub struct Multipart {
    parts: Vec<Part>,
}

impl Multipart {
    pub fn new() -> Self {
        Multipart::default()
    }

    /// Adds a plain `name=value` field -- no `Content-Type` or filename,
    /// the same shape a plain HTML form field takes.
    pub fn text(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.parts.push(Part {
            name: name.into(),
            filename: None,
            content_type: None,
            value: value.into().into_bytes(),
        });
        self
    }

    /// Adds a file part with `Content-Type: application/octet-stream`;
    /// see [`Multipart::file_with_content_type`] to set a specific type
    /// (e.g. `image/png`).
    pub fn file(
        self,
        name: impl Into<String>,
        filename: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        self.file_with_content_type(name, filename, DEFAULT_FILE_CONTENT_TYPE, bytes)
    }

    pub fn file_with_content_type(
        mut self,
        name: impl Into<String>,
        filename: impl Into<String>,
        content_type: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        self.parts.push(Part {
            name: name.into(),
            filename: Some(filename.into()),
            content_type: Some(content_type.into()),
            value: bytes.into(),
        });
        self
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Encodes every part with a freshly generated boundary, returning
    /// the finished body bytes and the `Content-Type` header value
    /// (`multipart/form-data; boundary=...`) it must be sent with.
    pub(crate) fn encode(&self) -> (Vec<u8>, String) {
        let boundary = generate_boundary();
        let mut body = Vec::new();

        for part in &self.parts {
            body.extend_from_slice(b"--");
            body.extend_from_slice(boundary.as_bytes());
            body.extend_from_slice(b"\r\n");

            body.extend_from_slice(b"Content-Disposition: form-data; name=\"");
            body.extend_from_slice(quote_escape(&part.name).as_bytes());
            body.extend_from_slice(b"\"");
            if let Some(filename) = &part.filename {
                body.extend_from_slice(b"; filename=\"");
                body.extend_from_slice(quote_escape(filename).as_bytes());
                body.extend_from_slice(b"\"");
            }
            body.extend_from_slice(b"\r\n");

            if let Some(content_type) = &part.content_type {
                body.extend_from_slice(b"Content-Type: ");
                body.extend_from_slice(content_type.as_bytes());
                body.extend_from_slice(b"\r\n");
            }

            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(&part.value);
            body.extend_from_slice(b"\r\n");
        }

        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"--\r\n");

        (body, format!("multipart/form-data; boundary={boundary}"))
    }
}

/// Backslash-escapes `"` and `\` in a field/file name per RFC 2183's
/// quoted-string rule (which RFC 7578's `Content-Disposition` parameters
/// follow) -- without this, a name containing `"` could break out of the
/// quoted parameter and smuggle extra `Content-Disposition` parameters
/// into the part header.
fn quote_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A boundary that won't collide with real part content by construction:
/// a fixed, distinctive prefix (never going to occur naturally) plus 128
/// bits of the crate's non-cryptographic random source (`crate::rand`) --
/// collision-resistant, not adversary-resistant, which is all a boundary
/// token needs. Every character used (`A-Za-z0-9-`) is in RFC 2046's
/// `bcharsnospace`, so it never needs escaping in the `Content-Type`
/// header or as a delimiter.
fn generate_boundary() -> String {
    format!(
        "RustyRequestBoundary{:016x}{:016x}",
        crate::rand::next_u64(),
        crate::rand::next_u64()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_string(form: &Multipart) -> (String, String) {
        let (body, content_type) = form.encode();
        (String::from_utf8_lossy(&body).into_owned(), content_type)
    }

    #[test]
    fn content_type_header_carries_the_boundary_used_in_the_body() {
        let form = Multipart::new().text("a", "1");
        let (body, content_type) = body_string(&form);
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        assert!(body.contains(&format!("--{boundary}\r\n")));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn text_part_has_no_content_type_or_filename() {
        let form = Multipart::new().text("field", "value");
        let (body, _) = body_string(&form);
        assert!(body.contains("Content-Disposition: form-data; name=\"field\"\r\n\r\nvalue\r\n"));
        assert!(!body.contains("filename="));
        assert!(!body.contains("Content-Type: application/octet-stream"));
    }

    #[test]
    fn file_part_defaults_to_octet_stream() {
        let form = Multipart::new().file("upload", "a.bin", vec![1, 2, 3]);
        let (body, _) = body_string(&form);
        assert!(body.contains(
            "Content-Disposition: form-data; name=\"upload\"; filename=\"a.bin\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        ));
    }

    #[test]
    fn file_part_with_explicit_content_type() {
        let form =
            Multipart::new().file_with_content_type("upload", "a.png", "image/png", vec![0xff]);
        let (body, _) = body_string(&form);
        assert!(body.contains("Content-Type: image/png\r\n"));
    }

    #[test]
    fn multiple_parts_are_each_preceded_by_a_boundary() {
        let form = Multipart::new().text("a", "1").text("b", "2");
        let (body, content_type) = body_string(&form);
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        assert_eq!(body.matches(&format!("--{boundary}\r\n")).count(), 2);
    }

    #[test]
    fn quotes_and_backslashes_in_names_are_escaped() {
        let form = Multipart::new().text("weird\"name\\", "value");
        let (body, _) = body_string(&form);
        assert!(body.contains("name=\"weird\\\"name\\\\\""));
    }

    #[test]
    fn binary_file_content_round_trips_unmodified() {
        let bytes = vec![0u8, 1, 2, 0xff, 0xfe, b'\r', b'\n'];
        let form = Multipart::new().file("f", "bin", bytes.clone());
        let (body, _) = form.encode();
        let window = body
            .windows(bytes.len())
            .position(|w| w == bytes.as_slice());
        assert!(window.is_some());
    }

    #[test]
    fn successive_boundaries_are_not_equal() {
        let (_, ct1) = Multipart::new().text("a", "1").encode();
        let (_, ct2) = Multipart::new().text("a", "1").encode();
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn is_empty_reflects_whether_any_part_was_added() {
        assert!(Multipart::new().is_empty());
        assert!(!Multipart::new().text("a", "1").is_empty());
    }
}
