use gix_error::{ErrorExt, Result, ResultExt, message};

use crate::http::{Body, Method, Options, Request, Response};

/// An HTTP client using the blocking client of `reqwest`.
///
/// `https://` URLs need a TLS backend of `reqwest` to be enabled, for instance by enabling one of the `reqwest`
/// TLS features of `gix`.
pub struct Client {
    client: ::reqwest::blocking::Client,
    extra_headers: Vec<(String, String)>,
}

impl Client {
    /// Create a new instance configured with `options`.
    pub fn new(options: &Options) -> Result<Self> {
        let mut builder = ::reqwest::blocking::Client::builder().timeout(None::<std::time::Duration>);
        if let Some(user_agent) = &options.user_agent {
            builder = builder.user_agent(user_agent);
        }
        if let Some(proxy) = &options.proxy {
            builder = builder
                .proxy(::reqwest::Proxy::all(proxy).or_raise(|| message!("Invalid proxy URL '{proxy}'").validation())?);
        }
        if let Some(timeout) = options.connect_timeout {
            builder = builder.connect_timeout(timeout);
        }
        let client = builder
            .build()
            .or_raise(|| message("Could not create the HTTP client"))?;
        let extra_headers = options
            .extra_headers
            .iter()
            .filter_map(|header| {
                let (name, value) = header.split_once(':')?;
                Some((name.trim().to_owned(), value.trim().to_owned()))
            })
            .collect();
        Ok(Client { client, extra_headers })
    }
}

impl crate::http::Client for Client {
    fn request(&self, request: Request) -> Result<Response> {
        let Request {
            method,
            url,
            headers,
            body,
        } = request;
        let method = match method {
            Method::Get => ::reqwest::Method::GET,
            Method::Post => ::reqwest::Method::POST,
            Method::Put => ::reqwest::Method::PUT,
        };
        let mut builder = self.client.request(method, &url);
        for (name, value) in self.extra_headers.iter().chain(headers.iter()) {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder = match body {
            Body::Empty => builder,
            Body::Bytes(bytes) => builder.body(bytes),
            Body::Stream { reader, len } => builder.body(::reqwest::blocking::Body::sized(reader, len)),
        };
        let response = builder.send().map_err(|err| {
            err.without_url()
                .and_raise(message!("HTTP request to '{}' failed", redact(&url)).retryable())
        })?;
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned())))
            .collect();
        Ok(Response {
            status: response.status().as_u16(),
            headers,
            body: Box::new(response),
        })
    }
}

/// Strip the query from `url`, as it may carry signatures.
fn redact(url: &str) -> &str {
    url.split_once('?').map_or(url, |(base, _)| base)
}
