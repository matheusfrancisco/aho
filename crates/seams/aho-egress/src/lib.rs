//! Egress: the bound world's network, as one bounded HTTP exchange per call.
//!
//! * [`Egress`] / [`EgressHandle`] (`HandleKind::Egress(Route::Sandbox)`): the only network a tool
//!   may hold (tools never hold `Egress(Route::Host)`). Under `world = host` the world provides
//!   the host network and [`Egress::enforced_by`] says `policy only`; a sandbox world routes it
//!   through its own NIC and says what confines it.
//! * [`HttpRequest`] carries its own bounds: [`Limits`] (body bytes, wall-clock timeout, redirect
//!   cap) and the operator's [`HostList`]. Every implementation enforces the same contract:
//!   the list is checked on every hop (redirects included) before connecting, reading stops at
//!   `max_body_bytes` with `truncated = true`, more than `max_redirects` hops is an error, the whole
//!   exchange runs under `timeout_ms`, and `secret` headers are dropped when a redirect leaves the
//!   original host.
//! * [`HostList`] is the egress allow/deny list (`egress.allow`, `egress.deny`). Under `host` it
//!   is enforced for `web_fetch`/MCP only; `bash` can `curl` anything.
//!
//! This crate performs no I/O.

#![forbid(unsafe_code)]

mod hosts;

use std::fmt;
use std::sync::Arc;

use aho_cap::{Handle, HandleKind, Route, SeamId};
use aho_types::BoxFuture;

pub use crate::hosts::{ALLOW_KEY, DENY_KEY, HostList, ParsedUrl, config_keys, glob_match, parse_url};

/// Seam bound by the world bundle that provides [`EgressHandle`].
pub const SEAM: SeamId = SeamId("egress");

/// Default cap on a response body read into memory (5 MiB).
pub const DEFAULT_MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
/// Default wall-clock limit for one exchange, redirects included.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Default number of redirects followed before giving up.
pub const DEFAULT_MAX_REDIRECTS: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Head,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Head => "HEAD",
        }
    }
}

/// One request header. `secret` values never print and never follow a redirect to another host.
#[derive(Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
    pub secret: bool,
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = if self.secret { "<secret>" } else { self.value.as_str() };
        f.debug_struct("Header").field("name", &self.name).field("value", &value).finish()
    }
}

/// The bounds every implementation enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Reading stops here; the response says `truncated = true`.
    pub max_body_bytes: usize,
    /// Whole exchange, every hop and the body read included.
    pub timeout_ms: u64,
    /// Redirects followed; one more is [`EgressError::TooManyRedirects`].
    pub max_redirects: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_body_bytes: DEFAULT_MAX_BODY_BYTES, timeout_ms: DEFAULT_TIMEOUT_MS, max_redirects: DEFAULT_MAX_REDIRECTS }
    }
}

#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<Header>,
    pub body: Option<Vec<u8>>,
    pub limits: Limits,
    /// Checked on the first hop and on every redirect target before connecting.
    pub hosts: HostList,
}

impl HttpRequest {
    /// A `GET` with default limits and an empty host list (everything allowed).
    pub fn get(url: impl Into<String>) -> HttpRequest {
        HttpRequest {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            limits: Limits::default(),
            hosts: HostList::default(),
        }
    }
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header { name: name.into(), value: value.into(), secret: false });
        self
    }
    /// A credential-bearing header: redacted in `Debug`, dropped on a cross-host redirect.
    pub fn secret_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header { name: name.into(), value: value.into(), secret: true });
        self
    }
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
    pub fn hosts(mut self, hosts: HostList) -> Self {
        self.hosts = hosts;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// The URL that answered, after redirects.
    pub final_url: String,
    /// Lower-cased names, in arrival order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The body was cut at `Limits::max_body_bytes`.
    pub truncated: bool,
    pub redirects: u32,
}

impl HttpResponse {
    /// First header named `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    /// The media type without parameters, lower-cased (`text/html`).
    pub fn media_type(&self) -> Option<String> {
        self.header("content-type").map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase()).filter(|m| !m.is_empty())
    }
    /// The `charset` parameter of `content-type`, lower-cased.
    pub fn charset(&self) -> Option<String> {
        let ct = self.header("content-type")?;
        ct.split(';').skip(1).find_map(|p| {
            let (k, v) = p.split_once('=')?;
            k.trim().eq_ignore_ascii_case("charset").then(|| v.trim().trim_matches('"').to_ascii_lowercase())
        })
    }
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressError {
    /// The host list refused a hop; nothing was sent to that host.
    Denied(String),
    InvalidUrl {
        url: String,
        why: String,
    },
    TooManyRedirects {
        limit: u32,
        url: String,
    },
    Timeout {
        ms: u64,
    },
    /// Could not connect or the connection failed mid-exchange.
    Connect {
        url: String,
        why: String,
    },
    /// The world cannot perform this exchange (e.g. a scheme it does not route).
    Unsupported(String),
}

impl fmt::Display for EgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EgressError::Denied(why) => write!(f, "{why}"),
            EgressError::InvalidUrl { url, why } => write!(f, "`{url}` is not a URL aho will fetch: {why}. Nothing was sent."),
            EgressError::TooManyRedirects { limit, url } => write!(
                f,
                "stopped after {limit} redirects; the next hop was {url}. Nothing more was fetched. Fetch the final URL directly, or raise the redirect cap."
            ),
            EgressError::Timeout { ms } => {
                write!(f, "the request did not finish within {ms} ms and was abandoned. Retry, or raise the timeout.")
            }
            EgressError::Connect { url, why } => write!(f, "could not reach {url}: {why}"),
            EgressError::Unsupported(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for EgressError {}

/// The bound world's network. Implementations enforce every field of [`HttpRequest`].
pub trait Egress: Send + Sync {
    fn fetch(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, EgressError>>;
    /// The mechanism confining this network: `policy only` on the host, the sandbox's otherwise.
    fn enforced_by(&self) -> &str;
}

#[derive(Clone)]
pub struct EgressHandle(pub Arc<dyn Egress>);

impl Handle for EgressHandle {
    const KIND: HandleKind = HandleKind::Egress(Route::Sandbox);
}

impl fmt::Debug for EgressHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EgressHandle").field(&self.0.enforced_by()).finish()
    }
}

/// Redirect statuses an implementation follows (with a `Location`).
pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The method of the next hop: 303 always becomes `GET`, 301/302 turn a `POST` into `GET`
/// (what browsers do); 307/308 keep method and body. Returns `(method, keep_body)`.
pub fn redirect_method(status: u16, method: Method) -> (Method, bool) {
    match (status, method) {
        (303, Method::Head) => (Method::Head, false),
        (303, _) | (301 | 302, Method::Post) => (Method::Get, false),
        (_, m) => (m, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(headers: &[(&str, &str)]) -> HttpResponse {
        HttpResponse {
            status: 200,
            final_url: "http://x/".into(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: vec![],
            truncated: false,
            redirects: 0,
        }
    }

    #[test]
    fn media_type_and_charset_parse_from_content_type() {
        let r = resp(&[("content-type", "Text/HTML; Charset=\"ISO-8859-1\"")]);
        assert_eq!(r.media_type().as_deref(), Some("text/html"));
        assert_eq!(r.charset().as_deref(), Some("iso-8859-1"));
        assert_eq!(resp(&[]).media_type(), None);
    }

    #[test]
    fn secret_headers_never_print() {
        let req = HttpRequest::get("http://x/").secret_header("x-subscription-token", "hunter2").header("accept", "text/html");
        let dbg = format!("{req:?}");
        assert!(!dbg.contains("hunter2") && dbg.contains("<secret>") && dbg.contains("text/html"), "{dbg}");
    }

    #[test]
    fn redirect_method_follows_browser_rules() {
        assert_eq!(redirect_method(303, Method::Post), (Method::Get, false));
        assert_eq!(redirect_method(302, Method::Post), (Method::Get, false));
        assert_eq!(redirect_method(307, Method::Post), (Method::Post, true));
        assert_eq!(redirect_method(301, Method::Get), (Method::Get, true));
        assert!(is_redirect(308) && !is_redirect(304));
    }
}
