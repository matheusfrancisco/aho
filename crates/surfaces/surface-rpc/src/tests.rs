//! Protocol round trips over an in-memory duplex against a real session (scripted model, host
//! world, fs tools).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aho_approval::{ChannelApproval, NeverApproval};
use aho_cap::{BuildError, Capability, CapabilityId, Composition, Env, Kind, Manifest, Os, PolicyInput, Registrar, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{EntryKind, MemoryLog};
use aho_session::{Boot, Session};
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf};

use super::*;

fn composition() -> Composition {
    Composition::new("rpc-test").provide::<world_host::HostWorld>().consume::<tool_fs::ToolFs>().consume::<Fetch>()
}

/// A network tool for the preset path: its class (NetworkEgress) is outside `workspace`, so the
/// chain asks and the card's smallest cut is a preset. The egress handle is the host world's.
struct FetchDeps;
impl aho_cap::Deps for FetchDeps {
    const REQUIRES: &'static [aho_cap::HandleKind] = &[aho_cap::HandleKind::Egress(aho_cap::Route::Sandbox)];
    fn resolve(r: &aho_cap::Resolver) -> Result<Self, aho_cap::MissingDep> {
        r.handle::<aho_egress::EgressHandle>().map(|_| FetchDeps)
    }
}
static FETCH: &[aho_cap::ToolDecl] = &[aho_cap::ToolDecl::new("fetch", "Fetch a URL.", Consequence::NetworkEgress)];
struct Fetch;
impl Capability for Fetch {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = FetchDeps;
    type Config = ();
    fn build(_: FetchDeps, _: ()) -> Result<Self, BuildError> {
        Ok(Fetch)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(&FETCH[0], Arc::new(|_| Box::pin(async { aho_cap::ExecOutcome::text("fetched") })));
    }
}
aho_cap::declare_capability!(Fetch, "tool-fetch-test", kind = Kind::Tool, provides = [], requires = [], tools = FETCH);

struct Rig {
    session: Arc<Session>,
    root: PathBuf,
    home: PathBuf,
    _dir: tempfile::TempDir,
}

async fn rig(steps: Vec<Step>, rules: &str, approval: Arc<dyn aho_approval::Approval>) -> Rig {
    let preset = "workspace";
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let comp = composition();
    let mut schema = Schema::default();
    comp.config_keys().into_iter().for_each(|k| schema.add(k));
    let text = format!(
        "[model]\nprovider = \"scripted\"\n[tools]\ngrace_ms = 100\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"{preset}\"\nrules = [{rules}]\n",
        root.display(),
        home.join("shadow").display()
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let allowed = aho_policy::preset(preset).unwrap().cut(&aho_cap::BoundWorld::host());
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: preset.into(), allowed }, vec![]).unwrap();
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(Arc::new(ScriptedLlm::from_steps(steps)))));
    let session = Session::boot(Boot {
        id: SessionId(0xabc),
        composition: &comp,
        plan: &plan,
        loadout: &loadout,
        bootstrap: vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: plan.allowed, preset: plan.preset.clone(), approval_ref: None }],
        log: Arc::new(MemoryLog::new()),
        approval,
        operator: Operator::authenticate(OperatorProof::local_tty()),
        workspace: root.display().to_string(),
        home: home.display().to_string(),
        spill_dir: home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    Rig { session, root, home, _dir: dir }
}

struct Client {
    w: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
}

impl Client {
    fn connect(r: &Rig, ctx: &Arc<RpcContext>) -> (Client, tokio::task::JoinHandle<Result<(), RpcError>>) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(serve(b, r.session.clone(), ctx.clone()));
        let (rd, w) = tokio::io::split(a);
        (Client { w, lines: BufReader::new(rd).lines() }, server)
    }

    async fn send(&mut self, v: Value) {
        let mut s = v.to_string();
        s.push('\n');
        self.w.write_all(s.as_bytes()).await.unwrap();
    }

    async fn recv(&mut self) -> Option<ServerFrame> {
        let line = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line()).await.expect("frame within 20 s").unwrap()?;
        Some(serde_json::from_str(&line).unwrap())
    }

    /// Frames up to and including the first one matching `pred`.
    async fn until(&mut self, pred: impl Fn(&ServerFrame) -> bool) -> Vec<ServerFrame> {
        let mut seen = Vec::new();
        loop {
            let f = self.recv().await.expect("connection open");
            let done = pred(&f);
            seen.push(f);
            if done {
                return seen;
            }
        }
    }

    async fn hello(&mut self, instance: &str, history: bool, takeover: bool) -> ServerFrame {
        self.send(json!({"v": 1, "id": 1, "method": "hello",
            "params": {"min": 1, "max": 1, "client_instance": instance, "has_local_history": history, "allow_takeover": takeover}}))
            .await;
        self.recv().await.unwrap()
    }
}

fn is_reply(id: u64) -> impl Fn(&ServerFrame) -> bool {
    move |f| f.reply_to == Some(id)
}

#[tokio::test]
async fn hello_negotiates_the_highest_common_version_and_ignores_unknown_fields() {
    let r = rig(vec![], "", Arc::new(NeverApproval)).await;
    let ctx = RpcContext::new(&r.session, None, None);
    let (mut c, _) = Client::connect(&r, &ctx);
    c.send(json!({"v": 1, "id": 7, "method": "hello", "trace": "x",
        "params": {"min": 0, "max": 9, "client_instance": "ts-sdk", "sdk": "typescript"}}))
        .await;
    let ok = c.recv().await.unwrap();
    assert_eq!((ok.event.as_str(), ok.reply_to), ("hello_ok", Some(7)));
    let ok: HelloOk = serde_json::from_value(ok.payload).unwrap();
    assert_eq!(ok.version, 1);
    assert_eq!(ok.session, "abc");
    assert!(ok.capabilities.iter().any(|m| m == "session.prompt") && ok.capabilities.iter().any(|m| m == "caps.tree"));
}

#[tokio::test]
async fn no_common_version_is_refused_and_the_connection_closes() {
    let r = rig(vec![], "", Arc::new(NeverApproval)).await;
    let ctx = RpcContext::new(&r.session, None, None);
    let (mut c, server) = Client::connect(&r, &ctx);
    c.send(json!({"v": 2, "id": 1, "method": "hello", "params": {"min": 2, "max": 3}})).await;
    let err = c.recv().await.unwrap();
    assert_eq!(err.event, "error");
    assert!(err.payload["message"].as_str().unwrap().contains("no common protocol version"), "{err:?}");
    assert!(c.recv().await.is_none(), "closed after the refusal");
    assert!(matches!(server.await.unwrap(), Err(RpcError::Handshake(_))));
}

#[tokio::test]
async fn the_first_frame_must_be_hello() {
    let r = rig(vec![], "", Arc::new(NeverApproval)).await;
    let ctx = RpcContext::new(&r.session, None, None);
    let (mut c, server) = Client::connect(&r, &ctx);
    c.send(json!({"v": 1, "id": 1, "method": "session.fold"})).await;
    assert_eq!(c.recv().await.unwrap().event, "error");
    assert!(matches!(server.await.unwrap(), Err(RpcError::Handshake(_))));
}

#[tokio::test]
async fn prompt_round_trip_streams_entries_and_replies() {
    let r = rig(vec![Step::text("hello from the model")], "", Arc::new(NeverApproval)).await;
    let ctx = RpcContext::new(&r.session, None, None);
    let (mut c, _) = Client::connect(&r, &ctx);
    assert_eq!(c.hello("cli", false, false).await.event, "hello_ok");
    c.send(json!({"v": 1, "id": 2, "method": "session.subscribe", "params": {"from": null}})).await;
    let replay = c.until(is_reply(2)).await;
    assert!(replay.iter().any(|f| f.event == "entry" && f.payload["kind"]["type"] == "policy_epoch"), "bootstrap is replayed");
    c.send(json!({"v": 1, "id": 3, "method": "session.prompt", "params": {"text": "hi"}})).await;
    let frames = c.until(is_reply(3)).await;
    let last = frames.last().unwrap();
    assert_eq!(last.event, "result");
    assert_eq!(last.payload["transition"]["transition"], "completed");
    assert!(frames.iter().any(|f| f.event == "stream" && f.payload["kind"] == "text"), "live stream chunks");
    assert!(frames.iter().any(|f| f.event == "idle"), "idle arrives before the reply");
    let msgs: Vec<&ServerFrame> =
        frames.iter().filter(|f| f.event == "entry" && f.payload["kind"]["type"] == "assistant_message").collect();
    assert_eq!(msgs.len(), 1, "each entry exactly once");
    assert_eq!(msgs[0].payload["kind"]["content"][0]["text"], "hello from the model");
    c.send(json!({"v": 1, "id": 4, "method": "session.fold"})).await;
    let fold = c.until(is_reply(4)).await.pop().unwrap();
    assert_eq!(fold.payload["turns"], 1);
}

#[tokio::test]
async fn approval_cards_round_trip_over_the_wire() {
    let (approval, rx) = ChannelApproval::new(4);
    let r = rig(
        vec![Step::tool_call("write", json!({"path": "y.txt", "content": "yes"})).with_stop(ScriptedStop::ToolUse), Step::text("written")],
        "\"ask write(*)\"",
        Arc::new(approval),
    )
    .await;
    let ctx = RpcContext::new(&r.session, Some(rx), None);
    let (mut c, _) = Client::connect(&r, &ctx);
    c.hello("web", false, false).await;
    c.send(json!({"v": 1, "id": 5, "method": "session.prompt", "params": {"text": "write it"}})).await;
    let card = c.until(|f| f.event == "approval").await.pop().unwrap();
    assert_eq!(card.payload["tool"], "write");
    assert_eq!(card.payload["cap"], "tool-fs");
    assert_eq!(card.payload["reason"], "Rule");
    c.send(json!({"v": 1, "id": 6, "method": "approval.answer", "params": {"id": card.payload["id"], "outcome": "allow"}})).await;
    let answered = c.until(is_reply(6)).await.pop().unwrap();
    assert_eq!(answered.event, "result", "{answered:?}");
    let done = c.until(is_reply(5)).await.pop().unwrap();
    assert_eq!(done.payload["transition"]["transition"], "completed");
    assert_eq!(std::fs::read_to_string(r.root.join("y.txt")).unwrap(), "yes");
    c.send(json!({"v": 1, "id": 7, "method": "approval.answer", "params": {"id": card.payload["id"], "outcome": "allow"}})).await;
    assert_eq!(c.until(is_reply(7)).await.pop().unwrap().event, "error", "a card is answered once");
}

fn caps_for(r: &Rig) -> CapsContext {
    let mut inputs = aho_compose::Inputs::new(&r.root, &r.home);
    if let Ok(p) = std::env::var("PATH") {
        inputs.env.insert("PATH".into(), p);
    }
    CapsContext::new(composition(), inputs)
}

#[tokio::test]
async fn the_preset_answer_writes_the_cut_and_allows_the_call() {
    let (approval, rx) = ChannelApproval::new(4);
    let r = rig(
        vec![
            Step::tool_call("fetch", json!({"url": "https://example.com"})).with_stop(ScriptedStop::ToolUse),
            Step::text("done").expect_tool_result("fetched"),
        ],
        "",
        Arc::new(approval),
    )
    .await;
    let ctx = RpcContext::new(&r.session, Some(rx), Some(caps_for(&r)));
    let (mut c, _) = Client::connect(&r, &ctx);
    c.hello("tui", false, false).await;
    c.send(json!({"v": 1, "id": 2, "method": "session.prompt", "params": {"text": "fetch it"}})).await;
    let card = c.until(|f| f.event == "approval").await.pop().unwrap();
    assert_eq!((card.payload["reason"].as_str(), card.payload["preset_cut"].as_str()), (Some("Preset"), Some("dev")), "{card:?}");
    c.send(json!({"v": 1, "id": 3, "method": "approval.answer", "params": {"id": card.payload["id"], "outcome": "preset"}})).await;
    let ok = c.until(is_reply(3)).await.pop().unwrap();
    assert_eq!(ok.payload["preset"], "dev", "{ok:?}");
    assert_eq!(c.until(is_reply(2)).await.pop().unwrap().payload["transition"]["transition"], "completed");
    let cfg = std::fs::read_to_string(r.home.join(".aho/config.toml")).unwrap();
    assert!(cfg.contains("preset = \"dev\""), "{cfg}");
    let st = r.session.fold().unwrap();
    assert!(st.policy.allowed.contains(Consequence::NetworkEgress), "the call was allowed for the session (epoch opened)");
}

#[tokio::test]
async fn takeover_needs_local_history_and_is_logged() {
    let r = rig(vec![], "", Arc::new(NeverApproval)).await;
    let ctx = RpcContext::new(&r.session, None, None);
    let (mut a, _) = Client::connect(&r, &ctx);
    assert_eq!(a.hello("tui-1", false, false).await.event, "hello_ok");
    let (mut b, b_server) = Client::connect(&r, &ctx);
    let refused = b.hello("phone", false, true).await;
    assert_eq!(refused.event, "error");
    assert!(refused.payload["message"].as_str().unwrap().contains("tui-1"));
    assert!(matches!(b_server.await.unwrap(), Err(RpcError::Refused(_))));
    let (mut c, _) = Client::connect(&r, &ctx);
    assert_eq!(c.hello("desktop", true, true).await.event, "hello_ok");
    let log = r.session.log();
    let attached: Vec<_> = log
        .read_branch(log.head().unwrap())
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::SurfaceAttached { client_instance, takeover, .. } => Some((client_instance, takeover)),
            _ => None,
        })
        .collect();
    assert_eq!(attached.len(), 3);
    assert!(matches!(&attached[1], (i, Takeover::Refused(_)) if i == "phone"));
}

#[tokio::test]
async fn a_frame_in_another_version_is_answered_with_an_error_not_a_close() {
    let r = rig(vec![], "", Arc::new(NeverApproval)).await;
    let ctx = RpcContext::new(&r.session, None, None);
    let (mut c, _) = Client::connect(&r, &ctx);
    c.hello("x", false, false).await;
    c.send(json!({"v": 2, "id": 2, "method": "session.fold"})).await;
    assert_eq!(c.until(is_reply(2)).await.pop().unwrap().event, "error");
    c.send(json!({"v": 1, "id": 3, "method": "no.such"})).await;
    assert!(c.until(is_reply(3)).await.pop().unwrap().payload["message"].as_str().unwrap().contains("unknown method"));
    c.send(json!({"v": 1, "id": 4, "method": "session.fold"})).await;
    assert_eq!(c.until(is_reply(4)).await.pop().unwrap().event, "result");
}

#[tokio::test]
async fn caps_set_plans_before_it_writes() {
    let r = rig(vec![], "", Arc::new(NeverApproval)).await;
    let ctx = RpcContext::new(&r.session, None, Some(caps_for(&r)));
    let (mut c, _) = Client::connect(&r, &ctx);
    c.hello("cli", false, false).await;
    let target = r.home.join(".aho/config.toml");

    c.send(json!({"v": 1, "id": 2, "method": "caps.set", "params": {"key": "policy.preset", "value": "read-only"}})).await;
    let plan = c.until(is_reply(2)).await.pop().unwrap();
    assert_eq!(plan.payload["applied"], false);
    assert!(plan.payload["rendered"].as_str().unwrap().contains("preset  workspace -> read-only"), "{plan:?}");
    assert!(!target.exists(), "planning writes nothing");

    c.send(json!({"v": 1, "id": 3, "method": "caps.set", "params": {"key": "policy.preset", "value": "read-only", "apply": true}})).await;
    let wrote = c.until(is_reply(3)).await.pop().unwrap();
    assert!(wrote.payload["wrote"][0].as_str().unwrap().starts_with(&format!("wrote {}:", target.display())), "{wrote:?}");
    assert!(std::fs::read_to_string(&target).unwrap().contains("read-only"));

    c.send(json!({"v": 1, "id": 4, "method": "caps.tree"})).await;
    assert_eq!(c.until(is_reply(4)).await.pop().unwrap().payload["preset"], "read-only");

    c.send(json!({"v": 1, "id": 5, "method": "caps.set", "params": {"key": "polcy.preset", "value": "x"}})).await;
    let err = c.until(is_reply(5)).await.pop().unwrap();
    assert!(err.payload["message"].as_str().unwrap().contains("not a setting"), "{err:?}");
}
