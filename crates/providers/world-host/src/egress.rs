//! The host network as the world's `Egress`: `enforced by: policy only`. The operator's
//! host list is checked on every hop against the host reqwest's own URL parser produced, before
//! connecting; redirects are followed here (never by reqwest) so each one is counted and checked.

use std::time::Duration;

use aho_egress::{Egress, EgressError, HttpRequest, HttpResponse, Method, is_redirect, redirect_method};
use aho_types::BoxFuture;
use reqwest::Url;

use crate::ENFORCED_BY;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct HostEgress {
    client: reqwest::Client,
}

impl HostEgress {
    #[allow(clippy::disallowed_methods, reason = "authority: world-host-egress-client")]
    pub fn new() -> Result<HostEgress, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("aho/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("could not set up the host HTTP client ({e}); web tools stay unavailable until this is fixed"))?;
        Ok(HostEgress { client })
    }

    #[allow(clippy::disallowed_methods, reason = "authority: world-host-egress-exchange")]
    async fn exchange(&self, req: HttpRequest) -> Result<HttpResponse, EgressError> {
        let mut url = Url::parse(&req.url).map_err(|e| EgressError::InvalidUrl { url: req.url.clone(), why: e.to_string() })?;
        let origin = authority(&url);
        let mut method = req.method;
        let mut body = req.body.clone();
        let mut redirects = 0u32;
        loop {
            check_hop(&req, &url)?;
            let mut b = self.client.request(reqwest_method(method), url.clone());
            let same_origin = authority(&url) == origin;
            for h in req.headers.iter().filter(|h| same_origin || !h.secret) {
                b = b.header(h.name.as_str(), h.value.as_str());
            }
            if let Some(bytes) = &body {
                b = b.body(bytes.clone());
            }
            let mut resp = b.send().await.map_err(|e| connect_error(&url, &e))?;
            let status = resp.status().as_u16();
            let location = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string);
            if let (true, Some(loc)) = (is_redirect(status), location) {
                let next = url.join(&loc).map_err(|e| EgressError::InvalidUrl { url: loc.clone(), why: e.to_string() })?;
                if redirects >= req.limits.max_redirects {
                    return Err(EgressError::TooManyRedirects { limit: req.limits.max_redirects, url: next.to_string() });
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
            let headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect();
            let (mut data, mut truncated) = (Vec::new(), false);
            if method != Method::Head {
                let max = req.limits.max_body_bytes;
                while let Some(chunk) = resp.chunk().await.map_err(|e| connect_error(&url, &e))? {
                    let room = max.saturating_sub(data.len());
                    if chunk.len() > room {
                        data.extend_from_slice(&chunk[..room]);
                        truncated = true;
                        break;
                    }
                    data.extend_from_slice(&chunk);
                }
            }
            return Ok(HttpResponse { status, final_url: url.to_string(), headers, body: data, truncated, redirects });
        }
    }
}

impl Egress for HostEgress {
    fn fetch(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, EgressError>> {
        Box::pin(async move {
            let ms = req.limits.timeout_ms;
            match tokio::time::timeout(Duration::from_millis(ms), self.exchange(req)).await {
                Ok(r) => r,
                Err(_) => Err(EgressError::Timeout { ms }),
            }
        })
    }

    fn enforced_by(&self) -> &str {
        ENFORCED_BY
    }
}

fn authority(url: &Url) -> (String, Option<u16>) {
    (url.host_str().unwrap_or("").to_ascii_lowercase(), url.port_or_known_default())
}

fn check_hop(req: &HttpRequest, url: &Url) -> Result<(), EgressError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(EgressError::InvalidUrl {
            url: url.to_string(),
            why: format!("only http and https are fetched, not `{}`", url.scheme()),
        });
    }
    let host = url.host_str().ok_or_else(|| EgressError::InvalidUrl { url: url.to_string(), why: "it has no host".into() })?;
    req.hosts.check_host(host).map_err(EgressError::Denied)
}

fn reqwest_method(m: Method) -> reqwest::Method {
    match m {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Head => reqwest::Method::HEAD,
    }
}

fn connect_error(url: &Url, e: &reqwest::Error) -> EgressError {
    let why = if e.is_connect() {
        format!("the connection failed ({e}); check the address and your network")
    } else if e.is_timeout() {
        format!("the connection timed out ({e})")
    } else {
        e.to_string()
    };
    EgressError::Connect { url: url.to_string(), why }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aho_egress::{HostList, Limits};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// A local HTTP/1.1 server: `respond(request head)` returns the raw response. Counts connections.
    async fn serve(
        respond: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static,
    ) -> (u16, Arc<AtomicUsize>, Arc<tokio::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let respond = Arc::new(respond);
        let (h, s) = (hits.clone(), seen.clone());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                h.fetch_add(1, Ordering::SeqCst);
                let (respond, s) = (respond.clone(), s.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    s.lock().await.push(head.clone());
                    let out = respond(&head);
                    sock.write_all(&out).await.ok();
                    sock.shutdown().await.ok();
                });
            }
        });
        (port, hits, seen)
    }

    fn ok(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
            .into_bytes()
    }

    fn redirect(to: &str) -> Vec<u8> {
        format!("HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
    }

    fn path(head: &str) -> String {
        head.split_whitespace().nth(1).unwrap_or("").to_string()
    }

    #[tokio::test]
    async fn follows_redirects_up_to_the_cap_and_no_further() {
        let (port, hits, _) = serve(|head| {
            let n: u32 = path(head).trim_start_matches("/r").parse().unwrap_or(0);
            if n == 0 { ok("landed") } else { redirect(&format!("/r{}", n - 1)) }
        })
        .await;
        let e = HostEgress::new().unwrap();
        let limits = Limits { max_redirects: 3, ..Limits::default() };
        let r = e.fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/r3")).limits(limits)).await.unwrap();
        assert_eq!((r.status, r.redirects, r.body.as_slice()), (200, 3, b"landed".as_slice()));
        assert!(r.final_url.ends_with("/r0"), "{}", r.final_url);
        let before = hits.load(Ordering::SeqCst);
        let err = e.fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/r4")).limits(limits)).await.unwrap_err();
        assert!(matches!(&err, EgressError::TooManyRedirects { limit: 3, url } if url.ends_with("/r0")), "{err:?}");
        assert_eq!(hits.load(Ordering::SeqCst) - before, 4, "the hop past the cap is never requested");
    }

    #[tokio::test]
    async fn body_reading_stops_at_the_cap() {
        let big = "x".repeat(100_000);
        let (port, _, _) = serve(move |_| ok(&big)).await;
        let e = HostEgress::new().unwrap();
        let limits = Limits { max_body_bytes: 1000, ..Limits::default() };
        let r = e.fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/")).limits(limits)).await.unwrap();
        assert_eq!((r.body.len(), r.truncated), (1000, true));
        let exact = Limits { max_body_bytes: 100_000, ..Limits::default() };
        let r = e.fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/")).limits(exact)).await.unwrap();
        assert_eq!((r.body.len(), r.truncated), (100_000, false));
    }

    #[tokio::test]
    async fn every_hop_is_checked_and_secrets_stay_with_their_host() {
        let (other, other_hits, other_seen) = serve(|_| ok("other")).await;
        let (port, _, seen) = serve(move |head| match path(head).as_str() {
            "/to-denied" => redirect(&format!("http://localhost:{other}/")),
            _ => ok("home"),
        })
        .await;
        let e = HostEgress::new().unwrap();
        let deny = HostList::new(vec![], vec!["localhost".into()]).unwrap();
        let err = e
            .fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/to-denied")).hosts(deny.clone()).secret_header("x-token", "s3cret"))
            .await
            .unwrap_err();
        assert!(matches!(&err, EgressError::Denied(m) if m.contains("egress.deny")), "{err:?}");
        assert_eq!(other_hits.load(Ordering::SeqCst), 0, "a denied redirect target is never contacted");

        // Without the deny entry the redirect is followed, but the secret header does not cross hosts.
        let r = e
            .fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/to-denied")).secret_header("x-token", "s3cret").header("x-plain", "p"))
            .await
            .unwrap();
        assert_eq!(r.body, b"other");
        assert!(seen.lock().await.iter().all(|h| h.contains("x-token: s3cret")));
        let crossed = other_seen.lock().await.join("\n");
        assert!(!crossed.contains("s3cret") && crossed.contains("x-plain: p"), "{crossed}");

        let first = HostList::new(vec!["example.org".into()], vec![]).unwrap();
        let err = e.fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/")).hosts(first)).await.unwrap_err();
        assert!(matches!(&err, EgressError::Denied(m) if m.contains("egress.allow")), "{err:?}");
    }

    #[tokio::test]
    async fn a_slow_server_hits_the_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let e = HostEgress::new().unwrap();
        let limits = Limits { timeout_ms: 200, ..Limits::default() };
        let err = e.fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/")).limits(limits)).await.unwrap_err();
        assert_eq!(err, EgressError::Timeout { ms: 200 });
        assert_eq!(e.enforced_by(), "policy only");
    }
}
