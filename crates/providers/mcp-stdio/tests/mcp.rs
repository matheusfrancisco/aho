//! Real fixture over a virtual-command world transport; its command cannot run on the host.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aho_cap::*;
use aho_config::{FileSnapshot, Layer, Loadout, ResolveInputs, Schema, resolve};
use aho_mcp::*;
use aho_proc::{Child, ProcError, SpawnSpec, Subprocess, SubprocessHandle, TransportChild};
use aho_types::*;
use mcp_stdio::{McpConfig, McpStdio};
use serde_json::json;
use tool_mcp_resources::ToolMcpResources;

const FIXTURE: &str = env!("CARGO_BIN_EXE_mcp-fixture");
const WORLD_COMMAND: &str = "/fixture-world/bin/mcp";

/// Test world: virtual executable and per-instance environment. Only this world translates the
/// virtual path to the real fixture binary; invoking it ambiently cannot work.
struct FixtureWorld {
    host: world_host::HostSubprocess,
    session: String,
    children: Mutex<Vec<Arc<dyn Child>>>,
}

impl FixtureWorld {
    fn new(session: &str) -> Self {
        Self { host: world_host::HostSubprocess::new(std::path::Path::new("/")), session: session.into(), children: Mutex::new(Vec::new()) }
    }
}

impl Subprocess for FixtureWorld {
    fn spawn(&self, _: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
        Box::pin(async { Err(ProcError::Denied("fixture world only permits protocol transports".into())) })
    }
    fn spawn_transport(&self, mut spec: SpawnSpec) -> BoxFuture<'_, Result<TransportChild, ProcError>> {
        Box::pin(async move {
            if spec.argv.first().map(String::as_str) != Some(WORLD_COMMAND) {
                return Err(ProcError::NotFound(spec.argv.first().cloned().unwrap_or_default()));
            }
            spec.argv[0] = FIXTURE.into();
            spec.env.push(("FIXTURE_SESSION".into(), self.session.clone()));
            let transport = self.host.spawn_transport(spec).await?;
            self.children.lock().push(transport.child.clone());
            Ok(transport)
        })
    }
}

struct FixtureWorldProvider(SubprocessHandle);
impl Capability for FixtureWorldProvider {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(Self(SubprocessHandle(Arc::new(FixtureWorld::new("fixture-session")))))
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(self.0.clone());
    }
}
declare_capability!(
    FixtureWorldProvider,
    "fixture-world",
    kind = Kind::Provider,
    provides = [aho_proc::SEAM],
    requires = [],
    tools = &[],
    provides_handles = [HandleKind::Subprocess],
    doc = "Virtual fixture execution world."
);

fn loadout(toml: &str) -> Loadout {
    let mut schema = Schema::default();
    core_keys().into_iter().chain(<McpConfig as Config>::keys()).for_each(|k| schema.add(k));
    let inputs = ResolveInputs {
        files: vec![FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: toml.into() }],
        ..ResolveInputs::default()
    };
    resolve(&inputs, &schema, false).unwrap()
}

fn start(toml: &str) -> Registry {
    let l = loadout(toml);
    let comp = Composition::new("t").provide::<FixtureWorldProvider>().provide::<McpStdio>().consume::<ToolMcpResources>();
    let policy = PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::ALL };
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    assert!(p.active().any(|r| r.id.name == "tool-mcp-resources"));
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    reg
}

fn server(name: &str, extra: &str) -> String {
    format!("[mcp.servers.{name}]\ncmd = \"{WORLD_COMMAND}\"\n{extra}\n")
}

fn status(reg: &Registry) -> Arc<Vec<ServerStatus>> {
    reg.service::<Arc<Vec<ServerStatus>>>(STATUS_SERVICE).unwrap()
}

fn view(tool: &str, args: Value, allowed: ConsequenceSet) -> ExecView {
    ExecView {
        call: CallId("c1".into()),
        tool: tool.into(),
        class: Consequence::ReadOnly,
        args,
        policy: PolicyView { epoch: Epoch(1), allowed, world: BoundWorld::host(), workspace: String::new(), home: String::new() },
        by: Principal::Agent { id: AgentId(0), turn: TurnId(1) },
    }
}

async fn call_with(reg: &Registry, name: &str, args: Value, cancel: CancelToken) -> ExecOutcome {
    let exec = match reg.tool(name) {
        Some((_, exec)) => exec,
        None => reg.dynamic_tools.iter().find(|(_, s, _)| s.name == name).map(|(_, _, e)| e.clone()).expect(name),
    };
    exec(ExecRequest { view: view(name, args, ConsequenceSet::ALL), cancel, progress: None }).await
}

async fn call(reg: &Registry, name: &str, args: Value) -> ExecOutcome {
    call_with(reg, name, args, CancelToken::new()).await
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n"),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_paginated_listing_and_calls() {
    let reg = start(&format!(
        "{}\n[credentials]\nfx = \"s3cret\"\n",
        server("fx", "class = \"NetworkEgress\"\nargs = []\nenv = { FIXTURE_SECRET = \"cred:fx\", PLAIN = \"p\" }")
    ));
    let weird = normalize("fx", "weird.name/with spaces");
    assert!(
        weird.starts_with("mcp__fx__weird_name_with_spaces_") && weird.len() == "mcp__fx__weird_name_with_spaces_".len() + HASH_HEX,
        "{weird}"
    );
    let names: Vec<&str> = reg.dynamic_tools.iter().map(|(_, s, _)| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["mcp__fx__echo", "mcp__fx__add", "mcp__fx__fail", "mcp__fx__env", weird.as_str(), "mcp__fx__image", "mcp__fx__slow"],
        "both pages, in server order (the fixture pings us before page one)"
    );
    let echo = &reg.dynamic_tools[0].1;
    assert_eq!(echo.class, Consequence::NetworkEgress, "tools carry the server's class");
    assert_eq!(echo.description, "Echo text back");
    assert_eq!(echo.input_schema.0["required"], json!(["text"]));
    assert_eq!(reg.dynamic_tools[4].1.input_schema.0, json!({"type": "object"}), "missing schema defaults to an object");
    assert!(reg.dynamic_tools.iter().all(|(cap, _, _)| cap.name == "mcp-stdio"));

    assert_eq!(text(&call(&reg, "mcp__fx__echo", json!({"text": "hi"})).await), "hi");
    let ExecOutcome::Done { value, .. } = call(&reg, "mcp__fx__add", json!({"a": 2, "b": 3})).await else { panic!() };
    assert_eq!(value["structuredContent"]["sum"], json!(5.0));
    assert_eq!(call(&reg, "mcp__fx__fail", json!({})).await, ExecOutcome::err("MCP_TOOL_ERROR", "boom"), "isError → Err");
    assert_eq!(text(&call(&reg, "mcp__fx__env", json!({"name": "FIXTURE_SECRET"})).await), "s3cret", "cred: resolved from credentials.fx");
    assert_eq!(text(&call(&reg, "mcp__fx__env", json!({"name": "FIXTURE_SESSION"})).await), "fixture-session");
    assert_eq!(text(&call(&reg, weird.as_str(), json!({})).await), "ok", "the raw name is kept, never parsed back");
    let ExecOutcome::Done { content, .. } = call(&reg, "mcp__fx__image", json!({})).await else { panic!() };
    assert_eq!(content, vec![ContentBlock::Image { media_type: "image/gif".into(), data: b"hi".to_vec() }]);

    let st = status(&reg);
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].state, ServerState::Ready);
    assert_eq!(st[0].tools.len(), 7);
    let dir = reg.handle::<McpHandle>().unwrap().0;
    assert_eq!(dir.names(), vec!["fx"]);
    assert_eq!(dir.get("fx").unwrap().instructions(), Some("Fixture server."));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unclassified_server_is_quarantined() {
    let reg = start(&server("q", ""));
    assert!(reg.dynamic_tools.is_empty(), "no tool from an unclassified server reaches the model");
    let st = status(&reg);
    assert_eq!(st[0].state, ServerState::Quarantined { reason: "unclassified".into() });
    assert_eq!(st[0].tools.len(), 7, "probed: the card can list what it would expose");
    assert_eq!(st[0].fix.as_deref(), Some("aho set mcp.servers.q.class NetworkEgress"));
    assert!(reg.handle::<McpHandle>().unwrap().0.names().is_empty());
    let out = call(&reg, tool_mcp_resources::LIST, json!({})).await;
    assert!(text(&out).starts_with("No MCP server is running."), "quarantined resources are unreachable too");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_conflict_rolls_back_only_that_server() {
    let reg = start(&format!(
        "{}{}",
        server("dup", "class = \"ReadOnly\"\nenv = { FIXTURE_PROFILE = \"dup\" }"),
        server("ok", "class = \"ReadOnly\"")
    ));
    let names: Vec<&str> = reg.dynamic_tools.iter().map(|(_, s, _)| s.name.as_str()).collect();
    assert!(names.iter().all(|n| n.starts_with("mcp__ok__")), "{names:?}");
    assert_eq!(names.len(), 7);
    let st = status(&reg);
    let dup = st.iter().find(|s| s.name == "dup").unwrap();
    assert!(matches!(&dup.state, ServerState::Conflict { error } if error.contains("`mcp__dup__a`")), "{:?}", dup.state);
    assert!(dup.tools.is_empty());
    assert_eq!(st.iter().find(|s| s.name == "ok").unwrap().state, ServerState::Ready);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_failures_are_reported_per_server_with_fixes() {
    let reg = start(&format!(
        "{}{}{}{}{}{}[mcp.servers.nocmd]\nclass = \"ReadOnly\"\n\n[mcp.servers.missing]\ncmd = \"/nonexistent/mcp-server\"\nclass = \"ReadOnly\"\n",
        server("badversion", "class = \"ReadOnly\"\nenv = { FIXTURE_PROFILE = \"badversion\" }"),
        server("crash", "class = \"ReadOnly\"\nenv = { FIXTURE_PROFILE = \"crash\" }"),
        server("nocred", "class = \"ReadOnly\"\nenv = { TOKEN = \"cred:nope\" }"),
        server("badclass", "class = \"Sometimes\""),
        server("notools", "class = \"ReadOnly\"\nenv = { FIXTURE_PROFILE = \"notools\" }"),
        server("good", "class = \"ReadOnly\""),
    ));
    let st = status(&reg);
    let get = |n: &str| st.iter().find(|s| s.name == n).unwrap();
    let err = |n: &str| match &get(n).state {
        ServerState::Failed { error } => error.clone(),
        other => panic!("{n}: {other:?}"),
    };
    assert!(err("badversion").contains("speaks protocol `1999-01-01`"), "{}", err("badversion"));
    assert!(err("crash").contains("fixture crashed on purpose"), "stderr tail explains the exit: {}", err("crash"));
    assert!(err("nocred").contains("credential `nope`"));
    assert_eq!(get("nocred").fix.as_deref(), Some("aho credential set nope"));
    assert!(err("badclass").contains("`mcp.servers.badclass.class = Sometimes` is not a consequence class"));
    assert!(err("nocmd").contains("`mcp.servers.nocmd.cmd` is not set"));
    assert!(err("missing").contains("could not start `/nonexistent/mcp-server`"));
    assert_eq!(get("notools").state, ServerState::Ready, "a server without tools is fine");
    assert!(get("notools").tools.is_empty());
    assert_eq!(get("good").state, ServerState::Ready, "one broken server does not stop the others");
    assert_eq!(reg.dynamic_tools.len(), 7);
    assert_eq!(reg.handle::<McpHandle>().unwrap().0.names(), vec!["good", "notools"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn resources_render_blobs_as_placeholders_and_keep_bytes_in_value() {
    let reg = start(&server("fx", "class = \"NetworkEgress\""));
    let listed = call(&reg, tool_mcp_resources::LIST, json!({})).await;
    assert_eq!(
        text(&listed),
        "server `fx`:\n- mem://readme  readme (text/plain) — The readme\n- mem://logo  logo (image/png)",
        "both pages"
    );
    let ExecOutcome::Done { value, .. } = &listed else { panic!() };
    assert_eq!(value.as_array().unwrap().len(), 2);
    assert_eq!(value[1]["server"], "fx");

    let templates = call(&reg, tool_mcp_resources::TEMPLATES, json!({"server": "fx"})).await;
    assert_eq!(text(&templates), "server `fx`:\n- mem://notes/{id}  note — A note by id");

    let logo = call(&reg, tool_mcp_resources::READ, json!({"server": "fx", "uri": "mem://logo"})).await;
    assert_eq!(text(&logo), "mem://logo (image/png)\n[binary resource: 8 bytes; available to programmatic callers]");
    let ExecOutcome::Done { value, .. } = &logo else { panic!() };
    assert_eq!(value["contents"][0]["blob"], "iVBORw0KGgo=");
    let readme = call(&reg, tool_mcp_resources::READ, json!({"server": "fx", "uri": "mem://readme"})).await;
    assert_eq!(text(&readme), "mem://readme (text/plain)\nhello from the fixture");

    let ExecOutcome::Err { code, reason } = call(&reg, tool_mcp_resources::READ, json!({"server": "fx", "uri": "mem://nope"})).await else {
        panic!()
    };
    assert_eq!(code, "MCP_RPC");
    assert!(reason.contains("resource not found: mem://nope"), "{reason}");
    let ExecOutcome::Err { code, reason } = call(&reg, tool_mcp_resources::READ, json!({"server": "gh", "uri": "x"})).await else {
        panic!()
    };
    assert_eq!(code, "MCP_UNKNOWN_SERVER");
    assert!(reason.contains("running: fx"));

    // the class gate: NetworkEgress outside the preset → ask first
    let gate = &reg.pre[0].1;
    let read_only = ConsequenceSet::of(&[Consequence::ReadOnly]);
    let decision = gate(&view(tool_mcp_resources::READ, json!({"server": "fx", "uri": "mem://readme"}), read_only));
    assert!(matches!(&decision, PreDecision::Ask { reason: Some(r) } if r.contains("`fx` (NetworkEgress)")), "{decision:?}");
    assert_eq!(gate(&view(tool_mcp_resources::LIST, json!({}), ConsequenceSet::ALL)), PreDecision::Allow);
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_calls_time_out_or_cancel_and_the_connection_survives() {
    let reg = start(&format!("{}{}", server("t", "class = \"ReadOnly\"\ntimeout_ms = 300"), server("c", "class = \"ReadOnly\"")));
    let started = Instant::now();
    let ExecOutcome::Err { code, reason } = call(&reg, "mcp__t__slow", json!({"ms": 1500})).await else { panic!() };
    assert_eq!(code, "MCP_TIMEOUT");
    assert!(reason.contains("did not answer `tools/call` within 300 ms"), "{reason}");
    assert!(started.elapsed() < Duration::from_millis(1200));

    let cancel = CancelToken::new();
    let firing = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        firing.cancel();
    });
    assert_eq!(call_with(&reg, "mcp__c__slow", json!({"ms": 800}), cancel).await, ExecOutcome::Cancelled);
    // the late answer to the cancelled request is dropped; the next request gets its own answer
    assert_eq!(text(&call(&reg, "mcp__c__echo", json!({"text": "after"})).await), "after");
}

#[tokio::test(flavor = "multi_thread")]
async fn separate_sessions_keep_their_world_environment_and_cleanup_children() {
    let a = Arc::new(FixtureWorld::new("session-a"));
    let b = Arc::new(FixtureWorld::new("session-b"));
    let spec = ServerSpec {
        name: "same-name".into(),
        cmd: WORLD_COMMAND.into(),
        args: vec![],
        env: vec![],
        class: Some(Consequence::ReadOnly),
        timeout_ms: 5000,
    };
    let first = mcp_stdio::StdioServer::start(&SubprocessHandle(a.clone()), &spec, Duration::from_secs(5)).unwrap();
    let second = mcp_stdio::StdioServer::start(&SubprocessHandle(b.clone()), &spec, Duration::from_secs(5)).unwrap();
    let args = json!({"name": "FIXTURE_SESSION"});
    let ra = first.call("env", args.clone(), CancelToken::new()).await.unwrap();
    let rb = second.call("env", args, CancelToken::new()).await.unwrap();
    assert_eq!(ra.content[0]["text"], "session-a");
    assert_eq!(rb.content[0]["text"], "session-b");
    drop(first);
    let child = a.children.lock()[0].clone();
    tokio::time::timeout(Duration::from_secs(2), child.wait()).await.unwrap().unwrap();
    assert_eq!(second.call("echo", json!({"text": "still alive"}), CancelToken::new()).await.unwrap().content[0]["text"], "still alive");
    drop(second);
    let child = b.children.lock()[0].clone();
    tokio::time::timeout(Duration::from_secs(2), child.wait()).await.unwrap().unwrap();
}

#[test]
fn a_world_without_protocol_transport_never_falls_back_to_host_spawn() {
    struct UnsupportedWorld;
    impl Subprocess for UnsupportedWorld {
        fn spawn(&self, _: SpawnSpec) -> BoxFuture<'_, Result<Arc<dyn Child>, ProcError>> {
            panic!("MCP must not fall back to captured or ambient process execution");
        }
    }
    let spec = ServerSpec {
        name: "unsupported".into(),
        cmd: FIXTURE.into(),
        args: vec![],
        env: vec![],
        class: Some(Consequence::ReadOnly),
        timeout_ms: 1000,
    };
    let result = mcp_stdio::StdioServer::start(&SubprocessHandle(Arc::new(UnsupportedWorld)), &spec, Duration::from_secs(1));
    assert!(matches!(result, Err(McpError::Spawn(_))));
}
