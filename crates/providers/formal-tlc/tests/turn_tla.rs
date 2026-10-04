//! `aho/formal/Turn.tla` with the real TLC, for session-scoped jobs: both models pass
//! (`Turn.cfg`, the loop with jobs off; `TurnJobs.cfg`, jobs on a smaller loop); a copy without a
//! job guard is caught by the invariant that guard keeps; a log with jobs, appended through the
//! real store and mapped by `aho_formal::trace_of`, is a behavior of the spec, while the same log
//! with a delivery where the session never appends one, or a finished job closed undelivered, is
//! not; and the logs of scripted sessions with jobs, run by the real `Session` (detach, delivery
//! at a boundary and at the next turn's start, abort scopes, restart recovery, rewind, a job that
//! finishes during a rate-limited retry), are behaviors of `Turn.tla`, `Log.tla` and `Policy.tla`.
//!
//! Needs `java` and `tla2tools.jar`: `AHO_TLA_JAR`, else `target/tmp/tla2tools.jar` (where
//! `real_tlc` downloads it), else `~/.aho/tools/tla2tools.jar`. Each TLC test prints why it
//! skipped when either is missing.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use aho_formal::{CheckConfig, Formal, Outcome, SpecRef, Verdict};
use aho_log::{CancelReason, DetachCause, Entry, EntryKind, LogStore, MemoryLog, OpKind, OpStatus, ToolOutcome, Transition, fold};
use aho_proc::SubprocessHandle;
use aho_types::{
    AgentId, CallId, Consequence, ConsequenceSet, ContentBlock, Epoch, Form, Hash, OpId, Principal, StepId, StopReason, TurnId, Usage,
};
use formal_tlc::{Tlc, TlcConfig, model};
use serde_json::{Value, json};

fn formal_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../formal")
}

fn spec() -> SpecRef {
    SpecRef::tla(formal_dir().join("Turn.tla"))
}

fn source(name: &str) -> String {
    std::fs::read_to_string(formal_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The model as checked here: the file's settings with a budget sized for a loaded CI host.
fn cfg(name: &str) -> CheckConfig {
    CheckConfig { budget_ms: 600_000, ..aho_formal::tla::parse_cfg(&source(name)).unwrap() }
}

/// TLC runs one at a time: each already uses every worker the host has.
static TLC_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn jar() -> Option<PathBuf> {
    [
        std::env::var_os("AHO_TLA_JAR").map(PathBuf::from),
        Some(Path::new(env!("CARGO_TARGET_TMPDIR")).join("tla2tools.jar")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".aho/tools/tla2tools.jar")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

/// The real TLC, or `None` (with the reason printed) when this host cannot run it.
fn tlc() -> Option<Tlc> {
    let Some(jar) = jar() else {
        eprintln!(
            "skipped: no tla2tools.jar (set AHO_TLA_JAR, or run `cargo test -p formal-tlc --test real_tlc -- --ignored` once to download \
             it into target/tmp)"
        );
        return None;
    };
    if !Command::new("java").arg("-version").output().map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skipped: `java` does not run on this host, and TLC needs a Java runtime");
        return None;
    }
    let proc = SubprocessHandle(Arc::new(world_host::HostSubprocess::new(&formal_dir())));
    let workers = std::thread::available_parallelism().map(|n| n.get().min(8) as u32).unwrap_or(1);
    let fs = aho_fs::FsWrite(Arc::new(world_host::HostFs::new(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")).unwrap()));
    Some(Tlc::new(proc, fs, TlcConfig { jar, java: "java".into(), workers, budget_ms: 300_000, max_mutants: 0 }))
}

/// TLC's closing count line, for the record.
fn states(o: &Outcome) -> String {
    o.output.lines().rev().find(|l| l.contains("distinct states found")).unwrap_or("").trim().to_string()
}

/// The delivery guard of `DeliverAs`: at a request boundary, after the result that detached it.
const DELIVER_GUARD: &str = concat!("    /\\ phase = \"prep\"\n", "    /\\ j.call \\notin open\n");

/// What each guard keeps out: `(what, guard text, text without it, the invariant that catches it)`.
const MUTANTS: &[(&str, &str, &str, &str)] = &[
    ("a delivery while the turn waits", DELIVER_GUARD, "    /\\ j.call \\notin open\n", "NoDeliveryBetweenCallAndResult"),
    ("a delivery before the result that detached it", DELIVER_GUARD, "    /\\ phase = \"prep\"\n", "NoDeliveryBetweenCallAndResult"),
    (
        "a turn that waits on its jobs",
        r#"THEN "idle" ELSE "wait""#,
        r#"THEN (IF jobs = {} THEN "idle" ELSE "wait") ELSE "wait""#,
        "IdleIffNothingPending",
    ),
    (
        "a cancelled job whose late finish overwrites its cancellation",
        r#"j.op = o /\ j.st = "running" /\ SetJob(j, "done")"#,
        r#"j.op = o /\ SetJob(j, "done")"#,
        "NoDeliveryAfterCancel",
    ),
    (
        "a detach past the lease",
        r#"Detach(o, c) == Cardinality(Running) < MaxJobs /\ DetachOp(o, c)"#,
        r#"Detach(o, c) == DetachOp(o, c)"#,
        "JobWithinLease",
    ),
    (
        "an OpDone without its delivery",
        r#"j.op = o /\ j.st = "delivered" /\ jobs' = jobs \ {j}"#,
        r#"j.op = o /\ jobs' = jobs \ {j}"#,
        "JobEndsOnce",
    ),
    (
        "a second delivery",
        r#"j.st \in {"done", "cancelled"} /\ DeliverAs"#,
        r#"j.st \in {"done", "cancelled", "delivered"} /\ DeliverAs"#,
        "JobEndsOnce",
    ),
];

#[test]
fn turn_tla_has_what_the_job_checks_need() {
    let src = source("Turn.tla");
    for name in ["Turn.cfg", "TurnJobs.cfg"] {
        for inv in &cfg(name).invariants {
            assert!(model::defines(&src, inv), "{name} checks {inv}, which Turn.tla does not define");
        }
    }
    let jobs = cfg("TurnJobs.cfg");
    for inv in ["JobEndsOnce", "NoDeliveryAfterCancel", "NoDeliveryBetweenCallAndResult", "JobWithinLease", "IdleIffNothingPending"] {
        assert!(jobs.invariants.iter().any(|i| i == inv), "TurnJobs.cfg does not check {inv}");
    }
    let lease = jobs.constants.iter().find(|(k, _)| k == "MaxJobs").map(|(_, v)| v.as_str());
    assert!(lease.is_some_and(|v| v != "0"), "TurnJobs.cfg lets no op detach: {lease:?}");
    for (what, guard, _, _) in MUTANTS {
        assert_eq!(src.matches(guard).count(), 1, "the guard against {what} moved: {guard}");
    }
}

#[tokio::test]
async fn turn_tla_passes_and_each_job_guard_is_load_bearing() {
    let Some(t) = tlc() else { return };
    let _turn = TLC_TURN.lock().await;
    for name in ["Turn.cfg", "TurnJobs.cfg"] {
        let o = t.check(&spec(), &cfg(name)).await.unwrap();
        assert_eq!(o.verdict, Verdict::Pass, "{name}: {}", o.output);
        println!("{name}: Pass, {:?}, {} ms, {}", o.mode, o.cost_ms, states(&o));
    }
    let jobs = cfg("TurnJobs.cfg");
    for (what, guard, without, inv) in MUTANTS {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        std::fs::write(dir.path().join("Turn.tla"), source("Turn.tla").replace(guard, without)).unwrap();
        let o = t.check(&SpecRef::tla(dir.path().join("Turn.tla")), &jobs).await.unwrap();
        let cx = o.counterexample.unwrap_or_else(|| panic!("{what} went unnoticed: {:?}\n{}", o.verdict, o.output));
        assert_eq!(cx.violated, format!("invariant {inv} of Turn"), "{what}:\n{}", cx.render());
        println!("{what}: {} after {} states", cx.violated, cx.states.len());
    }
}

#[tokio::test]
async fn a_job_log_is_a_behavior_and_a_misplaced_delivery_is_not() {
    let entries = job_session();
    let trace = aho_formal::trace_of(&entries);
    let Some(t) = tlc() else { return };
    let _turn = TLC_TURN.lock().await;
    behaves(&t, "a hand-built job log", &entries).await;

    let at = |action: &str, key: &str, v: Value| trace.iter().position(|r| r["action"] == action && r[key] == v).unwrap();
    let (detach, deliver, cancelled) = (at("Detach", "op", json!(1)), at("Deliver", "op", json!(1)), at("Deliver", "op", json!(3)));
    let waiting = at("Result", "call", json!("c2"));
    let tampered = [
        ("a delivery between the detach and its result", moved(&trace, deliver, Some(detach + 1)), "JobDelivered"),
        ("a delivery while the turn waits on another call", moved(&trace, deliver, Some(waiting)), "JobDelivered"),
        ("a finished job closed without its delivery", moved(&trace, deliver, None), "OpDone"),
        ("a delivery after the job's OpDone", moved(&trace, cancelled, Some(cancelled + 1)), "JobDelivered"),
    ];
    for (what, bad, refused) in tampered {
        let o = t.trace_validate(&spec(), &bad).await.unwrap();
        let cx = o.counterexample.unwrap_or_else(|| panic!("Turn.tla accepts {what}: {}", o.output));
        assert!(cx.violated.ends_with("is not a step of Turn"), "{what}: {}", cx.violated);
        let id = cx.violated.split("(entry ").nth(1).and_then(|s| s.split(')').next()).and_then(|s| s.parse::<u64>().ok()).unwrap();
        let entry = entries.iter().find(|e| e.id.0 == id).unwrap();
        assert_eq!(entry.kind.tag(), refused, "{what}: refused at {entry:?}");
        println!("{what}: {}", cx.violated);
    }
}

/// `entries` is a behavior of every aho spec whose actions jobs touch.
async fn behaves(t: &Tlc, what: &str, entries: &[Entry]) {
    let trace = aho_formal::trace_of(entries);
    for module in ["Turn", "Log", "Policy"] {
        let o = t.trace_validate(&SpecRef::tla(formal_dir().join(format!("{module}.tla"))), &trace).await.unwrap();
        assert_eq!(o.verdict, Verdict::Pass, "{module}.tla rejects {what}: {:?}\n{}", o.counterexample, o.output);
    }
    println!("{what}: {} entries, a behavior of Turn, Log and Policy", entries.len());
}

/// `trace` with record `i` moved to position `to` (in the shortened trace), or dropped.
fn moved(trace: &[Value], i: usize, to: Option<usize>) -> Vec<Value> {
    let mut t = trace.to_vec();
    let r = t.remove(i);
    if let Some(to) = to {
        t.insert(to, r);
    }
    t
}

/// A legal log with jobs, appended through the real store (fold-checked), in the order the session
/// appends them: c1 detached at start and delivered at the next step's top; c3 detached after a
/// while, the turn completing while it runs, cancelled by abort-all between turns and delivered
/// at the next turn's start; c4 re-dispatched by a restart (its op closed, a fresh op started on
/// the closed call and detached) and then closed undelivered by a rewind.
fn job_session() -> Vec<Entry> {
    let log = MemoryLog::new();
    let ap = |by: &Principal, k: EntryKind| log.append(None, by.clone(), k).map(|e| e.id).unwrap();
    let (sys, jobs, ops) = (Principal::system("loop"), Principal::system("jobs"), Principal::system("ops"));
    let (model, agent) = (Principal::system("model"), Principal::Agent { id: AgentId(1), turn: TurnId(1) });
    let c = |id: &str| CallId(id.into());
    let request = || EntryKind::RequestHeader {
        model: "m".into(),
        tools_visible: vec![],
        policy_epoch: Epoch(1),
        plan_hash: Hash::ZERO,
        section_hashes: vec![],
        open_calls: vec![],
        request_hash: Hash::ZERO,
    };
    let response = |ids: &[&str]| EntryKind::AssistantMessage {
        content: ids.iter().map(|i| ContentBlock::ToolUse { id: c(i), name: "bash".into(), args: json!({}) }).collect(),
        usage: Usage::default(),
        stop: if ids.is_empty() { StopReason::EndTurn } else { StopReason::ToolUse },
    };
    let call = |id: &str| EntryKind::ToolCall { call: c(id), tool: "bash".into(), args: json!({}), class: Consequence::SpawnsProcess };
    let result = |id: &str| EntryKind::ToolResult {
        call: c(id),
        outcome: ToolOutcome::Ok,
        value: json!(null),
        content: vec![ContentBlock::text("ok")],
        spill_ref: None,
        meta: None,
        late: false,
    };
    let started =
        |op, id: &str| EntryKind::OpStarted { op: OpId(op), call: c(id), kind: OpKind::Process, rerunnable: true, external_effect: false };
    let detached = |op, id: &str, cause| EntryKind::OpDetached { op: OpId(op), call: c(id), cause };
    let delivered = |op, id: &str, status: &OpStatus| EntryKind::JobDelivered {
        op: OpId(op),
        call: c(id),
        tool: "bash".into(),
        status: status.clone(),
        content: vec![ContentBlock::text("out")],
        spill_ref: None,
    };
    let done = |op, status: &OpStatus| EntryKind::OpDone { op: OpId(op), status: status.clone() };
    let prompt = || EntryKind::UserMessage { form: Form::Prompt, text: "go".into(), client_key: None };
    let step = |n, t| EntryKind::StepEnd { step: StepId(n), transition: t };
    let ok = OpStatus::Completed { code: 0 };
    let killed = OpStatus::Cancelled { reason: CancelReason::Aborted };

    let allowed = ConsequenceSet::of(&[Consequence::SpawnsProcess]);
    ap(&sys, EntryKind::PolicyEpoch { epoch: Epoch(1), allowed, preset: "p".into(), approval_ref: None });
    // turn 1, step 1: c1 detached at start, c2 a turn op
    ap(&sys, EntryKind::TurnStart { turn: TurnId(1) });
    ap(&Principal::Operator, prompt());
    ap(&sys, EntryKind::StepStart { step: StepId(1) });
    ap(&sys, request());
    ap(&model, response(&["c1", "c2"]));
    ap(&agent, call("c1"));
    ap(&ops, started(1, "c1"));
    ap(&jobs, detached(1, "c1", DetachCause::Requested));
    ap(&jobs, result("c1"));
    ap(&agent, call("c2"));
    ap(&ops, started(2, "c2"));
    ap(&ops, result("c2"));
    ap(&ops, done(2, &ok));
    ap(&sys, step(1, Transition::ToolUse));
    // step 2's top: c1 finished meanwhile; c3 detaches after a while
    ap(&jobs, delivered(1, "c1", &ok));
    ap(&jobs, done(1, &ok));
    ap(&sys, EntryKind::StepStart { step: StepId(2) });
    ap(&sys, request());
    ap(&model, response(&["c3"]));
    ap(&agent, call("c3"));
    ap(&ops, started(3, "c3"));
    ap(&jobs, detached(3, "c3", DetachCause::After(60_000)));
    ap(&jobs, result("c3"));
    ap(&sys, step(2, Transition::ToolUse));
    // step 3: the turn completes while c3 runs
    ap(&sys, EntryKind::StepStart { step: StepId(3) });
    ap(&sys, request());
    ap(&model, response(&[]));
    ap(&sys, step(3, Transition::Completed));
    ap(&sys, EntryKind::TurnEnd { turn: TurnId(1) });
    // abort(All) while idle cancelled c3: turn 2 starts with its cancellation, then detaches c4
    ap(&sys, EntryKind::TurnStart { turn: TurnId(2) });
    ap(&Principal::Operator, prompt());
    ap(&jobs, delivered(3, "c3", &killed));
    ap(&jobs, done(3, &killed));
    ap(&sys, EntryKind::StepStart { step: StepId(1) });
    ap(&sys, request());
    ap(&model, response(&["c4"]));
    ap(&agent, call("c4"));
    ap(&ops, started(4, "c4"));
    ap(&jobs, detached(4, "c4", DetachCause::Model));
    ap(&jobs, result("c4"));
    ap(&sys, step(1, Transition::ToolUse));
    ap(&sys, EntryKind::StepStart { step: StepId(2) });
    ap(&sys, request());
    ap(&model, response(&[]));
    ap(&sys, step(2, Transition::Completed));
    ap(&sys, EntryKind::TurnEnd { turn: TurnId(2) });
    // a restart re-dispatches c4 under a fresh op; a rewind then closes it undelivered
    let recovery = Principal::system("recovery");
    ap(&recovery, done(4, &OpStatus::Failed { err: "re-dispatched after restart".into() }));
    ap(&recovery, started(5, "c4"));
    ap(&recovery, detached(5, "c4", DetachCause::Model));
    ap(&Principal::system("rewind"), done(5, &OpStatus::Cancelled { reason: CancelReason::Rewind }));
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    fold(&entries, &Default::default()).unwrap();
    entries
}

// ───────────────────────────── live sessions ─────────────────────────────

/// Scripted sessions run by the real `Session` (the host world, a test tool that sleeps), whose
/// logs must be behaviors of the specs as they stand.
mod live {
    use std::sync::Arc;
    use std::time::Duration;

    use aho_approval::NeverApproval;
    use aho_cap::*;
    use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
    use aho_llm::{AdapterInfo, Llm, LlmError, LlmRoute, Request, Response, Sink};
    use aho_log::{Entry, EntryKind, JsonlLog, LogStore, MemoryLog, OpKind, RewindMode, Transition};
    use aho_session::{AbortScope, Boot, Session, SessionEvent};
    use aho_types::*;
    use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
    use serde_json::json;

    use aho_formal::Formal;

    use super::{TLC_TURN, behaves, tlc};

    struct JobTools;
    static JOB_TOOLS: &[ToolDecl] = &[
        ToolDecl::new("sleep", "Sleeps, then answers.", Consequence::ReadOnly).safe().schema(schema).detachable().op(
            OpKind::Timer,
            false,
            false,
        ),
        ToolDecl::new("redo", "Sleeps, then answers; safe to re-run.", Consequence::ReadOnly).safe().schema(schema).detachable().op(
            OpKind::Timer,
            true,
            false,
        ),
        ToolDecl::new("nap", "Sleeps; never a job.", Consequence::ReadOnly).safe().schema(schema),
    ];

    fn schema(_: &BoundWorld) -> RootSchema {
        RootSchema(json!({"type": "object", "properties": {"ms": {"type": "integer"}, "text": {"type": "string"}}}))
    }

    /// Sleeps `ms` (honouring cancel), then answers `text`.
    fn body() -> ToolExec {
        Arc::new(|req: ExecRequest| {
            Box::pin(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_millis(req.view.args["ms"].as_u64().unwrap_or(0));
                while tokio::time::Instant::now() < deadline {
                    if req.cancel.is_cancelled() {
                        return ExecOutcome::Cancelled;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                ExecOutcome::text(req.view.args["text"].as_str().unwrap_or("").to_string())
            })
        })
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
            for d in JOB_TOOLS {
                r.tool(d, body());
            }
        }
    }
    declare_capability!(JobTools, "job-tools", kind = Kind::Tool, provides = [], requires = [], tools = JOB_TOOLS);

    struct Rig {
        comp: Composition,
        plan: Plan,
        loadout: aho_config::Loadout,
        root: std::path::PathBuf,
        home: std::path::PathBuf,
        dir: tempfile::TempDir,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let (root, home) = (dir.path().join("ws"), dir.path().join("home"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
        let comp = Composition::new("turn-jobs").provide::<world_host::HostWorld>().consume::<JobTools>();
        let mut schema = Schema::default();
        for k in comp.config_keys() {
            schema.add(k);
        }
        let text = format!(
            "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 150\nheartbeat_s = 30\n[workspace]\nroot = \"{}\"\n\
             shadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n",
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
        Rig { comp, plan, loadout, root, home, dir }
    }

    async fn boot(r: &Rig, steps: Vec<Step>, log: Arc<dyn LogStore>) -> Arc<Session> {
        boot_with(r, Arc::new(ScriptedLlm::from_steps(steps)), log).await
    }

    /// The scripted model whose request number `.1` (from 0) fails as rate limited for `.2` ms,
    /// consuming no step; `.3` counts requests.
    struct RateLimitedOnce(ScriptedLlm, usize, u64, std::sync::atomic::AtomicUsize);

    impl Llm for RateLimitedOnce {
        fn info(&self) -> AdapterInfo {
            self.0.info()
        }
        fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
            if self.3.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == self.1 {
                let ms = self.2;
                return Box::pin(async move { Err(LlmError::RateLimited { retry_after_ms: ms }) });
            }
            self.0.stream(req, sink)
        }
    }

    async fn boot_with(r: &Rig, llm: Arc<dyn Llm>, log: Arc<dyn LogStore>) -> Arc<Session> {
        let mut seed = Registry::default();
        seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm)));
        Session::boot(Boot {
            id: SessionId(17),
            composition: &r.comp,
            plan: &r.plan,
            loadout: &r.loadout,
            bootstrap: vec![EntryKind::PolicyEpoch {
                epoch: Epoch(1),
                allowed: r.plan.allowed,
                preset: r.plan.preset.clone(),
                approval_ref: None,
            }],
            log,
            approval: Arc::new(NeverApproval),
            operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
            workspace: r.root.display().to_string(),
            home: r.home.display().to_string(),
            spill_dir: r.home.join("spill"),
            seed,
        })
        .await
        .unwrap()
    }

    fn entries(s: &Session) -> Vec<Entry> {
        s.log().read_branch(s.log().head().unwrap()).unwrap()
    }

    /// `(op, status)` of every `JobDelivered`, the status as the trace names it.
    fn delivered(es: &[Entry]) -> Vec<(OpId, String)> {
        aho_formal::trace_of(es)
            .into_iter()
            .filter(|r| r["action"] == "Deliver")
            .map(|r| (OpId(r["op"].as_u64().unwrap()), r["status"].as_str().unwrap().to_string()))
            .collect()
    }

    fn detached(es: &[Entry]) -> Vec<OpId> {
        es.iter().filter_map(|e| if let EntryKind::OpDetached { op, .. } = e.kind { Some(op) } else { None }).collect()
    }

    async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..500 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn settled(s: &Session) -> bool {
        s.jobs().unwrap().iter().all(|j| j.status.is_some())
    }

    #[tokio::test]
    async fn a_session_whose_jobs_land_at_boundaries_and_survive_a_turn_abort_is_a_behavior() {
        let Some(t) = tlc() else { return };
        let r = rig();
        let s = boot(
            &r,
            vec![
                Step::tool_call("sleep", json!({"ms": 300, "text": "bg-done", "background": true}))
                    .with_call(None, "nap", json!({"ms": 900, "text": "nap-done"}))
                    .with_stop(ScriptedStop::ToolUse),
                Step::text("both are in").expect_user("bg-done"),
                Step::tool_call("sleep", json!({"ms": 400, "text": "idle-done", "background": true})).with_stop(ScriptedStop::ToolUse),
                Step::text("started").expect_tool_result("background as job"),
                Step::tool_call("sleep", json!({"ms": 5000, "text": "never", "background": true}))
                    .with_call(None, "nap", json!({"ms": 5000}))
                    .with_stop(ScriptedStop::ToolUse)
                    .expect_user("idle-done"),
                Step::text("noted").expect_user("cancelled"),
            ],
            Arc::new(MemoryLog::new()),
        )
        .await;
        // a job delivered at a step boundary of its own turn
        assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
        // a job that finishes while idle, delivered when the next turn starts
        let mut events = s.subscribe();
        assert_eq!(s.prompt("start it", None).await.unwrap(), Transition::Completed);
        let finished = tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(events.recv().await, Ok(SessionEvent::JobFinished { .. })) {}
        });
        finished.await.expect("the idle session announces its finished job");
        // abort(Turn) leaves the next job running; abort(All) cancels it, delivered at the next turn
        let turn = tokio::spawn({
            let s = s.clone();
            async move { s.prompt("what happened?", None).await }
        });
        eventually("the turn to wait on nap", || detached(&entries(&s)).len() == 3 && s.ops().running().len() == 2).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        s.abort();
        let t3 = tokio::time::timeout(Duration::from_secs(5), turn).await.unwrap().unwrap().unwrap();
        assert!(matches!(t3, Transition::Aborted(_)), "{t3:?}");
        assert!(s.abort_scope(AbortScope::All));
        eventually("the job to stop", || settled(&s)).await;
        assert_eq!(s.prompt("status?", None).await.unwrap(), Transition::Completed);

        let es = entries(&s);
        let statuses: Vec<String> = delivered(&es).into_iter().map(|d| d.1).collect();
        assert_eq!(statuses, ["completed", "completed", "cancelled"], "the run exercised every delivery it claims");
        let _turn = TLC_TURN.lock().await;
        behaves(&t, "a scripted session with jobs", &es).await;
    }

    #[tokio::test]
    async fn a_restarted_session_that_re_dispatches_and_fails_its_jobs_is_a_behavior() {
        let Some(t) = tlc() else { return };
        let r = rig();
        let first = r.dir.path().join("first.jsonl");
        let steps = vec![
            Step::tool_call("redo", json!({"ms": 5000, "text": "redone", "background": true}))
                .with_call(None, "sleep", json!({"ms": 5000, "text": "lost", "background": true}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("both running"),
        ];
        let s = boot(&r, steps, Arc::new(JsonlLog::open(&first).unwrap())).await;
        assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
        // the process "dies" here: a copy of the log is what the next boot finds
        let crashed = r.dir.path().join("crashed.jsonl");
        std::fs::copy(&first, &crashed).unwrap();
        s.abort_scope(AbortScope::All);

        let steps = vec![Step::text("one was lost"), Step::text("the other was stopped")];
        let s = boot(&r, steps, Arc::new(JsonlLog::open(&crashed).unwrap())).await;
        let redo = *detached(&entries(&s)).last().unwrap();
        assert_eq!(detached(&entries(&s)).len(), 3, "the re-runnable job was re-dispatched as a job at load");
        assert_eq!(s.prompt("after the restart", None).await.unwrap(), Transition::Completed);
        s.abort_scope(AbortScope::Op(redo));
        eventually("the re-run job to stop", || settled(&s)).await;
        assert_eq!(s.prompt("and the other?", None).await.unwrap(), Transition::Completed);

        let es = entries(&s);
        let statuses: Vec<String> = delivered(&es).into_iter().map(|d| d.1).collect();
        assert_eq!(statuses, ["failed", "cancelled"], "the lost job failed, the re-run one was stopped");
        let _turn = TLC_TURN.lock().await;
        behaves(&t, "a session recovered after a restart", &es).await;
    }

    #[tokio::test]
    async fn a_rewind_that_cancels_a_running_job_is_a_behavior() {
        let Some(t) = tlc() else { return };
        let r = rig();
        let steps = vec![
            Step::tool_call("sleep", json!({"ms": 1200, "text": "kept", "background": true})).with_stop(ScriptedStop::ToolUse),
            Step::text("running"),
            Step::text("after the rewind"),
        ];
        let s = boot(&r, steps, Arc::new(MemoryLog::new())).await;
        assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
        let c = s.checkpoint_now().await.expect("the host world snapshots");
        let operator = aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty());
        s.rewind(&operator, c, RewindMode::ChatOnly).await.unwrap();
        let job = detached(&entries(&s))[0];
        eventually("the rewound-away job to stop", || s.ops().status(job) != Some(None)).await;
        assert_eq!(s.prompt("next", None).await.unwrap(), Transition::Completed);

        let es = entries(&s);
        assert_eq!((detached(&es).len(), delivered(&es).len()), (1, 0), "the rewound-away job delivers nothing");
        let closed =
            aho_formal::trace_of(&es).into_iter().any(|r| r["action"] == "OpDone" && r["op"] == job.0 && r["status"] == "cancelled");
        assert!(closed, "the rewind closes the job on the new branch");
        let _turn = TLC_TURN.lock().await;
        behaves(&t, "a session rewound under a running job", &es).await;
    }

    /// A retry after a rate limit logs no `StepEnd`: the job that finished meanwhile is delivered
    /// after the retry's `StepStart`, so the loop is preparing a request, not still in flight.
    #[tokio::test]
    async fn a_job_that_finishes_while_a_rate_limited_request_waits_is_a_behavior() {
        let Some(t) = tlc() else { return };
        let r = rig();
        let steps = vec![
            Step::tool_call("sleep", json!({"ms": 150, "text": "rl-done", "background": true})).with_stop(ScriptedStop::ToolUse),
            Step::text("heard it").expect_user("rl-done"),
        ];
        let llm = Arc::new(RateLimitedOnce(ScriptedLlm::from_steps(steps), 1, 500, Default::default()));
        let s = boot_with(&r, llm.clone(), Arc::new(MemoryLog::new())).await;
        assert_eq!(s.prompt("go", None).await.unwrap(), Transition::Completed);
        assert_eq!((llm.0.remaining(), llm.3.load(std::sync::atomic::Ordering::SeqCst)), (0, 3), "the limited request was retried");

        let es = entries(&s);
        assert_eq!(delivered(&es), [(detached(&es)[0], "completed".to_string())]);
        let steps = es.iter().filter(|e| matches!(e.kind, EntryKind::StepStart { .. })).count();
        let ends = es.iter().filter(|e| matches!(e.kind, EntryKind::StepEnd { .. })).count();
        assert!(steps > ends, "the retry started a step without ending the limited one: {steps} starts, {ends} ends");
        let _turn = TLC_TURN.lock().await;
        behaves(&t, "a session whose job finished during a rate-limited retry", &es).await;

        // the order before the fix: the delivery ahead of the retry's StepStart, while in flight
        let trace = aho_formal::trace_of(&es);
        let deliver = trace.iter().position(|r| r["action"] == "Deliver").unwrap();
        let retry = trace[..deliver].iter().rposition(|r| r["action"] == "StepStart").unwrap();
        let o = t.trace_validate(&super::spec(), &super::moved(&trace, deliver, Some(retry))).await.unwrap();
        let cx = o.counterexample.unwrap_or_else(|| panic!("Turn.tla accepts a delivery while a request is in flight: {}", o.output));
        assert!(cx.violated.contains("Deliver") && cx.violated.ends_with("is not a step of Turn"), "{}", cx.violated);
    }
}
