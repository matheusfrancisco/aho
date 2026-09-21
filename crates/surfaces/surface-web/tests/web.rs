//! The web surface against a real session (host world, tool-fs, scripted model) over HTTP on
//! 127.0.0.1:0, plus one HTTPS round trip with a fixture pair.
//!
//! Fixture pair (EC P-256, 100 years, CN/SAN localhost + 127.0.0.1):
//! `openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 36500 -subj /CN=localhost
//!  -addext subjectAltName=DNS:localhost,IP:127.0.0.1 -keyout tests/fixtures/localhost.key -out tests/fixtures/localhost.crt`
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;
use std::time::Duration;

use aho_approval::{ApprovalRequest, ChannelApproval};
use aho_cap::{Composition, Registry};
use aho_config::{FileSnapshot, Layer};
use aho_llm::LlmRoute;
use aho_log::{CapState, MemoryLog, State};
use aho_session::{Boot, Session};
use aho_types::*;
use futures::{SinkExt, StreamExt};
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::{Value, json};
use surface_tui::{Commands, Handler};
use surface_web::{SurfaceWeb, WebOptions, WebServer};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

struct Rig {
    comp: Composition,
    planned: aho_compose::Planned,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig(policy_extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
    let comp = Composition::new("web-test").provide::<world_host::HostWorld>().consume::<tool_fs::ToolFs>().surface::<SurfaceWeb>();
    let text = format!(
        "[model]\nprovider = \"scripted\"\n[tools]\ngrace_ms = 150\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n{policy_extra}",
        root.display(),
        home.join("shadow").display()
    );
    std::fs::write(root.join("aho.toml"), &text).unwrap();
    let mut inputs = aho_compose::Inputs::new(&root, &home);
    inputs.files.push(FileSnapshot { path: root.join("aho.toml").display().to_string(), layer: Layer::FileProject, text });
    if let Ok(p) = std::env::var("PATH") {
        inputs.env.insert("PATH".into(), p);
    }
    let planned = aho_compose::load(&comp, &inputs).unwrap();
    Rig { comp, planned, root, home, _dir: dir }
}

async fn boot(r: &Rig, steps: Vec<Step>) -> (Arc<Session>, mpsc::Receiver<ApprovalRequest>) {
    let (approval, rx) = ChannelApproval::new(4);
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(Arc::new(ScriptedLlm::from_steps(steps)))));
    let s = Session::boot(Boot {
        id: SessionId(0xabc),
        composition: &r.comp,
        plan: &r.planned.plan,
        loadout: &r.planned.loadout,
        bootstrap: aho_compose::bootstrap_entries(&r.planned),
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(approval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: r.root.display().to_string(),
        home: r.home.display().to_string(),
        spill_dir: r.home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    (s, rx)
}

async fn start(r: &Rig, steps: Vec<Step>) -> (WebServer, Arc<Session>) {
    start_with(r, steps, Commands::new()).await
}

async fn start_with(r: &Rig, steps: Vec<Step>, commands: Commands) -> (WebServer, Arc<Session>) {
    let (s, rx) = boot(r, steps).await;
    let web = WebServer::start(s.clone(), rx, r.planned.clone(), commands, "127.0.0.1:0".parse().unwrap(), None, WebOptions::default())
        .await
        .unwrap();
    (web, s)
}

async fn post(web: &WebServer, path: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new().post(format!("{}/api/{path}", web.base_url())).bearer_auth(&web.token).json(&body).send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_api_requires_the_process_token_or_the_one_time_cookie() {
    let r = rig("");
    let (web, _s) = start(&r, vec![]).await;
    let c = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let url = |p: &str| format!("{}{p}", web.base_url());

    let anon = c.post(url("/api/session/fold")).json(&json!({})).send().await.unwrap();
    assert_eq!(anon.status().as_u16(), 401);
    let body: Value = anon.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("Authorization: Bearer"), "{body}");
    let wrong = c.post(url("/api/session/fold")).bearer_auth("0".repeat(64)).json(&json!({})).send().await.unwrap();
    assert_eq!(wrong.status().as_u16(), 401);
    let ws_anon = tokio_tungstenite::connect_async(format!("ws://{}/api/mux", web.addr)).await;
    assert!(ws_anon.is_err(), "the mux needs the token too");
    let (status, fold) = post(&web, "session/fold", json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(fold["session"], "abc");

    // The SPA shell is public (it holds no data); the link exchanges the token once.
    assert_eq!(c.get(url("/")).send().await.unwrap().status().as_u16(), 200);
    let link = c.get(web.login_url()).send().await.unwrap();
    assert_eq!(link.status().as_u16(), 303);
    let set = link.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");
    let cookie = set.split(';').next().unwrap().to_string();
    let with_cookie = c.post(url("/api/session/fold")).header("cookie", &cookie).json(&json!({})).send().await.unwrap();
    assert_eq!(with_cookie.status().as_u16(), 200);
    let again = c.post(url("/api/auth/exchange")).json(&json!({"token": web.token})).send().await.unwrap();
    assert_eq!(again.status().as_u16(), 409, "a second exchange is refused");
    let (status, _) = post(&web, "session/fold", json!({})).await;
    assert_eq!(status, 200, "bearer auth keeps working after the exchange");
}

#[tokio::test]
async fn the_fence_refuses_foreign_origins_and_hosts() {
    let r = rig("");
    let (web, _s) = start(&r, vec![]).await;
    let c = reqwest::Client::new();
    let fold = || c.post(format!("{}/api/session/fold", web.base_url())).bearer_auth(&web.token).json(&json!({}));

    let evil = fold().header("origin", "https://evil.example").send().await.unwrap();
    assert_eq!(evil.status().as_u16(), 403);
    let body: Value = evil.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("cross-origin"), "{body}");
    let rebind = fold().header("host", format!("attacker.example:{}", web.addr.port())).send().await.unwrap();
    assert_eq!(rebind.status().as_u16(), 403, "DNS rebinding: a foreign Host is refused");
    let same = fold().header("origin", format!("http://127.0.0.1:{}", web.addr.port())).send().await.unwrap();
    assert_eq!(same.status().as_u16(), 200);
    let local = fold().header("host", format!("localhost:{}", web.addr.port())).send().await.unwrap();
    assert_eq!(local.status().as_u16(), 200);

    let mut req = format!("ws://{}/api/mux", web.addr).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {}", web.token).parse().unwrap());
    req.headers_mut().insert("origin", "https://evil.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err(), "a cross-site WebSocket is refused");
}

#[tokio::test]
async fn plain_http_is_refused_off_loopback() {
    let r = rig("");
    let (s, rx) = boot(&r, vec![]).await;
    let e = WebServer::start(s, rx, r.planned.clone(), Commands::new(), "0.0.0.0:0".parse().unwrap(), None, WebOptions::default())
        .await
        .err()
        .unwrap();
    assert!(e.contains("HTTPS-only") && e.contains("aho cert"), "{e}");
}

#[tokio::test]
async fn https_serves_with_the_operator_pair_and_plain_http_gets_nothing() {
    let r = rig("");
    let (s, rx) = boot(&r, vec![]).await;
    let pair = (include_str!("fixtures/localhost.crt").to_string(), include_str!("fixtures/localhost.key").to_string());
    let web =
        WebServer::start(s, rx, r.planned.clone(), Commands::new(), "127.0.0.1:0".parse().unwrap(), Some(pair), WebOptions::default())
            .await
            .unwrap();
    assert!(web.base_url().starts_with("https://"));
    let c = reqwest::Client::builder().danger_accept_invalid_certs(true).redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r1 = c.post(format!("{}/api/session/fold", web.base_url())).bearer_auth(&web.token).json(&json!({})).send().await.unwrap();
    assert_eq!(r1.status().as_u16(), 200);
    let link = c.get(web.login_url()).send().await.unwrap();
    assert!(link.headers()["set-cookie"].to_str().unwrap().contains("; Secure"), "the cookie is Secure over TLS");
    let plain = reqwest::Client::new().get(format!("http://{}/", web.addr)).timeout(Duration::from_secs(5)).send().await;
    assert!(plain.is_err(), "the TLS port does not speak plain HTTP");
    assert!(
        surface_web::WebServer::start(
            boot(&r, vec![]).await.0,
            mpsc::channel(1).1,
            r.planned.clone(),
            Commands::new(),
            "127.0.0.1:0".parse().unwrap(),
            Some(("not a pem".into(), "nor this".into())),
            WebOptions::default()
        )
        .await
        .err()
        .unwrap()
        .contains("CERTIFICATE")
    );
}

#[tokio::test]
async fn prompt_round_trip_then_mux_replays_from_an_entry_id_and_streams_live() {
    let r = rig("");
    let (web, s) = start(
        &r,
        vec![
            Step::tool_call("read", json!({"path": "a.txt"})).with_stop(ScriptedStop::ToolUse),
            Step::text("the file says alpha").expect_tool_result("alpha"),
            Step::text("second answer"),
        ],
    )
    .await;
    let (status, out) = post(&web, "session/prompt", json!({"text": "read a.txt"})).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["transition"], json!({"transition": "completed"}));
    let (_, fold) = post(&web, "session/fold", json!({})).await;
    assert_eq!(fold["turns"], 1);
    assert!(!fold["checkpoints"].as_array().unwrap().is_empty(), "{fold}");
    let (status, empty) = post(&web, "session/prompt", json!({"text": "   "})).await;
    assert_eq!(status, 400, "{empty}");

    // Replay from the operator's prompt entry: only later entries, in order, then `ready`.
    let entries = s.subscribe_from(None);
    let prompt_id = entries.iter().find(|e| matches!(&e.kind, aho_log::EntryKind::UserMessage { form: Form::Prompt, .. })).unwrap().id.0;
    let mut req = format!("ws://{}/api/mux?from={prompt_id}", web.addr).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {}", web.token).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut replayed = Vec::new();
    loop {
        let f = next_frame(&mut ws).await;
        assert_eq!(f["v"], 1);
        match f["event"].as_str().unwrap() {
            "entry" => replayed.push(f["payload"]["id"].as_u64().unwrap()),
            "ready" => break,
            other => panic!("unexpected frame before ready: {other}"),
        }
    }
    let expected: Vec<u64> = entries.iter().map(|e| e.id.0).filter(|id| *id > prompt_id).collect();
    assert_eq!(replayed, expected);
    let tool_result = entries.iter().any(|e| e.id.0 > prompt_id && matches!(e.kind, aho_log::EntryKind::ToolResult { .. }));
    assert!(tool_result, "the replay window covers the tool result");

    // Live: a second prompt streams new entries (no repeats) and ends with `idle`.
    let web_base = (web.base_url(), web.token.clone());
    let second = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{}/api/session/prompt", web_base.0))
            .bearer_auth(web_base.1)
            .json(&json!({"text": "again"}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });
    let mut live = Vec::new();
    let mut streamed = String::new();
    loop {
        let f = next_frame(&mut ws).await;
        match f["event"].as_str().unwrap() {
            "entry" => live.push(f["payload"]["id"].as_u64().unwrap()),
            "stream" if f["payload"]["kind"] == "text" => streamed.push_str(f["payload"]["text"].as_str().unwrap()),
            "idle" => {
                assert_eq!(f["payload"]["transition"], json!({"transition": "completed"}));
                break;
            }
            _ => {}
        }
    }
    assert_eq!(second.await.unwrap(), 200);
    assert!(live.windows(2).all(|w| w[0] < w[1]) && live[0] > *replayed.last().unwrap(), "{live:?}");
    assert_eq!(streamed, "second answer", "stream chunks reach the mux before the durable message");
    ws.send(Message::Close(None)).await.unwrap();

    // Plan-then-apply rewind over the API.
    let cp = fold["checkpoints"][0]["id"].as_u64().unwrap();
    let (status, plan) = post(&web, "session/rewind", json!({"checkpoint": cp, "mode": "chat"})).await;
    assert_eq!(status, 200, "{plan}");
    assert!(plan["plan"]["restore"].is_null() && plan["plan"]["fork_at"].is_u64(), "{plan}");
    let (status, bad) = post(&web, "session/rewind", json!({"checkpoint": cp, "mode": "sideways"})).await;
    assert_eq!(status, 400, "{bad}");
    let (status, done) = post(&web, "session/rewind", json!({"checkpoint": cp, "mode": "chat", "apply": true})).await;
    assert_eq!(status, 200, "{done}");
    let restored = s.log().get(EntryId(done["entry"].as_u64().unwrap())).unwrap();
    assert!(matches!(restored.kind, aho_log::EntryKind::WorkspaceRestored { mode: aho_log::RewindMode::ChatOnly, .. }));

    let (_, tree) = post(&web, "caps/tree", json!({})).await;
    assert!(tree["text"].as_str().unwrap().contains("● tool-fs"), "{tree}");
    let (_, why) = post(&web, "caps/why", json!({"id": "tool-fs"})).await;
    assert_eq!(why["id"], "tool-fs");
    let (status, _) = post(&web, "caps/why", json!({"id": "nope"})).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn slash_commands_run_behind_auth_list_on_help_and_refuse_unknown_names_without_a_turn() {
    let r = rig("");
    let (record, seen) = std::sync::mpsc::channel::<String>();
    let mut commands = Commands::new();
    let echo: Handler = Arc::new(move |_: &Arc<Session>, args: &str, _| {
        record.send(args.to_string()).unwrap();
        Ok(format!("echoed {args}"))
    });
    commands.register("echo", "<text>", "repeat the text", echo).unwrap();
    // No scripted steps: a line that reached the model would fail the turn.
    let (web, _s) = start_with(&r, vec![], commands).await;
    let url = format!("{}/api/session/command", web.base_url());

    let anon = reqwest::Client::new().post(&url).json(&json!({"line": "/echo sneaky"})).send().await.unwrap();
    assert_eq!(anon.status().as_u16(), 401);
    let foreign = reqwest::Client::new()
        .post(&url)
        .bearer_auth(&web.token)
        .header("origin", "https://evil.example")
        .json(&json!({"line": "/echo sneaky"}))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status().as_u16(), 403, "the Host/Origin fence covers commands too");
    assert!(seen.try_recv().is_err(), "no refused request reached the handler");

    let mut req = format!("ws://{}/api/mux", web.addr).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {}", web.token).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    while next_frame(&mut ws).await["event"] != "ready" {}

    let (status, out) = post(&web, "session/command", json!({"line": "/echo  hi there "})).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out, json!({"command": "echo", "output": "echoed hi there"}));
    assert_eq!(seen.try_iter().collect::<Vec<_>>(), ["hi there"]);
    // The command's log pair reaches connected tabs although no session event carries it.
    let mut announced = Vec::new();
    while announced.len() < 2 {
        let f = next_frame(&mut ws).await;
        if f["event"] == "entry" {
            announced.push(f["payload"]["kind"]["type"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(announced, ["command_run", "command_done"]);

    let (status, help) = post(&web, "session/command", json!({"line": "/help"})).await;
    assert_eq!(status, 200, "{help}");
    assert_eq!(help["output"], "/help — list these commands\n/echo <text> — repeat the text");

    let (status, unknown) = post(&web, "session/command", json!({"line": "/nope x"})).await;
    assert_eq!(status, 404, "{unknown}");
    let why = unknown["error"].as_str().unwrap();
    assert!(why.contains("nothing was sent to the model") && why.contains("Commands: /help, /echo"), "{why}");
    assert_eq!(unknown["commands"], json!(["help", "echo"]));
    let (status, plain) = post(&web, "session/command", json!({"line": "just text"})).await;
    assert_eq!(status, 400, "{plain}");

    let (_, fold) = post(&web, "session/fold", json!({})).await;
    assert_eq!(fold["turns"], 0, "no command started a turn: {fold}");
    assert!(seen.try_recv().is_err(), "only /echo reached its handler");
}

async fn next_frame<S>(ws: &mut S) -> Value
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("a frame within 10s").unwrap().unwrap();
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

#[tokio::test]
async fn an_approval_answer_resolves_the_pending_card() {
    let r = rig("rules = [\"ask write(*)\"]\n");
    let (web, _s) = start(
        &r,
        vec![Step::tool_call("write", json!({"path": "y.txt", "content": "yes"})).with_stop(ScriptedStop::ToolUse), Step::text("written")],
    )
    .await;
    let mut req = format!("ws://{}/api/mux", web.addr).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {}", web.token).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

    let base = (web.base_url(), web.token.clone());
    let turn = tokio::spawn(async move {
        let r = reqwest::Client::new()
            .post(format!("{}/api/session/prompt", base.0))
            .bearer_auth(base.1)
            .json(&json!({"text": "write y"}))
            .send()
            .await
            .unwrap();
        r.json::<Value>().await.unwrap()
    });
    let card = loop {
        let f = next_frame(&mut ws).await;
        if f["event"] == "approval" {
            break f["payload"].clone();
        }
    };
    assert_eq!(card["tool"], "write");
    assert_eq!(card["cap"], "tool-fs");
    assert_eq!(card["class"], "workspace_write");
    assert_eq!(card["reason"], "Rule");
    assert_eq!(card["args"], json!({"path": "y.txt", "content": "yes"}));
    assert_eq!(card["epoch_next"], 2);
    let (_, pending) = post(&web, "approval/pending", json!({})).await;
    assert_eq!(pending["pending"].as_array().unwrap().len(), 1);

    let id = card["id"].as_u64().unwrap();
    let (status, bad) = post(&web, "approval/answer", json!({"id": id, "outcome": "maybe"})).await;
    assert_eq!(status, 400, "{bad}");
    let (status, ans) = post(&web, "approval/answer", json!({"id": id, "outcome": "allow", "scope": "once"})).await;
    assert_eq!(status, 200, "{ans}");
    let resolved = loop {
        let f = next_frame(&mut ws).await;
        if f["event"] == "approval_resolved" {
            break f["payload"].clone();
        }
    };
    assert_eq!(resolved, json!({"id": id, "outcome": "allow"}));
    assert_eq!(turn.await.unwrap()["transition"], json!({"transition": "completed"}));
    assert_eq!(std::fs::read_to_string(r.root.join("y.txt")).unwrap(), "yes");
    let (status, _) = post(&web, "approval/answer", json!({"id": id, "outcome": "reject"})).await;
    assert_eq!(status, 404, "a card is answered exactly once");
    let (_, pending) = post(&web, "approval/pending", json!({})).await;
    assert!(pending["pending"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn panels_ship_only_for_active_capabilities() {
    let r = rig("");
    let (web, s) = start(&r, vec![]).await;
    let (_, p) = post(&web, "ui/panels", json!({})).await;
    let slots: Vec<&str> = p["panels"].as_array().unwrap().iter().map(|x| x["slot"].as_str().unwrap()).collect();
    assert_eq!(slots, ["chat", "sidebar.tree", "sidebar.inventory", "sidebar.jobs", "sidebar.invariants"]);
    assert_eq!(p["tools"]["read"], "tool-fs");

    let mut st: State = s.fold().unwrap();
    st.caps.insert("surface-web".into(), CapState::Disabled { why: "--disable surface-web".into() });
    assert!(surface_web::active_slots(&s.registry().panels, &st).is_empty(), "a disabled capability ships no panel");
}
