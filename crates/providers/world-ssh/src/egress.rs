//! The remote host's network as the world's `Egress` (07: "`ssh` uses the remote's own network
//! and marks `NetworkEgress` as `enforced by: remote host`").
//!
//! Each hop is one `curl` run on the remote over the ControlMaster. Redirects are followed here,
//! never by curl, so the operator's host list is checked on every hop before anything is sent
//! and each hop is counted; `secret` headers are dropped once a redirect leaves the original
//! host. Headers and the URL travel as a curl config on stdin (never on a command line another
//! remote user could read with `ps`), the body right after it; both land in a private
//! `mktemp -d` directory that is removed when curl ends. The body is read here only up to
//! `max_body_bytes`; the rest is cut off by closing the channel.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_egress::{Egress, EgressError, HttpRequest, HttpResponse, Method, ParsedUrl, is_redirect, redirect_method};
use aho_types::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ENFORCED_BY;
use crate::ssh::{SSH_FAILED, Ssh, classify, sh_command};

/// Room for response headers on top of `max_body_bytes`.
const HEADER_ALLOWANCE: usize = 256 * 1024;

/// `$1` = `body` or `none`, `$2` = body length; stdin = `<config length>\n<curl config><body>`.
/// Both sizes are checked, so a connection that dies mid-upload never sends a partial request.
/// Exit codes: 50 no curl, 51/52 could not stage the request; otherwise curl's own.
const CURL: &str = r#"command -v curl >/dev/null 2>&1 || exit 50
IFS= read -r _n || exit 52
_d=$(mktemp -d "${TMPDIR:-/tmp}/aho-egress.XXXXXX") || exit 51
trap 'rm -rf -- "$_d"' EXIT
dd bs=1 count="$_n" of="$_d/cfg" 2>/dev/null || exit 51
[ "$(wc -c < "$_d/cfg" | tr -d ' ')" = "$_n" ] || exit 52
cat > "$_d/body" || exit 51
[ "$(wc -c < "$_d/body" | tr -d ' ')" = "$2" ] || exit 52
if [ "$1" = body ]; then curl -q -K "$_d/cfg" --data-binary "@$_d/body"; else curl -q -K "$_d/cfg"; fi
"#;

#[derive(Clone)]
pub struct SshEgress {
    ssh: Arc<Ssh>,
}

impl SshEgress {
    pub fn new(ssh: Arc<Ssh>) -> SshEgress {
        SshEgress { ssh }
    }

    fn host(&self) -> String {
        self.ssh.target().display()
    }

    async fn exchange(&self, req: HttpRequest, deadline: Instant) -> Result<HttpResponse, EgressError> {
        let mut url = req.url.clone();
        let origin = authority(&check_hop(&req, &url)?);
        let mut method = req.method;
        let mut body = req.body.clone();
        let mut redirects = 0u32;
        loop {
            let parsed = check_hop(&req, &url)?;
            let same_origin = authority(&parsed) == origin;
            let headers: Vec<(&str, &str)> =
                req.headers.iter().filter(|h| same_origin || !h.secret).map(|h| (h.name.as_str(), h.value.as_str())).collect();
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(EgressError::Timeout { ms: req.limits.timeout_ms });
            }
            let cfg = curl_config(&url, method, &headers, left)?;
            let raw = self.hop(&url, &cfg, body.as_deref(), req.limits.max_body_bytes, req.limits.timeout_ms).await?;
            let Framed { status, headers: resp_headers, body: data } = parse_response(&raw.bytes).ok_or_else(|| EgressError::Connect {
                url: url.clone(),
                why: format!("{} returned a response curl could not frame as HTTP", self.host()),
            })?;
            let location = resp_headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone());
            if let (true, Some(loc)) = (is_redirect(status), location) {
                let next = join_location(&url, &loc).map_err(|why| EgressError::InvalidUrl { url: loc.clone(), why })?;
                if redirects >= req.limits.max_redirects {
                    return Err(EgressError::TooManyRedirects { limit: req.limits.max_redirects, url: next });
                }
                redirects += 1;
                let (m, keep_body) = redirect_method(status, method);
                method = m;
                if !keep_body {
                    body = None;
                }
                url = next;
                continue;
            }
            let max = req.limits.max_body_bytes;
            let mut data = if method == Method::Head { Vec::new() } else { data.to_vec() };
            let truncated = raw.capped || data.len() > max;
            data.truncate(max);
            return Ok(HttpResponse { status, final_url: url, headers: resp_headers, body: data, truncated, redirects });
        }
    }

    /// One curl run; stops reading once headers plus `max_body + 1` bytes arrived.
    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-egress-hop")]
    async fn hop(&self, url: &str, cfg: &str, body: Option<&[u8]>, max_body: usize, timeout_ms: u64) -> Result<Raw, EgressError> {
        let connect = |why: String| EgressError::Connect { url: url.to_string(), why };
        let mut input = format!("{}\n{cfg}", cfg.len()).into_bytes();
        input.extend_from_slice(body.unwrap_or(&[]));
        let body_len = body.map_or(0, <[u8]>::len).to_string();
        let mut cmd = self.ssh.command(&sh_command(CURL, &[if body.is_some() { "body" } else { "none" }, &body_len]));
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| connect(format!("could not start ssh to {}: {e}", self.host())))?;
        let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err(connect("ssh started without its pipes".into()));
        };
        let writer = tokio::spawn(async move { stdin.write_all(&input).await });
        let errors = tokio::spawn(async move {
            let mut s = Vec::new();
            stderr.read_to_end(&mut s).await.map(|_| String::from_utf8_lossy(&s).into_owned())
        });
        let cap = HEADER_ALLOWANCE + max_body + 1;
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut capped = false;
        loop {
            let n = stdout.read(&mut buf).await.map_err(|e| connect(format!("the channel from {} broke: {e}", self.host())))?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            if bytes.len() >= cap {
                capped = true;
                break;
            }
        }
        if capped {
            // Cut the rest off: curl dies of SIGPIPE on the remote, its trap removes the staging dir.
            child.start_kill().map_err(|e| connect(format!("could not stop the transfer: {e}")))?;
            return Ok(Raw { bytes, capped });
        }
        let status = child.wait().await.map_err(|e| connect(format!("lost track of ssh: {e}")))?;
        let stderr = errors.await.ok().and_then(Result::ok).unwrap_or_default();
        // A failed write means the remote staged a partial body; only a complete one is trusted.
        let wrote = matches!(writer.await, Ok(Ok(())));
        let host = self.host();
        match status.code() {
            Some(0) if wrote => Ok(Raw { bytes, capped }),
            Some(0) => Err(connect(format!("the request could not be sent completely to {host}; the response was discarded"))),
            Some(SSH_FAILED) => Err(connect(classify(self.ssh.target(), self.ssh.effective(), &stderr).to_string())),
            Some(50) => Err(EgressError::Unsupported(format!(
                "{host} has no `curl`, which the ssh world uses to fetch from the remote's network; nothing was sent. Install curl on {host}."
            ))),
            Some(51 | 52) => Err(connect(format!("could not stage the request on {host} ({}); nothing was sent", stderr.trim()))),
            Some(28) => Err(EgressError::Timeout { ms: timeout_ms }),
            code => Err(connect(format!("curl on {host} failed (exit {code:?}: {})", stderr.trim()))),
        }
    }
}

struct Raw {
    bytes: Vec<u8>,
    capped: bool,
}

impl Egress for SshEgress {
    fn fetch(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, EgressError>> {
        Box::pin(async move {
            let ms = req.limits.timeout_ms;
            let deadline = Instant::now() + Duration::from_millis(ms);
            match tokio::time::timeout(Duration::from_millis(ms), self.exchange(req, deadline)).await {
                Ok(r) => r,
                Err(_) => Err(EgressError::Timeout { ms }),
            }
        })
    }

    fn enforced_by(&self) -> &str {
        ENFORCED_BY
    }
}

fn check_hop(req: &HttpRequest, url: &str) -> Result<ParsedUrl, EgressError> {
    match aho_egress::parse_url(url) {
        Err(why) => Err(EgressError::InvalidUrl { url: url.to_string(), why }),
        Ok(_) => req.hosts.check_url(url).map_err(EgressError::Denied),
    }
}

fn authority(u: &ParsedUrl) -> (String, u16) {
    let default = if u.scheme == "https" { 443 } else { 80 };
    (u.host.clone(), u.port.unwrap_or(default))
}

/// A curl config string: `\` and `"` escaped inside double quotes (curl's config syntax).
fn cq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn curl_config(url: &str, method: Method, headers: &[(&str, &str)], left: Duration) -> Result<String, EgressError> {
    let mut c = String::new();
    c.push_str(&format!("url = {}\n", cq(url)));
    c.push_str("globoff\nsilent\nshow-error\nproto = \"=http,https\"\n");
    c.push_str(&format!("max-time = {:.3}\n", left.as_secs_f64().max(0.001)));
    c.push_str(&format!("user-agent = {}\n", cq(concat!("aho/", env!("CARGO_PKG_VERSION")))));
    match method {
        Method::Head => c.push_str("head\n"),
        Method::Post => c.push_str("request = \"POST\"\ndump-header = \"-\"\n"),
        Method::Get => c.push_str("dump-header = \"-\"\n"),
    }
    // curl adds `Expect: 100-continue` and a form content type to bodies; the host world does neither.
    c.push_str("header = \"Expect:\"\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
        c.push_str("header = \"Content-Type:\"\n");
    }
    for (k, v) in headers {
        if k.is_empty() || k.contains([':', '\r', '\n']) || v.contains(['\r', '\n']) {
            return Err(EgressError::Unsupported(format!("header `{k}` has a line break, colon or empty name; nothing was sent.")));
        }
        c.push_str(&format!("header = {}\n", cq(&format!("{k}: {v}"))));
    }
    Ok(c)
}

/// One final (non-1xx) response as curl's `-D -` printed it.
struct Framed<'a> {
    status: u16,
    /// Lower-cased names, in arrival order.
    headers: Vec<(String, String)>,
    body: &'a [u8],
}

/// Frames curl's `-D -` output, skipping interim 1xx blocks.
fn parse_response(raw: &[u8]) -> Option<Framed<'_>> {
    let mut rest = raw;
    loop {
        let (block, after) = split_header_block(rest)?;
        let text = String::from_utf8_lossy(block);
        let mut lines = text.lines();
        let status: u16 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
        if (100..200).contains(&status) {
            rest = after;
            continue;
        }
        let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
        return Some(Framed { status, headers, body: after });
    }
}

fn split_header_block(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let crlf = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, 4));
    let lf = raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let (i, len) = match (crlf, lf) {
        (Some(a), Some(b)) => {
            if a.0 <= b.0 {
                a
            } else {
                b
            }
        }
        (a, b) => a.or(b)?,
    };
    Some((&raw[..i], &raw[i + len..]))
}

/// Resolves a `Location` against the URL that sent it (absolute, `//host`, `/path`, `?query`, relative).
fn join_location(base: &str, loc: &str) -> Result<String, String> {
    let loc = loc.split('#').next().unwrap_or("");
    let lower = loc.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(loc.to_string());
    }
    let (scheme, rest) = base.split_once("://").ok_or_else(|| format!("the redirect base `{base}` has no scheme"))?;
    if let Some(net) = loc.strip_prefix("//") {
        return Ok(format!("{scheme}://{net}"));
    }
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let origin = format!("{scheme}://{}", &rest[..auth_end]);
    let path_q = rest[auth_end..].split('#').next().unwrap_or("");
    let path = path_q.split('?').next().unwrap_or("");
    if loc.starts_with('/') {
        return Ok(format!("{origin}{loc}"));
    }
    if loc.starts_with('?') {
        let path = if path.is_empty() { "/" } else { path };
        return Ok(format!("{origin}{path}{loc}"));
    }
    if loc.is_empty() {
        return Ok(format!("{origin}{path_q}"));
    }
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    Ok(format!("{origin}{dir}{loc}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_resolve_like_a_browser() {
        let b = "https://a.example/x/y?q=1";
        assert_eq!(join_location(b, "http://b.example/z").unwrap(), "http://b.example/z");
        assert_eq!(join_location(b, "//c.example/p").unwrap(), "https://c.example/p");
        assert_eq!(join_location(b, "/root").unwrap(), "https://a.example/root");
        assert_eq!(join_location(b, "sib").unwrap(), "https://a.example/x/sib");
        assert_eq!(join_location(b, "?r=2").unwrap(), "https://a.example/x/y?r=2");
        assert_eq!(join_location("http://h:8080", "next").unwrap(), "http://h:8080/next");
        assert_eq!(join_location(b, "/f#frag").unwrap(), "https://a.example/f");
    }

    #[test]
    fn curl_output_frames_status_headers_and_body_skipping_interim_responses() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 302 Found\r\nLocation: /next\r\nX-A: b: c\r\n\r\nbody\r\n\r\nmore";
        let Framed { status, headers, body } = parse_response(raw).unwrap();
        assert_eq!(status, 302);
        assert_eq!(headers, vec![("location".to_string(), "/next".to_string()), ("x-a".into(), "b: c".into())]);
        assert_eq!(body, b"body\r\n\r\nmore");
        assert!(parse_response(b"not http").is_none());
    }

    #[test]
    fn the_curl_config_escapes_values_and_refuses_header_injection() {
        let c = curl_config("http://h/p", Method::Post, &[("X-Q", "a \"b\" \\c")], Duration::from_millis(1500)).unwrap();
        assert!(c.contains("url = \"http://h/p\"\n"), "{c}");
        assert!(c.contains("header = \"X-Q: a \\\"b\\\" \\\\c\"\n"), "{c}");
        assert!(c.contains("request = \"POST\"") && c.contains("max-time = 1.500") && c.contains("header = \"Content-Type:\""), "{c}");
        assert!(matches!(
            curl_config("http://h/", Method::Get, &[("X", "a\r\nInjected: 1")], Duration::from_secs(1)),
            Err(EgressError::Unsupported(_))
        ));
        assert!(curl_config("http://h/", Method::Head, &[], Duration::from_secs(1)).unwrap().contains("head\n"));
    }
}
