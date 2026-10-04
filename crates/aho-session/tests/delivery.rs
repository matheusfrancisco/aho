//! The delivery chain end to end: a `Registrar::delivery` callback or a `HookEvent::JobDone` hook
//! replaces, suppresses or defers a job's delivery; callbacks run before hooks, each in start
//! order, and a suppress ends the chain; every run is audited under its capability; and the
//! logged `JobDelivered` holds exactly what the model saw, so replay stays equal.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aho_approval::NeverApproval;
use aho_cap::*;
use aho_config::{ConfigKey, FileSnapshot, Layer, Loadout, ResolveInputs, Schema, Ty, resolve};
use aho_llm::{AdapterInfo, LateResults, Llm, LlmError, LlmRoute, Request, Response, Sink};
use aho_log::{Entry, EntryKind, MemoryLog, OpKind, OpStatus, Transition};
use aho_session::{Boot, Session, SessionEvent, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;
use tokio::sync::broadcast;

/// How long the `defer` action holds a delivery.
const HOLD: Duration = Duration::from_millis(300);

// ── a detachable test tool: sleeps `ms`, then answers `text` and the args it saw ──

struct JobTools;
static JOB_TOOLS: &[ToolDecl] =
    &[ToolDecl::new("sleep", "Sleeps.", Consequence::ReadOnly).safe().schema(schema).detachable().op(OpKind::Timer, false, false)];

fn schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": {"ms": {"type": "integer"}, "text": {"type": "string"}}}))
}

impl Capability for JobTools {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(JobTools)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(
            &JOB_TOOLS[0],
            Arc::new(|req: ExecRequest| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(req.view.args["ms"].as_u64().unwrap_or(0))).await;
                    ExecOutcome::text(format!("{} args={}", req.view.args["text"].as_str().unwrap_or(""), req.view.args))
                })
            }),
        );
    }
}
declare_capability!(JobTools, "job-tools", kind = Kind::Tool, provides = [], requires = [], tools = JOB_TOOLS);

// ── `shaper`: one action, through `Registrar::delivery` or a `JobDone` hook (`shaper.via`) ──

#[derive(Clone)]
struct ShaperConfig {
    via: String,
    action: String,
}

impl Config for ShaperConfig {
    const PREFIX: &'static str = "shaper";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new("shaper.via", Ty::String, Some(json!("delivery")), "delivery | hook"),
            ConfigKey::new("shaper.action", Ty::String, Some(json!("keep")), "replace | suppress | defer | keep"),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        Ok(ShaperConfig { via: cfg_str(l, "shaper.via", "delivery"), action: cfg_str(l, "shaper.action", "keep") })
    }
}

/// Every view the shaper was offered, as JSON (the hook payload, or the serialized `JobView`).
type Seen = Arc<Mutex<Vec<Value>>>;

struct Shaper {
    cfg: ShaperConfig,
    seen: Seen,
}

/// `replace` redacts and marks the text; `defer` holds a first offer for `HOLD`.
fn shape(action: &str, text: &str, deferred: u64) -> Delivery {
    match action {
        "replace" => Delivery::Replace(vec![ContentBlock::text(format!("rewritten: {}", text.replace("secret", "[redacted]")))]),
        "suppress" => Delivery::Suppress,
        "defer" if deferred == 0 => Delivery::Defer { until: Instant::now() + HOLD },
        _ => Delivery::Keep,
    }
}

impl Capability for Shaper {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ShaperConfig;
    fn build(_: (), cfg: ShaperConfig) -> Result<Self, BuildError> {
        Ok(Shaper { cfg, seen: Seen::default() })
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.service("shaper.seen", self.seen.clone());
        let (action, seen) = (self.cfg.action.clone(), self.seen.clone());
        if self.cfg.via == "hook" {
            r.hook(job_done(move |p| {
                seen.lock().unwrap_or_else(PoisonError::into_inner).push(p.clone());
                shape(&action, p["content"].as_str().unwrap_or(""), p["deferred"].as_u64().unwrap_or(0))
            }));
        } else {
            r.delivery(Arc::new(move |v: &JobView| {
                seen.lock().unwrap_or_else(PoisonError::into_inner).push(serde_json::to_value(v).unwrap());
                shape(&action, &text(&v.content), u64::from(v.deferred))
            }));
        }
    }
}
declare_capability!(Shaper, "shaper", kind = Kind::Hook, provides = [], requires = [], tools = &[]);

/// A `JobDone` hook deciding from the payload.
fn job_done(f: impl Fn(&Value) -> Delivery + Send + Sync + 'static) -> HookSpec {
    HookSpec {
        on: &[HookEvent::JobDone],
        if_class: None,
        if_tool: None,
        timeout_ms: 1_000,
        once: false,
        on_error: OnError::Deny,
        run: Arc::new(move |_, p: &Value| {
            let delivery = f(p);
            Box::pin(async move { HookOutput { delivery, ..HookOutput::default() } })
        }),
    }
}

// ── taggers: append ` <id>` to the delivered text, through a callback or a hook ──

fn tag(r: &mut Registrar<'_>, name: &'static str, hook: bool) {
    let append = move |t: &str| Delivery::Replace(vec![ContentBlock::text(format!("{t} {name}"))]);
    if hook {
        r.hook(job_done(move |p| append(p["content"].as_str().unwrap_or(""))));
    } else {
        r.delivery(Arc::new(move |v: &JobView| append(&text(&v.content))));
    }
}

macro_rules! tagger {
    ($ty:ident, $name:literal, hook = $hook:literal) => {
        struct $ty;
        impl Capability for $ty {
            const ID: CapabilityId = Self::ID_;
            const MANIFEST: &'static Manifest = &Self::MANIFEST_;
            type Deps = ();
            type Config = ();
            fn build(_: (), _: ()) -> Result<Self, BuildError> {
                Ok($ty)
            }
            fn register(&self, r: &mut Registrar<'_>) {
                tag(r, $name, $hook);
            }
        }
        declare_capability!($ty, $name, kind = Kind::Hook, provides = [], requires = [], tools = &[]);
    };
}

tagger!(TagA, "tag-a", hook = false);
tagger!(TagB, "tag-b", hook = false);
// Starts before both taggers (id order), yet runs after them: hooks follow callbacks.
tagger!(TagHook, "tag-0", hook = true);

// ── rig ──

/// The scripted model, recording every request.
struct Recording(ScriptedLlm, Mutex<Vec<Request>>);

impl Llm for Recording {
    fn info(&self) -> AdapterInfo {
        self.0.info()
    }
    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        self.1.lock().unwrap_or_else(PoisonError::into_inner).push(req.clone());
        self.0.stream(req, sink)
    }
}

struct Rig {
    comp: Composition,
    plan: Plan,
    loadout: Loadout,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn base() -> Composition {
    Composition::new("delivery-test").provide::<world_host::HostWorld>().consume::<JobTools>()
}

fn rig(comp: Composition, extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 150\nheartbeat_s = 30\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n{extra}",
        root.display(),
        home.join("shadow").display()
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let allowed = aho_policy::preset("workspace").unwrap().cut(&BoundWorld::host());
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();
    Rig { comp, plan, loadout, root, home, _dir: dir }
}

async fn boot(r: &Rig, steps: Vec<Step>) -> (Arc<Session>, Arc<Recording>) {
    let llm = Arc::new(Recording(ScriptedLlm::from_steps(steps), Mutex::default()));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let s = Session::boot(Boot {
        id: SessionId(12),
        composition: &r.comp,
        plan: &r.plan,
        loadout: &r.loadout,
        bootstrap: vec![EntryKind::PolicyEpoch {
            epoch: Epoch(1),
            allowed: r.plan.allowed,
            preset: r.plan.preset.clone(),
            approval_ref: None,
        }],
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: r.root.display().to_string(),
        home: r.home.display().to_string(),
        spill_dir: r.home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    (s, llm)
}

/// Turn 1 sends a `sleep` answering `secret-out` to the background and ends while it runs;
/// returns once the idle session announces it finished. `then` scripts the later turns.
async fn finished_job(r: &Rig, then: Vec<Step>) -> (Arc<Session>, Arc<Recording>, OpId) {
    finished_job_with(r, "secret-out", then).await
}

async fn finished_job_with(r: &Rig, text: &str, then: Vec<Step>) -> (Arc<Session>, Arc<Recording>, OpId) {
    let mut steps = vec![
        Step::tool_call("sleep", json!({"ms": 200, "text": text, "background": true})).with_stop(ScriptedStop::ToolUse),
        Step::text("started"),
    ];
    steps.extend(then);
    let (s, llm) = boot(r, steps).await;
    let mut events = s.subscribe();
    assert_eq!(s.prompt("start it", None).await.unwrap(), Transition::Completed);
    let op = announced(&mut events).await;
    (s, llm, op)
}

async fn announced(events: &mut broadcast::Receiver<SessionEvent>) -> OpId {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(SessionEvent::JobFinished { op }) = events.recv().await {
                return op;
            }
        }
    })
    .await
    .expect("the idle session announces the finished job")
}

fn entries(s: &Session) -> Vec<Entry> {
    s.log().read_branch(s.log().head().unwrap()).unwrap()
}

fn replay_ok(s: &Session) -> usize {
    verify_replay(&entries(s), &Default::default(), LateResults::SameCallId).unwrap()
}

fn text(content: &[ContentBlock]) -> String {
    content.iter().filter_map(|b| b.as_text()).collect()
}

/// `(op, status, content text)` of every `JobDelivered`.
fn delivered(s: &Session) -> Vec<(OpId, OpStatus, String)> {
    entries(s)
        .into_iter()
        .filter_map(|e| {
            if let EntryKind::JobDelivered { op, status, content, .. } = e.kind { Some((op, status, text(&content))) } else { None }
        })
        .collect()
}

/// `(cap, decision)` of every `JobDone` run, in order; each pair is logged under its capability.
fn audit(s: &Session) -> Vec<(String, String)> {
    entries(s)
        .windows(2)
        .filter_map(|w| match (&w[0].kind, &w[1].kind) {
            (EntryKind::HookInvoked { point, cap, .. }, EntryKind::HookResult { decision, .. }) if point == "JobDone" => {
                assert!(w[0].by == Principal::system(cap) && w[1].by == w[0].by, "audited under the capability: {:?}", w[0].by);
                Some((cap.clone(), decision.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The job notices the model was shown in its latest request.
fn notices(llm: &Recording) -> Vec<String> {
    let reqs = llm.1.lock().unwrap_or_else(PoisonError::into_inner);
    reqs.last()
        .unwrap()
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(Message::text)
        .filter(|t| t.starts_with("Background job"))
        .collect()
}

fn seen(s: &Session) -> Vec<Value> {
    s.registry().service::<Seen>("shaper.seen").unwrap().lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The delivery sits at the turn's start: after its prompt and the first step's start (then the
/// chain's audit pairs); its `OpDone` follows it.
fn delivered_at_turn_start(s: &Session, op: OpId, prompt: &str) {
    let es = entries(s);
    let d = es.iter().position(|e| matches!(e.kind, EntryKind::JobDelivered { op: o, .. } if o == op)).unwrap();
    let mut before = es[..d].iter().rev().filter(|e| !matches!(e.kind, EntryKind::HookInvoked { .. } | EntryKind::HookResult { .. }));
    assert!(matches!(before.next().unwrap().kind, EntryKind::StepStart { step: StepId(1) }));
    let prompted = before.next().unwrap();
    assert!(matches!(&prompted.kind, EntryKind::UserMessage { text, .. } if text == prompt), "{:?}", prompted.kind);
    assert!(matches!(es[d + 1].kind, EntryKind::OpDone { op: o, .. } if o == op));
}

fn shaper(via: &str, action: &str) -> Rig {
    rig(base().consume::<Shaper>(), &format!("[shaper]\nvia = \"{via}\"\naction = \"{action}\"\n"))
}

// ── replace ──

async fn replaced_via(via: &str) {
    let r = shaper(via, "replace");
    let (s, llm, op) = finished_job(&r, vec![Step::text("noted")]).await;
    assert_eq!(s.prompt("what happened?", None).await.unwrap(), Transition::Completed);

    let got = delivered(&s);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].0, &got[0].1), (op, &OpStatus::Completed { code: 0 }));
    assert!(got[0].2.starts_with("rewritten: [redacted]-out") && !got[0].2.contains("secret"), "{}", got[0].2);
    let shown = notices(&llm);
    assert_eq!(shown.len(), 1);
    assert!(shown[0].ends_with(&got[0].2), "the model saw exactly the logged content: {}", shown[0]);
    assert_eq!(audit(&s), vec![("shaper".to_string(), "replace".to_string())]);
    let offered = seen(&s);
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0]["op"], json!(op.0));
    assert!(offered[0]["content"].to_string().contains("secret-out"), "the chain sees the tool's own output: {}", offered[0]);
    delivered_at_turn_start(&s, op, "what happened?");
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn a_delivery_callback_replaces_what_the_model_sees() {
    replaced_via("delivery").await;
}

#[tokio::test]
async fn a_job_done_hook_replaces_what_the_model_sees() {
    replaced_via("hook").await;
}

// ── suppress ──

async fn suppressed_via(via: &str) {
    let r = shaper(via, "suppress");
    let (s, llm, op) = finished_job(&r, vec![Step::text("noted")]).await;
    assert_eq!(s.prompt("what happened?", None).await.unwrap(), Transition::Completed);

    let got = delivered(&s);
    assert_eq!(got, vec![(op, OpStatus::Completed { code: 0 }, "Its output was suppressed by shaper.".to_string())], "never dropped");
    let shown = notices(&llm);
    assert_eq!(shown, vec![format!("Background job {} (sleep) completed:Its output was suppressed by shaper.", op.0)]);
    assert_eq!(audit(&s), vec![("shaper".to_string(), "suppress".to_string())]);
    delivered_at_turn_start(&s, op, "what happened?");
    let st = s.fold().unwrap();
    assert!(st.jobs.is_empty() && st.open_ops.is_empty(), "the job is closed like any other");
    assert!(replay_ok(&s) >= 3);
}

#[tokio::test]
async fn a_delivery_callback_suppresses_with_a_marker_and_the_entry_is_still_logged() {
    suppressed_via("delivery").await;
}

#[tokio::test]
async fn a_job_done_hook_suppresses_with_a_marker_and_the_entry_is_still_logged() {
    suppressed_via("hook").await;
}

// ── defer ──

async fn deferred_via(via: &str) {
    let r = shaper(via, "defer");
    let (s, llm, op) = finished_job(&r, vec![Step::text("nothing yet"), Step::text("there it is").expect_user("secret-out")]).await;
    let mut events = s.subscribe();
    assert_eq!(s.prompt("anything?", None).await.unwrap(), Transition::Completed);
    assert!(delivered(&s).is_empty(), "held: nothing delivered this turn");
    assert!(notices(&llm).is_empty());
    let job = s.jobs().unwrap();
    assert_eq!((job.len(), job[0].op, &job[0].status), (1, op, &Some(OpStatus::Completed { code: 0 })), "finished, still open");

    let started = Instant::now();
    assert_eq!(announced(&mut events).await, op, "the hold ends and the delivery is announced again");
    assert!(started.elapsed() < HOLD + Duration::from_millis(250), "announced when the hold ends, not later");
    assert_eq!(s.prompt("and now?", None).await.unwrap(), Transition::Completed);

    let got = delivered(&s);
    assert_eq!(got.len(), 1);
    assert!(got[0].2.starts_with("secret-out"), "delivered as it was: {}", got[0].2);
    let decisions: Vec<String> = audit(&s).into_iter().map(|(_, d)| d).collect();
    assert_eq!(decisions.len(), 2, "{decisions:?}");
    assert!(decisions[0].starts_with("defer ") && decisions[1] == "keep", "{decisions:?}");
    let counts: Vec<Value> = seen(&s).iter().map(|v| v["deferred"].clone()).collect();
    assert_eq!(counts, vec![json!(0), json!(1)], "the second offer knows it was deferred once");
    delivered_at_turn_start(&s, op, "and now?");
    assert!(replay_ok(&s) >= 4);
}

#[tokio::test]
async fn a_delivery_callback_defers_and_the_job_is_offered_again_after_its_hold() {
    deferred_via("delivery").await;
}

#[tokio::test]
async fn a_job_done_hook_defers_and_the_job_is_offered_again_after_its_hold() {
    deferred_via("hook").await;
}

// ── the hook payload ──

#[tokio::test]
async fn a_job_done_hook_gets_bounded_content_and_the_model_still_gets_all_of_it() {
    let r = shaper("hook", "keep");
    let long = "x".repeat(6_000);
    let (s, _, op) = finished_job_with(&r, &long, vec![Step::text("noted")]).await;
    assert_eq!(s.prompt("what happened?", None).await.unwrap(), Transition::Completed);

    let p = seen(&s).remove(0);
    assert_eq!(p["content"].as_str().unwrap().chars().count(), 4096, "cut to the payload bound");
    assert_eq!(p["content_truncated"], json!(true));
    let call =
        entries(&s).into_iter().find_map(|e| if let EntryKind::JobDelivered { call, .. } = e.kind { Some(call) } else { None }).unwrap();
    assert_eq!((&p["op"], &p["call"], &p["tool"]), (&json!(op.0), &json!(call), &json!("sleep")));
    assert_eq!(
        (&p["status"]["status"], &p["cause"]["cause"], &p["deferred"], &p["with"]),
        (&json!("completed"), &json!("requested"), &json!(0), &json!([op.0]))
    );
    assert!(delivered(&s)[0].2.starts_with(&long), "kept: the delivery itself is not cut");
    assert!(replay_ok(&s) >= 3);
}

// ── order ──

#[tokio::test]
async fn callbacks_run_before_hooks_each_in_start_order_and_a_suppress_ends_the_chain() {
    // Composition order is not the chain order: capabilities start by id within a layer.
    let r = rig(base().consume::<TagB>().consume::<TagHook>().consume::<TagA>(), "");
    let (s, _, op) = finished_job(&r, vec![Step::text("noted")]).await;
    assert_eq!(s.prompt("what happened?", None).await.unwrap(), Transition::Completed);
    let got = delivered(&s);
    assert!(got[0].2.starts_with("secret-out") && got[0].2.ends_with(" tag-a tag-b tag-0"), "{}", got[0].2);
    let caps: Vec<String> = audit(&s).into_iter().map(|(c, _)| c).collect();
    assert_eq!(caps, ["tag-a", "tag-b", "tag-0"]);
    delivered_at_turn_start(&s, op, "what happened?");
    assert!(replay_ok(&s) >= 3);

    // `shaper` starts first and suppresses: no later callback or hook is consulted.
    let r = rig(base().consume::<TagB>().consume::<Shaper>().consume::<TagHook>(), "[shaper]\nvia = \"delivery\"\naction = \"suppress\"\n");
    let (s, _, _) = finished_job(&r, vec![Step::text("noted")]).await;
    assert_eq!(s.prompt("what happened?", None).await.unwrap(), Transition::Completed);
    assert_eq!(audit(&s), vec![("shaper".to_string(), "suppress".to_string())]);
    assert_eq!(delivered(&s)[0].2, "Its output was suppressed by shaper.");
    assert!(replay_ok(&s) >= 3);
}
