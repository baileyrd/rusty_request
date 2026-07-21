use crate::body::Body;
use crate::header::HeaderMap;
use crate::method::Method;
use crate::url::Url;
use std::time::Duration;

/// A fully-assembled request, ready to send. Built via
/// [`crate::Client::request`]/[`crate::RequestBuilder`] rather than
/// constructed directly.
#[derive(Debug, Clone)]
pub struct Request {
    pub(crate) method: Method,
    pub(crate) url: Url,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Body,
    pub(crate) timeout: Option<Duration>,
}

impl Request {
    pub fn method(&self) -> Method {
        self.method
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn body(&self) -> &Body {
        &self.body
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}
