use crate::error::{Error, Result};
use crate::header::HeaderMap;
use crate::json;
use crate::status::StatusCode;
use crate::url::Url;

#[derive(Debug, Clone)]
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    url: Url,
    body: Vec<u8>,
}

impl Response {
    pub(crate) fn new(status: StatusCode, headers: HeaderMap, url: Url, body: Vec<u8>) -> Self {
        Response {
            status,
            headers,
            url,
            body,
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn bytes(&self) -> &[u8] {
        &self.body
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.body
    }

    /// Decodes the body as UTF-8 text. This MVP doesn't inspect
    /// `Content-Type`'s `charset` parameter -- every body is assumed
    /// UTF-8 (or ASCII, a subset of it), same as the common case
    /// `requests` handles without needing `chardet` to guess.
    pub fn text(&self) -> Result<String> {
        String::from_utf8(self.body.clone())
            .map_err(|e| Error::InvalidResponse(format!("response body is not valid UTF-8: {e}")))
    }

    pub fn json(&self) -> Result<json::Value> {
        let text = self.text().map_err(|e| Error::Json(e.to_string()))?;
        json::Value::parse(&text).map_err(Error::Json)
    }

    /// Requests-style ergonomic error check: turns a 4xx/5xx status into
    /// `Err(Error::Status(..))`, otherwise passes `self` through
    /// unchanged so it can be chained: `client.get(url).send().await?.error_for_status()?`.
    pub fn error_for_status(self) -> Result<Self> {
        if self.status.is_client_error() || self.status.is_server_error() {
            Err(Error::Status(self.status))
        } else {
            Ok(self)
        }
    }
}
