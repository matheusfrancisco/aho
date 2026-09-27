//! Against a local Streamable HTTP MCP server (`tests/support`), through the composition.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_cap::tokio_like::Sender;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, Loadout, ResolveInputs, Schema, resolve};
use aho_mcp::*;
use aho_types::*;
use mcp_http::{HttpServer, Listening, McpHttp, McpHttpConfig, SERVERS_SERVICE};
use mcp_stdio::{McpConfig, McpStdio};
use serde_json::json;
use tool_mcp_resources::ToolMcpResources;

fn loadout(toml: &str) -> Loadout {
    let mut schema = Schema::default();
    core_keys().into_iter().chain(<McpConfig as Config>::keys()).chain(<McpHttpConfig as Config>::keys()).for_each(|k| schema.add(k));
    let inputs = ResolveInputs {
        files: vec![FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: toml.into() }],
        ..ResolveInputs::default()
    };
    resolve(&inputs, &schema, false).unwrap()
}

fn http_only() -> Composition {
    Composition::new("t").provide::<McpHttp>().consume::<ToolMcpResources>()
}

fn start_with(comp: &Composition, toml: &str) -> Registry {
    let l = loadout(toml);
    let policy = PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::ALL };
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    reg
}

fn start(toml: &str) -> Registry {
    start_with(&http_only(), toml)
}

fn row(name: &str, url: &str, extra: &str) -> String {
    format!("[mcp.servers.{name}]\nurl = \"{url}\"\n{extra}\n")
}

fn status(reg: &Registry) -> Arc<Vec<ServerStatus>> {
    reg.service::<Arc<Vec<ServerStatus>>>(STATUS_SERVICE).unwrap()
}

fn row_of(reg: &Registry, name: &str) -> ServerStatus {
    status(reg).iter().find(|s| s.name == name).cloned().unwrap_or_else(|| panic!("no status row for {name}"))
}

fn server(reg: &Registry, name: &str) -> HttpServer {
    let all = reg.service::<Arc<Vec<HttpServer>>>(SERVERS_SERVICE).unwrap();
    all.iter().find(|s| s.name() == name).cloned().unwrap()
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

async fn call_full(reg: &Registry, name: &str, args: Value, cancel: CancelToken, progress: Option<Sender>) -> ExecOutcome {
    let exec = match reg.tool(name) {
        Some((_, exec)) => exec,
        None => reg.dynamic_tools.iter().find(|(_, s, _)| s.name == name).map(|(_, _, e)| e.clone()).expect(name),
    };
    exec(ExecRequest { view: view(name, args), cancel, progress }).await
}

async fn call(reg: &Registry, name: &str, args: Value) -> ExecOutcome {
    call_full(reg, name, args, CancelToken::new(), None).await
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n"),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn err(o: ExecOutcome) -> (String, String) {
    match o {
        ExecOutcome::Err { code, reason } => (code, reason),
        other => panic!("expected Err, got {other:?}"),
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const CLASSIFIED: &str = "class = \"NetworkEgress\"";

#[tokio::test(flavor = "multi_thread")]
async fn handshake_keeps_the_session_and_lists_every_page() {
    let mock = support::start(None, true);
    let reg = start(&row("fx", &mock.url, CLASSIFIED));

    let seen = mock.st.seen();
    let init = &seen[0];
    assert_eq!(init.rpc.as_deref(), Some("initialize"));
    assert_eq!((init.session.as_deref(), init.version.as_deref()), (None, None), "initialize carries neither header");
    assert_eq!(init.accept.as_deref(), Some("application/json, text/event-stream"));
    assert_eq!(init.body["params"]["protocolVersion"], PROTOCOL_VERSION);
    let initialized = &seen[1];
    assert_eq!(initialized.rpc.as_deref(), Some("notifications/initialized"));
    assert_eq!((initialized.session.as_deref(), initialized.version.as_deref()), (Some("s-1"), Some("2025-06-18")));
    let lists = mock.st.rpc("tools/list");
    assert_eq!(lists.len(), 2, "two pages");
    assert_eq!(lists[1].body["params"]["cursor"], "4");
    assert!(lists.iter().all(|s| s.session.as_deref() == Some("s-1") && s.version.as_deref() == Some("2025-06-18")));

    let st = row_of(&reg, "fx");
    assert_eq!(st.state, ServerState::Ready);
    assert_eq!(st.tools.len(), 6);
    assert_eq!(reg.dynamic_tools.len(), 6);
    let echo = reg.dynamic_tools.iter().find(|(_, s, _)| s.name == "mcp__fx__echo").map(|(_, s, _)| s.clone()).unwrap();
    assert_eq!(echo.class, Consequence::NetworkEgress, "tools carry the server's class");
    assert_eq!(echo.description, "Echo text back");
    assert_eq!(echo.input_schema.0["required"], json!(["text"]));

    let fx = server(&reg, "fx");
    assert_eq!(fx.session_id().as_deref(), Some("s-1"));
    assert_eq!(fx.info().name, "mock");
    assert!(fx.info().tools_list_changed);
    let dir = reg.handle::<McpHandle>().unwrap();
    assert_eq!(dir.0.get("fx").unwrap().instructions(), Some("Use echo."));
    assert_eq!(fx.generation().unwrap().number, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_answer_as_json_or_as_an_event_stream_with_progress_and_server_requests() {
    let mock = support::start(None, true);
    let reg = start(&row("fx", &mock.url, CLASSIFIED));

    let echo = call(&reg, "mcp__fx__echo", json!({"text": "hi"})).await;
    assert_eq!(text(&echo), "hi");
    let ExecOutcome::Done { value, .. } = &echo else { panic!() };
    assert_eq!(value["content"][0]["text"], "hi");

    let progress = Sender::default();
    let streamed = call_full(&reg, "mcp__fx__progress", json!({}), CancelToken::new(), Some(progress.clone())).await;
    assert_eq!(text(&streamed), "done", "the response arrives after a comment, a priming event, a notification and a request");
    let lines = progress.drain();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let payload: Value = serde_json::from_str(lines[0].strip_prefix("AHO_PROGRESS ").unwrap()).unwrap();
    assert_eq!(payload, json!({"progress": 1, "total": 2, "message": "half"}));
    let sent = mock.st.rpc("tools/call");
    let request = sent.iter().find(|s| s.body["params"]["name"] == "progress").unwrap();
    assert_eq!(request.body["params"]["_meta"]["progressToken"], request.body["id"], "the progress token is the request id");
    wait_until("the answer to the server's ping", || {
        mock.st.http("POST").iter().any(|s| s.body["id"] == "srv-1" && s.body["result"] == json!({}) && s.session.as_deref() == Some("s-1"))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_changed_on_the_get_stream_relists_and_swaps_the_generation() {
    let mock = support::start(None, true);
    let reg = start(&row("fx", &mock.url, CLASSIFIED));
    let fx = server(&reg, "fx");
    wait_until("the notification stream", || fx.listening() == Listening::Connected && mock.st.listeners() == 1).await;
    let get = &mock.st.http("GET")[0];
    assert_eq!((get.accept.as_deref(), get.session.as_deref()), (Some("text/event-stream"), Some("s-1")));

    {
        let mut tools = support::lock(&mock.st.tools);
        tools.retain(|t| t["name"] != "boom");
        tools.push(support::tool("fresh", "Added later"));
    }
    assert_eq!(mock.st.push(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})), 1);
    wait_until("the re-listed generation", || fx.generation().map(|g| g.number) == Some(2)).await;
    let names: Vec<String> = fx.generation().unwrap().tools.iter().map(|(_, t)| t.raw_name.clone()).collect();
    assert_eq!(names, vec!["echo", "progress", "fail", "http500", "sleep", "fresh"]);
    assert_eq!(fx.tool_list_changes(), 1);
    assert_eq!(fx.relist_error(), None);
    assert_eq!(mock.st.rpc("tools/list").len(), 4, "two pages at start, two on the re-list");

    let (code, reason) = err(call(&reg, "mcp__fx__boom", json!({})).await);
    assert_eq!(code, "MCP_TOOL_GONE");
    assert!(reason.contains("no longer offered by MCP server `fx`") && reason.contains("generation 2"), "{reason}");
    assert!(mock.st.rpc("tools/call").is_empty(), "a withdrawn tool is never sent");
    assert_eq!(text(&call(&reg, "mcp__fx__echo", json!({"text": "still"})).await), "still");
    assert_eq!(reg.dynamic_tools.len(), 6, "added tools reach the model when the session restarts");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_session_is_renewed_once_and_the_request_retried_once() {
    let mock = support::start(None, false);
    let reg = start(&row("fx", &mock.url, CLASSIFIED));
    let fx = server(&reg, "fx");
    wait_until("the 405 on GET", || fx.listening() == Listening::Unsupported).await;

    mock.st.expire_sessions();
    assert_eq!(text(&call(&reg, "mcp__fx__echo", json!({"text": "again"})).await), "again");
    let calls = mock.st.rpc("tools/call");
    assert_eq!(calls.iter().map(|s| s.session.clone().unwrap()).collect::<Vec<_>>(), vec!["s-1", "s-2"], "404 on s-1, retried on s-2");
    let inits = mock.st.rpc("initialize");
    assert_eq!(inits.len(), 2);
    assert_eq!(inits[1].session, None, "the new session starts without an id");
    assert!(mock.st.rpc("notifications/initialized").iter().any(|s| s.session.as_deref() == Some("s-2")));
    assert_eq!(fx.session_id().as_deref(), Some("s-2"));

    // A server that forgets every session: one renewal attempt, then an error (no loop).
    mock.st.amnesia.store(true, std::sync::atomic::Ordering::SeqCst);
    let (code, reason) = err(call(&reg, "mcp__fx__echo", json!({"text": "x"})).await);
    assert_eq!(code, "MCP_TRANSPORT");
    assert!(reason.contains("404"), "{reason}");
    assert_eq!(mock.st.rpc("initialize").len(), 3, "exactly one more initialize");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bearer_credential_is_sent_and_refusals_name_the_fix() {
    let mock = support::start(Some("s3cret"), true);
    let toml = format!(
        "{}{}{}[credentials]\nsvc = \"s3cret\"\nwrong = \"guess\"\n",
        row("ok", &mock.url, &format!("{CLASSIFIED}\nbearer = \"cred:svc\"")),
        row("bad", &mock.url, &format!("{CLASSIFIED}\nbearer = \"cred:wrong\"")),
        row("missing", &mock.url, &format!("{CLASSIFIED}\nbearer = \"cred:nope\"")),
    );
    let reg = start(&toml);
    assert_eq!(row_of(&reg, "ok").state, ServerState::Ready);
    let ok = server(&reg, "ok");
    wait_until("the notification stream", || ok.listening() == Listening::Connected).await;
    assert_eq!(text(&call(&reg, "mcp__ok__echo", json!({"text": "authed"})).await), "authed");
    let with_session: Vec<_> = mock.st.seen().into_iter().filter(|s| s.session.is_some()).collect();
    assert!(with_session.iter().any(|s| s.http == "GET"));
    assert!(with_session.iter().all(|s| s.auth.as_deref() == Some("Bearer s3cret")), "every request of the session carries the token");

    let bad = row_of(&reg, "bad");
    let ServerState::Failed { error } = &bad.state else { panic!("{bad:?}") };
    assert!(error.contains("HTTP 401 Unauthorized") && error.contains("credential `wrong`"), "{error}");
    assert_eq!(bad.fix.as_deref(), Some("aho credential set wrong"));

    let missing = row_of(&reg, "missing");
    let ServerState::Failed { error } = &missing.state else { panic!("{missing:?}") };
    assert!(error.contains("names credential `nope`, which is not set"), "{error}");
    assert_eq!(missing.fix.as_deref(), Some("aho credential set nope"));
    assert_eq!(mock.st.rpc("initialize").len(), 2, "the server without its credential is never contacted");
}

#[tokio::test(flavor = "multi_thread")]
async fn server_errors_map_to_operator_messages() {
    let mock = support::start(None, true);
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let toml = format!(
        "{}{}{}",
        row("fx", &mock.url, CLASSIFIED),
        row("slow", &mock.url, &format!("{CLASSIFIED}\ntimeout_ms = 300")),
        row("down", &format!("http://{closed}/mcp"), CLASSIFIED)
    );
    let reg = start(&toml);

    let (code, reason) = err(call(&reg, "mcp__fx__boom", json!({})).await);
    assert_eq!((code.as_str(), reason.as_str()), ("MCP_RPC", "the MCP server answered with error -32000: kaboom"));
    let (code, reason) = err(call(&reg, "mcp__fx__fail", json!({})).await);
    assert_eq!((code.as_str(), reason.as_str()), ("MCP_TOOL_ERROR", "it broke"));
    let (code, reason) = err(call(&reg, "mcp__fx__http500", json!({})).await);
    assert_eq!(code, "MCP_TRANSPORT");
    assert!(reason.contains("answered `tools/call` with HTTP 500 Internal Server Error: database on fire"), "{reason}");

    let started = Instant::now();
    let (code, reason) = err(call(&reg, "mcp__slow__sleep", json!({})).await);
    assert_eq!(code, "MCP_TIMEOUT");
    assert!(reason.contains("did not answer `tools/call` within 300 ms"), "{reason}");
    assert!(started.elapsed() < Duration::from_millis(2000));
    wait_until("notifications/cancelled for the timeout", || {
        mock.st
            .rpc("notifications/cancelled")
            .iter()
            .any(|s| s.body["params"]["reason"] == "timed out" && s.body["params"]["requestId"].is_u64())
    })
    .await;

    let cancel = CancelToken::new();
    let firing = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        firing.cancel();
    });
    assert_eq!(call_full(&reg, "mcp__fx__sleep", json!({}), cancel, None).await, ExecOutcome::Cancelled);
    wait_until("notifications/cancelled for the cancel", || {
        mock.st.rpc("notifications/cancelled").iter().any(|s| s.body["params"]["reason"] == "cancelled by aho")
    })
    .await;

    let down = row_of(&reg, "down");
    let ServerState::Failed { error } = &down.state else { panic!("{down:?}") };
    assert!(error.contains("could not reach MCP server `down`"), "{error}");
    assert_eq!(down.fix.as_deref(), Some("aho explain mcp.servers.down.url"));
}

#[tokio::test(flavor = "multi_thread")]
async fn quarantine_egress_and_bad_rows_are_reported_and_dropping_deletes_sessions() {
    let mock = support::start(None, true);
    let toml = format!(
        "[egress]\ndeny = [\"localhost\"]\n{}{}{}{}{}{}",
        row("q", &mock.url, ""),
        row("fx", &mock.url, CLASSIFIED),
        row("denied", "http://localhost:9/mcp", CLASSIFIED),
        row("both", &mock.url, &format!("{CLASSIFIED}\ncmd = \"npx\"")),
        row("ftp", "ftp://example.com/mcp", CLASSIFIED),
        row("reserved", &mock.url, &format!("{CLASSIFIED}\nheaders = {{ Mcp-Session-Id = \"forged\" }}")),
    );
    let reg = start(&toml);
    let names: Vec<String> = status(&reg).iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, vec!["both", "denied", "ftp", "fx", "q", "reserved"], "one row per HTTP server, in name order");

    let q = row_of(&reg, "q");
    assert_eq!(q.state, ServerState::Quarantined { reason: "unclassified".into() });
    assert_eq!(q.tools.len(), 6, "probed: the card can list what it would expose");
    assert_eq!(q.fix.as_deref(), Some("aho set mcp.servers.q.class NetworkEgress"));
    assert!(reg.dynamic_tools.iter().all(|(_, s, _)| !s.name.starts_with("mcp__q__")));
    assert_eq!(reg.handle::<McpHandle>().unwrap().0.names(), vec!["fx"]);
    let q_session = mock.st.rpc("initialize").len();
    assert_eq!(q_session, 2, "q and fx were contacted; the refused rows never were");
    wait_until("the quarantined probe's DELETE", || mock.st.http("DELETE").len() == 1).await;

    let denied = row_of(&reg, "denied");
    let ServerState::Failed { error } = &denied.state else { panic!("{denied:?}") };
    assert!(error.contains("matches `localhost` in `egress.deny`"), "{error}");
    assert_eq!(denied.fix.as_deref(), Some("aho explain egress.deny"));
    let both = row_of(&reg, "both");
    assert!(matches!(&both.state, ServerState::Failed { error } if error.contains("are both set")), "{both:?}");
    let ftp = row_of(&reg, "ftp");
    assert!(matches!(&ftp.state, ServerState::Failed { error } if error.contains("only http and https")), "{ftp:?}");
    let reserved = row_of(&reg, "reserved");
    assert!(matches!(&reserved.state, ServerState::Failed { error } if error.contains("header aho manages itself")), "{reserved:?}");

    let fx_session = server(&reg, "fx").session_id().unwrap();
    drop(reg);
    wait_until("the DELETE on drop", || mock.st.http("DELETE").iter().any(|s| s.session.as_deref() == Some(fx_session.as_str()))).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_servers_join_the_stdio_directory_and_status() {
    let mock = support::start(None, true);
    // Inserted before stdio on purpose: the build follows the dependency order, not insertion.
    // Stdio servers run in the bound world (never a host fallback), so this composition binds one.
    let comp =
        Composition::new("t").provide::<McpHttp>().provide::<McpStdio>().provide::<world_host::HostWorld>().consume::<ToolMcpResources>();
    let ws = tempfile::tempdir().unwrap();
    let toml = format!(
        "[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n{}[mcp.servers.local]\ncmd = \"/nonexistent/aho-mcp-test\"\n{CLASSIFIED}\n",
        ws.path().display(),
        ws.path().join(".shadow").display(),
        row("web", &mock.url, CLASSIFIED)
    );
    let reg = {
        let mut schema = Schema::default();
        comp.config_keys().into_iter().chain(core_keys()).for_each(|k| schema.add(k));
        let inputs = ResolveInputs {
            files: vec![FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: toml }],
            ..ResolveInputs::default()
        };
        let l = resolve(&inputs, &schema, false).unwrap();
        let mut env = Env::empty(Os::current());
        env.binaries.insert("git".into());
        let policy = PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::ALL };
        let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
        let (reg, broken) = comp.start(&p, &l);
        assert!(broken.is_empty(), "{broken:?}");
        reg
    };
    let st = status(&reg);
    assert_eq!(st.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["local", "web"]);
    assert!(matches!(&st[0].state, ServerState::Failed { error } if error.contains("could not start")), "{:?}", st[0]);
    assert_eq!(st[1].state, ServerState::Ready, "the stdio provider's `url` refusal is replaced");
    assert_eq!(reg.handle::<McpHandle>().unwrap().0.names(), vec!["web"]);

    let listed = call(&reg, tool_mcp_resources::LIST, json!({})).await;
    assert_eq!(text(&listed), "server `web`:\n- mem://readme  readme (text/plain)");
    let read = call(&reg, tool_mcp_resources::READ, json!({"server": "web", "uri": "mem://readme"})).await;
    assert_eq!(text(&read), "mem://readme (text/plain)\nhello from http");
}

#[test]
fn specs_resolve_credentials_and_never_print_secrets() {
    let l = loadout(&format!(
        "{}[credentials]\ntok = \"sekrit-token\"\nhdr = \"sekrit-header\"\n",
        row(
            "gh",
            "https://api.example.com/mcp?key=sekrit-query",
            "bearer = \"cred:tok\"\nheaders = { X-Api-Key = \"cred:hdr\", X-Team = \"core\" }"
        )
    ));
    let cfg = McpHttpConfig::from_loadout(&l).unwrap();
    let spec = cfg.servers[0].spec.as_ref().unwrap();
    assert_eq!(spec.bearer.as_deref(), Some("sekrit-token"));
    assert_eq!(spec.credential.as_deref(), Some("tok"));
    assert!(spec.headers.contains(&("X-Api-Key".to_string(), "sekrit-header".to_string())));
    let map = spec.header_map().unwrap();
    assert_eq!(map.get("authorization").unwrap().to_str().unwrap(), "Bearer sekrit-token");
    let shown = format!("{spec:?}");
    assert!(!shown.contains("sekrit"), "{shown}");
    assert!(shown.contains("https://api.example.com/mcp"), "{shown}");

    let stdio_only = loadout("[mcp.servers.local]\ncmd = \"npx\"\n");
    assert!(McpHttpConfig::from_loadout(&stdio_only).unwrap().servers.is_empty(), "rows without `url` belong to mcp-stdio");
}
