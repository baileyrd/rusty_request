/// A request body. Always fully buffered in memory in this MVP -- no
/// streaming request bodies yet (see the backlog).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Body {
    #[default]
    Empty,
    Bytes(Vec<u8>),
}

impl Body {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Body::Empty => &[],
            Body::Bytes(b) => b,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
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
