//! Search backends. Each is a real public API reached through the world's egress:
//!
//! * **Brave Search API**: `GET https://api.search.brave.com/res/v1/web/search?q=…&count=…` with
//!   `X-Subscription-Token: <key>` and `Accept: application/json`; results live in
//!   `web.results[]` as `{title, url, description, age?, extra_snippets?}` (descriptions carry
//!   `<strong>` highlighting). A response without a `web` section means no web results.
//! * **SearXNG**: `GET <instance>/search?q=…&format=json`; results live in `results[]` as
//!   `{url, title, content, engine, engines, score, publishedDate?}`. An instance that has not
//!   enabled the `json` output format answers 403.

use aho_egress::{HostList, HttpRequest, HttpResponse, Limits};
use aho_types::Value;
use url::Url;

use crate::html::strip_tags;

pub const BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
/// Brave's documented maximum `count`.
pub const BRAVE_MAX_COUNT: u32 = 20;

#[derive(Clone)]
pub enum Backend {
    Brave { url: String, key: String },
    Searxng { url: String },
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Brave { url, .. } => f.debug_struct("Brave").field("url", url).field("key", &"<secret>").finish(),
            Backend::Searxng { url } => f.debug_struct("Searxng").field("url", url).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Brave { .. } => "brave",
            Backend::Searxng { .. } => "searxng",
        }
    }

    /// The URL a query goes to; `Err` names the key holding a malformed endpoint.
    pub fn endpoint(&self, query: &str, count: u32) -> Result<String, String> {
        let (key, base) = match self {
            Backend::Brave { url, .. } => ("web.search.brave_url", url.clone()),
            Backend::Searxng { url } => {
                let trimmed = url.trim_end_matches('/');
                let base = if trimmed.ends_with("/search") { trimmed.to_string() } else { format!("{trimmed}/search") };
                ("web.search.searxng_url", base)
            }
        };
        let mut u =
            Url::parse(&base).map_err(|e| format!("`{key}` = `{base}` is not a URL ({e}); set it to the endpoint's http(s) URL"))?;
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("q", query);
            match self {
                Backend::Brave { .. } => {
                    q.append_pair("count", &count.clamp(1, BRAVE_MAX_COUNT).to_string());
                }
                Backend::Searxng { .. } => {
                    q.append_pair("format", "json");
                }
            }
        }
        Ok(u.to_string())
    }

    pub fn request(&self, query: &str, count: u32, limits: Limits, hosts: HostList) -> Result<HttpRequest, String> {
        let req = HttpRequest::get(self.endpoint(query, count)?).header("accept", "application/json").limits(limits).hosts(hosts);
        Ok(match self {
            Backend::Brave { key, .. } => req.secret_header("x-subscription-token", key.clone()),
            Backend::Searxng { .. } => req,
        })
    }

    /// Operator-facing error on a non-2xx answer or a body that is not the documented shape.
    pub fn parse(&self, resp: &HttpResponse, count: u32) -> Result<Vec<Hit>, String> {
        if !resp.is_success() {
            return Err(self.http_error(resp));
        }
        if resp.truncated {
            return Err(format!(
                "the {} response was larger than the size cap and was cut; no results were read. Raise web.search.max_bytes.",
                self.name()
            ));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| format!("the {} answer is not JSON ({e}); check that the endpoint is a {} API", self.name(), self.name()))?;
        let (rows, snippet_key) = match self {
            Backend::Brave { .. } => (v.get("web").and_then(|w| w.get("results")), "description"),
            Backend::Searxng { .. } => (v.get("results"), "content"),
        };
        let rows = match rows {
            None if matches!(self, Backend::Brave { .. }) && v.get("type").is_some() => return Ok(Vec::new()),
            None => return Err(format!("the {} answer has no result list; check that the endpoint is a {} API", self.name(), self.name())),
            Some(r) => r.as_array().ok_or_else(|| format!("the {} result list is not an array", self.name()))?,
        };
        let s = |r: &Value, k: &str| strip_tags(r.get(k).and_then(Value::as_str).unwrap_or(""));
        Ok(rows
            .iter()
            .filter(|r| r.get("url").and_then(Value::as_str).is_some())
            .map(|r| Hit { title: s(r, "title"), url: s(r, "url"), snippet: s(r, snippet_key) })
            .take(count as usize)
            .collect())
    }

    fn http_error(&self, resp: &HttpResponse) -> String {
        let body = String::from_utf8_lossy(&resp.body[..resp.body.len().min(300)]).into_owned();
        let status = resp.status;
        match (self, status) {
            (Backend::Brave { .. }, 401 | 403) => format!(
                "Brave Search refused the API key (HTTP {status}); no search was run. Check the key with `aho credential set brave`."
            ),
            (Backend::Brave { .. }, 429) => {
                "Brave Search is rate-limiting this key (HTTP 429); no results. Wait and retry, or raise the plan's quota.".to_string()
            }
            (Backend::Searxng { .. }, 403) => "the SearXNG instance refused JSON output (HTTP 403); no results. Enable `json` under `search.formats` in its settings.yml, or point web.search.searxng_url at an instance that allows it."
                .to_string(),
            _ => format!("the {} endpoint answered HTTP {status}: {}", self.name(), body.trim()),
        }
    }
}

pub fn render(query: &str, backend: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("No results for `{query}` ({backend}).");
    }
    let mut out = format!("Results for `{query}` ({backend}):\n");
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}\n", i + 1, if h.title.is_empty() { &h.url } else { &h.title }, h.url));
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
    }
    out
}
