//! The HTTP requests the LFS client needs, behind a trait so any HTTP implementation can be used.
use std::{io::Read, time::Duration};

use gix_error::Result;

/// An implementation using the blocking client of `reqwest`.
#[cfg(feature = "http-client-reqwest")]
pub mod reqwest;

/// The HTTP methods used by LFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Download objects.
    Get,
    /// Call the batch and verify APIs.
    Post,
    /// Upload objects.
    Put,
}

/// The body of a [`Request`].
pub enum Body {
    /// No body.
    Empty,
    /// A body held in memory.
    Bytes(Vec<u8>),
    /// A body of `len` bytes, read from `reader` while sending.
    Stream {
        /// The source of the body.
        reader: Box<dyn Read + Send>,
        /// The amount of bytes `reader` yields, sent as `Content-Length`.
        len: u64,
    },
}

/// A request to send.
pub struct Request {
    /// The method to use.
    pub method: Method,
    /// The URL to send the request to.
    pub url: String,
    /// The headers to send, in addition to those the implementation adds.
    pub headers: Vec<(String, String)>,
    /// The body to send.
    pub body: Body,
}

/// The response to a [`Request`], whatever its status.
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// The response headers.
    pub headers: Vec<(String, String)>,
    /// The response body, read on demand.
    pub body: Box<dyn Read + Send>,
}

impl Response {
    /// Return the first value of the header `name`, compared case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Send HTTP requests.
pub trait Client: Send + Sync {
    /// Send `request` and return the response, whatever its status.
    ///
    /// Errors are reserved for failures to obtain a response, and should be classified as retryable if
    /// they are transient, like connection failures.
    fn request(&self, request: Request) -> Result<Response>;
}

/// Settings for HTTP clients, typically taken from the `http.*` configuration of the remote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The `User-Agent` header to send.
    pub user_agent: Option<String>,
    /// Headers like `Name: value` to send with every request, as configured with `http.extraHeader`.
    pub extra_headers: Vec<String>,
    /// The proxy to send all requests through.
    pub proxy: Option<String>,
    /// The time to wait for a connection to be established.
    pub connect_timeout: Option<Duration>,
}
