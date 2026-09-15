//! web_fetch / web_search against local HTTP servers through world-host's real egress.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aho_cap::*;
use aho_config::{Loadout, ResolveInputs, Schema, resolve};
use aho_egress::EgressHandle;
use aho_log::CapState;
use aho_types::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tool_web::{FetchConfig, SearchConfig, WebFetch, WebSearch};
use world_host::{HostEgress, HostWorld};

struct Server {
    port: u16,
    hits: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
}

/// Local HTTP/1.1 server: `respond(request head)` returns the raw response bytes.
async fn serve(respond: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let respond = Arc::new(respond);
    let (h, s) = (hits.clone(), heads.clone());
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
                sock.write_all(&respond(&head)).await.ok();
                sock.shutdown().await.ok();
            });
        }
    });
    Server { port, hits, heads }
}

fn response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out =
        format!("HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len())
            .into_bytes();
    out.extend_from_slice(body);
    out
}

fn redirect(to: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
}

fn target(head: &str) -> String {
    head.split_whitespace().nth(1).unwrap_or("").to_string()
}

fn comp() -> Composition {
    Composition::new("t").provide::<HostWorld>().provide::<WebFetch>().provide::<WebSearch>()
}

fn loadout(sets: &[(&str, &str)]) -> Loadout {
    let mut s = Schema::default();
    for k in comp().config_keys() {
        s.add(k);
    }
    let mut i = ResolveInputs::default();
    i.sets = sets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    resolve(&i, &s, false).unwrap()
}

fn egress() -> EgressHandle {
    EgressHandle(Arc::new(HostEgress::new().unwrap()))
}

fn fetcher(sets: &[(&str, &str)]) -> WebFetch {
    WebFetch::new(egress(), FetchConfig::from_loadout(&loadout(sets)).unwrap())
}

fn searcher(sets: &[(&str, &str)]) -> WebSearch {
    let cfg = SearchConfig::from_loadout(&loadout(sets)).unwrap();
    WebSearch::new(egress(), cfg.backend.clone().expect("a backend is configured"), cfg)
}

fn view(tool: &str, args: Value) -> ExecView {
    ExecView {
        call: CallId("c1".into()),
        tool: tool.into(),
        class: Consequence::NetworkEgress,
        args,
        policy: PolicyView {
            epoch: Epoch(1),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: String::new(),
            home: String::new(),
        },
        by: Principal::Agent { id: AgentId(0), turn: TurnId(1) },
    }
}

fn text_of(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(|c| c.as_text()).collect::<Vec<_>>().join("\n"),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn err_of(o: &ExecOutcome) -> (String, String) {
    match o {
        ExecOutcome::Err { code, reason } => (code.clone(), reason.clone()),
        other => panic!("expected Err, got {other:?}"),
    }
}

const PAGE: &str = r#"<!doctype html><html><head><title>Aho &mdash; Docs</title><script>alert("x")</script></head>
<body><nav><a href="/">Home</a> | <a href="/blog">Blog</a></nav>
<main><h1>Getting started</h1><p>Install with <code>cargo install aho</code>, then read the <a href="guide.html">guide</a>.</p>
<ul><li>fast</li><li>honest</li></ul></main><footer>legal</footer></body></html>"#;

#[tokio::test]
async fn fetch_turns_html_into_readable_markdown() {
    let srv = serve(|_| response("200 OK", "text/html; charset=utf-8", PAGE.as_bytes())).await;
    let out = fetcher(&[]).fetch(&json!({"url": format!("http://127.0.0.1:{}/docs/", srv.port)}), &CancelToken::new()).await;
    let text = text_of(&out);
    for want in [
        "Title: Aho — Docs",
        "Content-Type: text/html",
        "# Getting started",
        "Install with `cargo install aho`, then read the [guide](http://127.0.0.1:",
        "/docs/guide.html)",
        "- fast\n- honest",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(!text.contains("alert") && !text.contains("Blog") && !text.contains("legal"), "{text}");
    let ExecOutcome::Done { value, .. } = &out else { unreachable!() };
    assert_eq!((value["status"].as_u64(), value["kind"].as_str(), value["truncated"].as_bool()), (Some(200), Some("html"), Some(false)));

    let raw = fetcher(&[]).fetch(&json!({"url": format!("http://127.0.0.1:{}/", srv.port), "raw": true}), &CancelToken::new()).await;
    assert!(text_of(&raw).contains("<main><h1>Getting started</h1>"));
}

#[tokio::test]
async fn fetch_is_content_type_aware() {
    let srv = serve(|head| match target(head).as_str() {
        "/latin1" => response("200 OK", "text/plain; charset=iso-8859-1", b"caf\xe9"),
        "/json" => response("200 OK", "application/json", br#"{"a":1}"#),
        "/png" => response("200 OK", "image/png", b"\x89PNG\r\n\x1a\nrest"),
        "/pdf" => response("200 OK", "application/pdf", b"%PDF-1.7 binary"),
        _ => response("404 Not Found", "text/html", b"<h1>Nope</h1><p>no such page</p>"),
    })
    .await;
    let f = fetcher(&[]);
    let at = |p: &str| json!({"url": format!("http://127.0.0.1:{}{p}", srv.port)});
    let c = CancelToken::new();
    assert!(text_of(&f.fetch(&at("/latin1"), &c).await).ends_with("café"));
    assert!(text_of(&f.fetch(&at("/json"), &c).await).contains(r#"{"a":1}"#));
    let ExecOutcome::Done { content, .. } = f.fetch(&at("/png"), &c).await else { panic!() };
    assert!(matches!(&content[1], ContentBlock::Image { media_type, data } if media_type == "image/png" && data.starts_with(b"\x89PNG")));
    let pdf = text_of(&f.fetch(&at("/pdf"), &c).await);
    assert!(pdf.contains("[binary content: application/pdf, 15 bytes read") && !pdf.contains("%PDF"), "{pdf}");
    let (code, reason) = err_of(&f.fetch(&at("/missing"), &c).await);
    assert_eq!(code, "HTTP_404");
    assert!(reason.contains("HTTP 404") && reason.contains("# Nope") && reason.contains("no such page"), "{reason}");
}

#[tokio::test]
async fn fetch_stops_at_the_redirect_cap() {
    let srv = serve(|head| {
        let n: u32 = target(head).trim_start_matches("/r").parse().unwrap_or(0);
        if n == 0 { response("200 OK", "text/plain", b"arrived") } else { redirect(&format!("/r{}", n - 1)) }
    })
    .await;
    let f = fetcher(&[("web.fetch.max_redirects", "2")]);
    let ok = f.fetch(&json!({"url": format!("http://127.0.0.1:{}/r2", srv.port)}), &CancelToken::new()).await;
    let text = text_of(&ok);
    assert!(text.contains("Redirects: 2") && text.ends_with("arrived"), "{text}");
    let before = srv.hits.load(Ordering::SeqCst);
    let (code, reason) = err_of(&f.fetch(&json!({"url": format!("http://127.0.0.1:{}/r3", srv.port)}), &CancelToken::new()).await);
    assert_eq!(code, "TOO_MANY_REDIRECTS");
    assert!(reason.contains("stopped after 2 redirects") && reason.contains("/r0"), "{reason}");
    assert_eq!(srv.hits.load(Ordering::SeqCst) - before, 3, "the hop past the cap is not requested");
}

#[tokio::test]
async fn fetch_reads_at_most_the_size_cap() {
    let body = "a".repeat(10_000);
    let srv = serve(move |_| response("200 OK", "text/plain", body.as_bytes())).await;
    let url = format!("http://127.0.0.1:{}/big", srv.port);
    let out = fetcher(&[("web.fetch.max_bytes", "1000")]).fetch(&json!({"url": url}), &CancelToken::new()).await;
    let ExecOutcome::Done { value, content, .. } = &out else { panic!("{out:?}") };
    assert_eq!((value["bytes"].as_u64(), value["truncated"].as_bool()), (Some(1000), Some(true)));
    assert!(content[0].as_text().unwrap().contains("only the first 1000 bytes were read (web.fetch.max_bytes)"));
    assert_eq!(content[1].as_text().unwrap().len(), 1000);
    // A call may ask for less, never more.
    let out = fetcher(&[("web.fetch.max_bytes", "1000")]).fetch(&json!({"url": url, "max_bytes": 50_000}), &CancelToken::new()).await;
    let ExecOutcome::Done { value, .. } = &out else { panic!() };
    assert_eq!(value["bytes"].as_u64(), Some(1000));
    let out = fetcher(&[]).fetch(&json!({"url": url, "max_bytes": 10}), &CancelToken::new()).await;
    let ExecOutcome::Done { value, .. } = &out else { panic!() };
    assert_eq!(value["bytes"].as_u64(), Some(10));
}

#[tokio::test]
async fn the_deny_list_refuses_before_any_connection() {
    let srv = serve(|_| response("200 OK", "text/plain", b"should never be served")).await;
    let url = format!("http://127.0.0.1:{}/", srv.port);
    let f = fetcher(&[("egress.deny", r#"["127.0.0.*"]"#)]);

    // Decision chain step 4: the guard refuses the call, naming the key to change.
    let why = f.guard(&view("web_fetch", json!({"url": url}))).expect("the guard refuses a denied host");
    assert!(why.contains("`127.0.0.*` in `egress.deny`") && why.contains("remove `127.0.0.*` from `egress.deny`"), "{why}");
    assert_eq!(f.guard(&view("read", json!({"url": url}))), None, "other tools are not this guard's business");

    // Even dispatched directly, the body refuses before egress.
    let (code, reason) = err_of(&f.fetch(&json!({"url": url}), &CancelToken::new()).await);
    assert_eq!(code, "EGRESS_DENIED");
    assert!(reason.contains("egress.deny"), "{reason}");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(srv.hits.load(Ordering::SeqCst), 0, "the server saw zero connections");
}

#[tokio::test]
async fn the_allow_list_admits_only_its_hosts_and_redirects_are_rechecked() {
    let other = serve(|_| response("200 OK", "text/plain", b"from the other host")).await;
    let other_port = other.port;
    let srv = serve(move |head| match target(head).as_str() {
        "/away" => redirect(&format!("http://127.0.0.1:{other_port}/")),
        _ => response("200 OK", "text/plain", b"allowed host"),
    })
    .await;
    let f = fetcher(&[("egress.allow", r#"["localhost"]"#)]);
    let ok = f.fetch(&json!({"url": format!("http://localhost:{}/", srv.port)}), &CancelToken::new()).await;
    assert!(text_of(&ok).ends_with("allowed host"));
    assert_eq!(f.guard(&view("web_fetch", json!({"url": format!("http://LOCALHOST:{}/", srv.port)}))), None);

    let why = f.guard(&view("web_fetch", json!({"url": format!("http://127.0.0.1:{}/", srv.port)}))).unwrap();
    assert!(why.contains("not in `egress.allow` (localhost)") && why.contains("add `127.0.0.1`"), "{why}");

    // An allowed first hop redirecting to a host outside the list is stopped at that hop.
    let (code, reason) = err_of(&f.fetch(&json!({"url": format!("http://localhost:{}/away", srv.port)}), &CancelToken::new()).await);
    assert_eq!(code, "EGRESS_DENIED");
    assert!(reason.contains("egress.allow"), "{reason}");
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
}

/// Shape of `GET /res/v1/web/search` from the Brave Search API documentation (fields trimmed).
const BRAVE_BODY: &str = r#"{
  "type": "search",
  "query": { "original": "rust async", "more_results_available": true },
  "mixed": { "type": "mixed", "main": [{ "type": "web", "index": 0, "all": false }] },
  "web": {
    "type": "search",
    "results": [
      { "type": "search_result", "title": "Asynchronous Programming in <strong>Rust</strong>", "url": "https://rust-lang.github.io/async-book/",
        "description": "The <strong>async</strong> book &amp; guide.", "age": "2 days ago", "language": "en", "family_friendly": true,
        "profile": { "name": "rust-lang.github.io" }, "extra_snippets": ["more"] },
      { "type": "search_result", "title": "Tokio", "url": "https://tokio.rs/", "description": "An asynchronous runtime." },
      { "type": "search_result", "title": "Third", "url": "https://example.com/3", "description": "cut by count" }
    ],
    "family_friendly": true
  }
}"#;

#[tokio::test]
async fn brave_search_speaks_the_documented_api() {
    let srv = serve(|head| {
        if head.contains("x-subscription-token: test-key") {
            response("200 OK", "application/json", BRAVE_BODY.as_bytes())
        } else {
            response("401 Unauthorized", "application/json", br#"{"type":"ErrorResponse"}"#)
        }
    })
    .await;
    let endpoint = format!("http://127.0.0.1:{}/res/v1/web/search", srv.port);
    let s = searcher(&[("web.search.brave_url", &endpoint), ("web.search.brave.key", "test-key")]);
    assert_eq!(s.backend().name(), "brave");
    let out = s.search(&json!({"query": "rust async", "count": 2}), &CancelToken::new()).await;
    let ExecOutcome::Done { value, content, .. } = &out else { panic!("{out:?}") };
    assert_eq!(value["backend"], "brave");
    assert_eq!(
        value["results"],
        json!([
            {"title": "Asynchronous Programming in Rust", "url": "https://rust-lang.github.io/async-book/", "snippet": "The async book & guide."},
            {"title": "Tokio", "url": "https://tokio.rs/", "snippet": "An asynchronous runtime."}
        ])
    );
    assert!(content[0].as_text().unwrap().contains("1. Asynchronous Programming in Rust\n   https://rust-lang.github.io/async-book/"));
    let head = srv.heads.lock().await[0].clone();
    assert!(target(&head) == "/res/v1/web/search?q=rust+async&count=2", "{head}");
    assert!(head.contains("accept: application/json"), "{head}");

    let bad = searcher(&[("web.search.brave_url", &endpoint), ("web.search.brave.key", "wrong")]);
    let (code, reason) = err_of(&bad.search(&json!({"query": "x"}), &CancelToken::new()).await);
    assert_eq!(code, "SEARCH_FAILED");
    assert!(reason.contains("refused the API key (HTTP 401)") && reason.contains("aho credential set brave"), "{reason}");

    // `cred:brave` indirects through the named credential.
    let via = searcher(&[("web.search.brave_url", &endpoint), ("web.search.brave.key", "cred:brave"), ("credentials.brave", "test-key")]);
    assert!(matches!(via.search(&json!({"query": "q"}), &CancelToken::new()).await, ExecOutcome::Done { .. }));
}

/// Shape of SearXNG's `/search?format=json` (fields trimmed).
const SEARXNG_BODY: &str = r#"{
  "query": "aho harness", "number_of_results": 0,
  "results": [
    { "url": "https://github.com/x/aho", "title": "x/aho", "content": "A modular <b>agent</b> harness.", "engine": "duckduckgo",
      "parsed_url": ["https", "github.com", "/x/aho", "", "", ""], "engines": ["duckduckgo", "brave"], "positions": [1, 2], "score": 4.0, "category": "general" }
  ],
  "answers": [], "corrections": [], "infoboxes": [], "suggestions": ["aho rust"], "unresponsive_engines": []
}"#;

#[tokio::test]
async fn searxng_search_and_its_json_format_error() {
    let srv = serve(|head| {
        if target(head).starts_with("/searx/search?q=aho+harness&format=json") {
            response("200 OK", "application/json", SEARXNG_BODY.as_bytes())
        } else {
            response("403 Forbidden", "text/html", b"<h1>403 Forbidden</h1>")
        }
    })
    .await;
    let s = searcher(&[("web.search.searxng_url", &format!("http://127.0.0.1:{}/searx/", srv.port))]);
    assert_eq!(s.backend().name(), "searxng");
    let out = s.search(&json!({"query": "aho harness"}), &CancelToken::new()).await;
    let ExecOutcome::Done { value, .. } = &out else { panic!("{out:?}") };
    assert_eq!(value["results"], json!([{"title": "x/aho", "url": "https://github.com/x/aho", "snippet": "A modular agent harness."}]));
    let (_, reason) = err_of(&s.search(&json!({"query": "other"}), &CancelToken::new()).await);
    assert!(reason.contains("HTTP 403") && reason.contains("Enable `json` under `search.formats`"), "{reason}");

    // The search endpoint is under the same egress list.
    let denied = searcher(&[("web.search.searxng_url", &format!("http://127.0.0.1:{}/searx/", srv.port)), ("egress.deny", r#"["127.*"]"#)]);
    let before = srv.hits.load(Ordering::SeqCst);
    assert!(denied.guard(&view("web_search", json!({"query": "q"}))).unwrap().contains("egress.deny"));
    assert_eq!(err_of(&denied.search(&json!({"query": "q"}), &CancelToken::new()).await).0, "EGRESS_DENIED");
    assert_eq!(srv.hits.load(Ordering::SeqCst), before);
}

fn plan_for(sets: &[(&str, &str)]) -> (Plan, Loadout) {
    let ws = std::env::temp_dir();
    let mut all: Vec<(&str, String)> = sets.iter().map(|(k, v)| (*k, v.to_string())).collect();
    all.push(("workspace.root", ws.to_string_lossy().into_owned()));
    let owned: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let l = loadout(&owned);
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let p = plan(&comp().manifests(), &l, &env, &PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::ALL }, vec![]).unwrap();
    (p, l)
}

#[test]
fn search_is_locked_with_a_fix_until_a_backend_exists_while_fetch_stays_active() {
    let (p, l) = plan_for(&[]);
    assert_eq!(p.row("tool-web-fetch").unwrap().state, CapState::Active);
    let row = p.row("tool-web-search").unwrap();
    let CapState::Locked { missing } = &row.state else { panic!("{:?}", row.state) };
    assert_eq!(missing, &vec!["AnyOf(Credential(brave) | Setting(web.search.searxng_url Set))".to_string()]);
    assert_eq!(row.missing[0].fix.as_deref(), Some("aho credential set brave"));
    assert_eq!(row.ceiling, ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::NetworkEgress]));

    let (reg, broken) = comp().start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    assert!(reg.tool("web_fetch").is_some() && reg.tool("web_search").is_none());
    assert_eq!(reg.guards.len(), 1);

    let (p, l) = plan_for(&[("web.search.searxng_url", "https://searx.example.org")]);
    assert_eq!(p.row("tool-web-search").unwrap().state, CapState::Active);
    let (reg, broken) = comp().start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    assert!(reg.tool("web_search").is_some());

    let (p, _) = plan_for(&[("web.search.brave.key", "k")]);
    assert_eq!(p.row("tool-web-search").unwrap().state, CapState::Active, "a Brave key unlocks it too");
}

#[tokio::test]
async fn the_registered_tool_runs_through_the_composition() {
    let srv = serve(|_| response("200 OK", "text/html", PAGE.as_bytes())).await;
    let (p, l) = plan_for(&[]);
    let (reg, _) = comp().start(&p, &l);
    let (decl, exec) = reg.tool("web_fetch").unwrap();
    assert_eq!((decl.class, decl.max_inline), (Consequence::NetworkEgress, Some(tool_web::MAX_INLINE)));
    assert!(matches!((decl.check)(&json!({"url": "file:///etc/passwd"}), &view("web_fetch", json!({})).policy), Some(Decision::Deny(_))));
    let req = ExecRequest {
        view: view("web_fetch", json!({"url": format!("http://127.0.0.1:{}/", srv.port)})),
        cancel: CancelToken::new(),
        progress: None,
    };
    assert!(text_of(&exec(req).await).contains("# Getting started"));
}
