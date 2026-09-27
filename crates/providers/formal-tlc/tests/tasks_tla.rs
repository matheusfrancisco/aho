//! `aho/formal/Tasks.tla` with the real TLC: the model passes; a copy whose gates
//! may pass over an unaddressed LOW sibling is caught with a counterexample; with invariant
//! evidence (`TasksVerify.cfg`) it passes, and a copy whose verify gate may pass over a failing or
//! stale invariant is caught; generic mutants of its actions are caught; and a session log holding
//! an aho-tasks simulator run, written and read back through `JsonlLog` and mapped by
//! `aho_formal::trace_for`, is a behavior of the spec, while the same log with one tampered
//! artifact, or with the invariant check made of other code, is not.
//!
//! Needs `java` and `tla2tools.jar`: `AHO_TLA_JAR`, else `target/tmp/tla2tools.jar` (where
//! `real_tlc` downloads it), else `~/.aho/tools/tla2tools.jar`. Each TLC test prints why it
//! skipped when either is missing.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;

use aho_formal::{CheckConfig, CheckMode, Formal, Outcome, SpecRef, Verdict};
use aho_log::{CheckpointCause, Entry, EntryKind, JsonlLog, LogStore};
use aho_proc::SubprocessHandle;
use aho_tasks::sim::{Action, Script, Sim, Turn};
use aho_tasks::{
    Actor, ChildSpec, Control, Event, Graph, HandoffArtifact, Injection, InvariantSeen, InvariantStatus, Kind, ROOT_GATE, Salvage,
    SeedSpec, Settings, Status, TaskError, TaskId,
};
use aho_types::{AgentId, CheckpointId, Hash, Principal};
use formal_tlc::{Tlc, TlcConfig, model, mutate};
use serde_json::json;

fn formal_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../formal")
}

fn spec() -> SpecRef {
    SpecRef::tla(formal_dir().join("Tasks.tla"))
}

fn source(name: &str) -> String {
    std::fs::read_to_string(formal_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The model as checked here: the file's settings with a budget sized for a loaded CI host (a full
/// `cargo test --workspace` runs TLC next to every other test binary).
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
fn tlc(max_mutants: usize) -> Option<Tlc> {
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
    Some(Tlc::new(proc, fs, TlcConfig { jar, java: "java".into(), workers, budget_ms: 300_000, max_mutants }))
}

/// TLC's closing count line, for the record.
fn states(o: &Outcome) -> String {
    o.output.lines().rev().find(|l| l.contains("distinct states found")).unwrap_or("").trim().to_string()
}

/// A copy of Tasks.tla with `guard` replaced by `TRUE`, next to its configs.
fn mutant(guard: &str) -> (tempfile::TempDir, SpecRef) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::write(dir.path().join("Tasks.tla"), source("Tasks.tla").replace(guard, "    /\\ TRUE\n")).unwrap();
    std::fs::write(dir.path().join("Tasks.cfg"), source("Tasks.cfg")).unwrap();
    let spec = SpecRef::tla(dir.path().join("Tasks.tla"));
    (dir, spec)
}

/// The LOW-sibling guard of `Completed` (validate_gate's `UnaddressedLowConfidence`).
const LOW_GUARD: &str = "    /\\ IsGate(id) => \\A d \\in node[id].deps :\n                        (node[d].status = \"completed\" /\\ node[d].conf = \"low\") => Addressed(id, d)\n";
/// The covering-invariant guard of `Completed` (validate_gate's `InvariantNotPassing`).
const VERIFY_GUARD: &str = "    /\\ \\A s \\in seen : s.status \\notin Blocking\n";

#[test]
fn tasks_tla_has_what_checks_traces_and_mutants_need() {
    let src = source("Tasks.tla");
    assert_eq!(aho_formal::tla::module_name(&src).as_deref(), Some("Tasks"));
    for op in ["Init", "Next", "vars", "TraceStep", "TraceInv", "TypeOK"] {
        assert!(model::defines(&src, op), "Tasks.tla defines no {op}");
    }
    for name in ["Tasks.cfg", "TasksVerify.cfg"] {
        let cfg = cfg(name);
        assert!(!cfg.check_deadlock, "an all-terminal graph has no successor; that is not a deadlock");
        for inv in [
            "TypeOK",
            "Acyclic",
            "BlockedDerived",
            "GateCoversScope",
            "GateNotOverLow",
            "StaleScope",
            "ArtifactOrNothing",
            "ReclaimCapped",
            "VerifyNotOverFailingInvariant",
        ] {
            assert!(cfg.invariants.iter().any(|i| i == inv), "{name} does not check {inv}");
        }
        for inv in &cfg.invariants {
            assert!(model::defines(&src, inv), "{name} checks {inv}, which Tasks.tla does not define");
        }
    }
    let evidence = |name: &str, c: &str| cfg(name).constants.iter().find(|(k, _)| k == c).map(|(_, v)| v.clone());
    assert_eq!(evidence("Tasks.cfg", "Invs").as_deref(), Some("{}"), "trace validation reads Tasks.cfg: a log starts with no checks");
    assert_ne!(evidence("TasksVerify.cfg", "Invs").as_deref(), Some("{}"));
    assert!(src.contains(LOW_GUARD), "the LOW guard the mutant test removes moved");
    assert!(src.contains(VERIFY_GUARD), "the invariant guard the mutant test removes moved");
    let names: BTreeSet<String> = mutate::mutants(&src, &cfg("Tasks.cfg").next).into_iter().map(|m| m.name).collect();
    for m in
        ["drop-guard:Started#2", "drop-guard:Completed#5", "drop-guard:Requeued#2", "drop-guard:Reclaimed#4", "negate-guard:Released#3"]
    {
        assert!(names.contains(m), "no mutant {m} in {names:?}");
    }
}

#[tokio::test]
async fn tasks_tla_passes_and_a_gate_that_may_pass_over_low_is_caught() {
    let Some(t) = tlc(0) else { return };
    let _turn = TLC_TURN.lock().await;
    let cfg = cfg("Tasks.cfg");
    let o = t.check(&spec(), &cfg).await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass, "{}", o.output);
    assert!(matches!(o.mode, CheckMode::Bounded(d) if d >= 7), "explored {:?}", o.mode);
    println!("Tasks: Pass, {:?}, {} ms, {}", o.mode, o.cost_ms, states(&o));

    let (_dir, low) = mutant(LOW_GUARD);
    let o = t.check(&low, &cfg).await.unwrap();
    assert!(matches!(o.verdict, Verdict::Fail { .. }), "the mutant passed: {}", o.output);
    let cx = o.counterexample.unwrap();
    assert_eq!(cx.violated, "invariant GateNotOverLow of Tasks");
    // the last state has a passed gate over a LOW sibling and no injected follow-up
    let node = cx.states.last().unwrap()["tla"]["node"].as_str().unwrap().to_string();
    let injected = node.matches("injBy |-> ").count() - node.matches("injBy |-> \"\"").count();
    assert!(node.contains("status |-> \"completed\"") && node.contains("conf |-> \"low\"") && injected == 0, "{}", cx.render());
    println!("mutant without the LOW guard: {} after {} states\n{}", cx.violated, cx.states.len(), cx.render());
}

#[tokio::test]
async fn a_verify_gate_that_may_pass_over_a_failing_invariant_is_caught() {
    let Some(t) = tlc(0) else { return };
    let _turn = TLC_TURN.lock().await;
    let cfg = cfg("TasksVerify.cfg");
    let o = t.check(&spec(), &cfg).await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass, "{}", o.output);
    println!("Tasks with invariant evidence: Pass, {:?}, {} ms, {}", o.mode, o.cost_ms, states(&o));

    let (_dir, open) = mutant(VERIFY_GUARD);
    let o = t.check(&open, &cfg).await.unwrap();
    assert!(matches!(o.verdict, Verdict::Fail { .. }), "the mutant passed: {}", o.output);
    let cx = o.counterexample.unwrap();
    assert_eq!(cx.violated, "invariant VerifyNotOverFailingInvariant of Tasks");
    // the last state has a passed verify gate that saw a failing or stale invariant
    let node = cx.states.last().unwrap()["tla"]["node"].as_str().unwrap().to_string();
    assert!(node.contains("kind |-> \"gate:verify\"") && node.contains("status |-> \"completed\""), "{}", cx.render());
    assert!(node.contains("status |-> \"fail\"") || node.contains("status |-> \"stale\""), "{}", cx.render());
    println!("mutant without the invariant guard: {} after {} states\n{}", cx.violated, cx.states.len(), cx.render());
}

#[tokio::test]
async fn generic_mutants_of_tasks_tla_are_caught() {
    let Some(t) = tlc(8) else { return };
    let _turn = TLC_TURN.lock().await;
    let cfg = CheckConfig { mode: CheckMode::Bounded(6), ..cfg("Tasks.cfg") };
    let r = t.mutate(&spec(), &cfg).await.unwrap();
    for m in &r.mutants {
        println!("  {:<28} caught={} {:?}", m.name, m.caught, m.violated);
    }
    assert_eq!(r.verdict(), Verdict::Pass, "no mutant was caught: the invariants constrain nothing");
    assert!(r.caught() >= 4, "{} of {} caught", r.caught(), r.mutants.len());
    let caught: BTreeSet<&str> = r.mutants.iter().filter(|m| m.caught).map(|m| m.name.as_str()).collect();
    assert!(caught.contains("drop-guard:Started#2"), "starting a started node went unnoticed: {caught:?}");
}

#[tokio::test]
async fn a_simulator_run_in_a_session_log_is_a_behavior_of_tasks_tla() {
    let sim = scenario();
    let dir = tempfile::tempdir().unwrap();
    let tree = Hash::of(b"tree after the swarm's edits");
    let entries = written(&sim, &dir.path().join("swarm.jsonl"), tree);
    let trace = aho_formal::trace_for("Tasks", &entries);
    let seen: BTreeSet<&str> = trace.iter().filter_map(|r| r["action"].as_str()).collect();
    for name in Event::NAMES.iter().chain(&["InvariantChecked", "WorkspaceCheckpoint", "Other"]) {
        assert!(seen.contains(name), "the log never produced {name}: {:?}", sim.names());
    }
    assert_eq!(trace[0]["action"], "WorkspaceCheckpoint", "the run configures first, so no default settings are stated");
    let verify = trace.iter().find(|r| r["action"] == "Completed" && r["task"] == "survey/verify").unwrap();
    assert_eq!(verify["invs"], json!([{"id": INV, "status": "pass"}]));
    let Some(t) = tlc(0) else { return };
    let _turn = TLC_TURN.lock().await;
    let o = t.trace_validate(&spec(), &trace).await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass, "Tasks.tla rejects a legal simulator run: {:?}\n{}", o.counterexample, o.output);
    println!("session log of {} entries ({} records) accepted in {} ms", entries.len(), trace.len(), o.cost_ms);

    // the follow-up that addresses the LOW sibling reports LOW itself: the gate's pass is no step
    let mut bad = trace.clone();
    let i = bad.iter().position(|r| r["action"] == "Completed" && r["task"] == "survey.b2").unwrap();
    bad[i]["conf"] = json!("low");
    let o = t.trace_validate(&spec(), &bad).await.unwrap();
    let cx = o.counterexample.as_ref().unwrap_or_else(|| panic!("the tampered trace passed: {}", o.output));
    assert!(matches!(o.verdict, Verdict::Fail { .. }));
    assert!(cx.violated.contains("Completed") && cx.violated.ends_with("is not a step of Tasks"), "{}", cx.violated);
    println!("tampered trace refused: {}", cx.violated);

    // the check was of other code: the verify gate reported Pass for a Stale invariant
    let stale = written(&sim, &dir.path().join("stale.jsonl"), Hash::of(b"code the check never saw"));
    let o = t.trace_validate(&spec(), &aho_formal::trace_for("Tasks", &stale)).await.unwrap();
    let cx = o.counterexample.as_ref().unwrap_or_else(|| panic!("a pass over a stale check was accepted: {}", o.output));
    assert!(cx.violated.contains("Completed") && cx.violated.ends_with("is not a step of Tasks"), "{}", cx.violated);
    let at = cx.violated.split("(entry ").nth(1).and_then(|s| s.split(')').next()).and_then(|s| s.parse::<u64>().ok()).unwrap();
    let refused = stale.iter().find(|e| e.id.0 == at).unwrap();
    assert!(matches!(&refused.kind, EntryKind::Custom { body, .. } if body["id"] == "survey/verify"), "{refused:?}");
    println!("pass over a stale invariant refused: {}", cx.violated);
}

#[tokio::test]
async fn a_default_swarm_log_starts_from_the_folds_settings() {
    // default settings emit no Task/Configured, and the session's own model is AgentId(0)
    let mut sim = Sim::new(Settings::default()).unwrap();
    sim.seed(Actor::Operator, vec![seed("solo", Kind::Explore, &[], None)]).unwrap();
    sim.add_worker(
        AgentId(0),
        Box::new(
            |t: &Turn<'_>| if t.node.kind.is_gate() { Action::Complete(covers(&t.node.depends_on)) } else { Action::Complete(art("high")) },
        ),
    );
    sim.run(10).unwrap();
    assert!(sim.graph().all_terminal() && sim.names()[0] != "Configured", "{:?}", sim.names());
    let dir = tempfile::tempdir().unwrap();
    let log = JsonlLog::open(dir.path().join("solo.jsonl")).unwrap();
    for (name, body) in sim.log() {
        log.append(None, Principal::system("tool-tasks"), EntryKind::Custom { ns: "Task".into(), name: name.clone(), body: body.clone() })
            .unwrap();
    }
    let trace = aho_formal::trace_for("Tasks", &log.read_branch(log.head().unwrap()).unwrap());
    assert_eq!(
        (trace[0]["action"].clone(), trace[0]["maxLive"].clone()),
        (json!("Configured"), json!(Settings::default().max_live_workers))
    );
    let Some(t) = tlc(0) else { return };
    let _turn = TLC_TURN.lock().await;
    let o = t.trace_validate(&spec(), &trace).await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass, "{:?}\n{}", o.counterexample, o.output);
}

// ───────────────────────────── the simulator run ─────────────────────────────

/// The invariant the swarm's verify gate is judged against.
const INV: &str = "inv.tasks.gates";

fn id(s: &str) -> TaskId {
    TaskId::new(s)
}

fn art(conf: &str) -> HandoffArtifact {
    HandoffArtifact::parse(&json!({"findings": "found it", "confidence": conf, "what_i_did_not_check": ["load behaviour"]})).unwrap()
}

fn covers(ids: &[TaskId]) -> HandoffArtifact {
    let ids: Vec<&str> = ids.iter().map(TaskId::as_str).collect();
    HandoffArtifact::parse(&json!({"findings": "reviewed", "confidence": "high", "what_i_did_not_check": ["perf"], "covers": ids})).unwrap()
}

fn seed(i: &str, kind: Kind, deps: &[&str], parent: Option<&str>) -> SeedSpec {
    SeedSpec { id: id(i), content: format!("do {i}"), kind, depends_on: deps.iter().map(|d| id(d)).collect(), parent: parent.map(id) }
}

fn child(i: &str, kind: Kind) -> ChildSpec {
    ChildSpec { id: id(i), content: format!("do {i}"), kind, depends_on: vec![] }
}

/// Moves the scripted workers and the operator have made so far.
#[derive(Default)]
struct Plot {
    reopened: bool,
    paused: bool,
    resumed: bool,
    retries: u32,
    gaps: bool,
}

/// One script for every worker; workers 1 and 2 die the first time they hold `flaky`.
fn script(worker: AgentId, plot: Rc<RefCell<Plot>>) -> Script {
    Box::new(move |t: &Turn<'_>| {
        let mut p = plot.borrow_mut();
        let deps = &t.node.depends_on;
        match t.node.id.as_str() {
            "survey" if !t.node.expanded => Action::Expand(vec![child("survey.a", Kind::Explore), child("survey.b", Kind::Explore)]),
            "survey" if !p.reopened => {
                p.reopened = true;
                Action::Reopen { producer: id("survey.a"), reason: "the API map misses the v2 routes".into() }
            }
            "survey.b" => Action::Complete(art("low")),
            "survey.b2" => Action::Complete(art("medium")),
            "survey/critique" if t.graph.node(&id("survey.b2")).is_none() && t.node.no_artifact_requeues == 1 => {
                Action::Inject(vec![Injection {
                    id: id("survey.b2"),
                    content: "re-check the storage map".into(),
                    kind: Kind::Explore,
                    depends_on: vec![],
                    addresses: vec![id("survey.b")],
                }])
            }
            // the first try rubber-stamps over the LOW sibling and is refused by structure
            "survey/critique" | "survey/verify" | ROOT_GATE => Action::Complete(covers(deps)),
            "flaky" if worker.0 <= 2 => Action::Die,
            "flaky" if p.retries < 2 => Action::EndTurn,
            "extra" if !p.paused => {
                p.paused = true;
                Action::Checkpoint { summary: "halfway through the scan".into(), recent: vec!["grep → 3 hits".into()] }
            }
            _ => Action::Complete(art("high")),
        }
    })
}

/// An operator (or planner) move, built against the current graph and committed.
fn apply(sim: &mut Sim, build: impl FnOnce(&Graph) -> Result<Vec<Event>, TaskError>) {
    let events = build(sim.graph()).unwrap_or_else(|e| panic!("{e}\n{}", sim.graph().render_status()));
    sim.apply(events).unwrap();
}

/// Seed, expand, gap work, a refused rubber stamp, injection, re-open, deaths, requeues,
/// failures, operator control, stops, a resume, a verify gate judged against an invariant: every
/// `Task/*` event through the real engine.
fn scenario() -> Sim {
    let settings = Settings { deep: false, max_members: 16, max_live_workers: 2, max_reclaims: 1 };
    let mut sim = Sim::new(settings).unwrap();
    sim.set_invariants(vec![InvariantSeen { id: INV.into(), status: InvariantStatus::Pass }]);
    sim.seed(
        Actor::Operator,
        vec![
            seed("survey", Kind::Explore, &[], None),
            seed("impl", Kind::Code, &["survey"], None),
            seed("flaky", Kind::Explore, &[], None),
            seed("extra", Kind::Explore, &[], None),
        ],
    )
    .unwrap();
    // re-sent seeds are idempotent: no events
    assert!(sim.graph().seed(&Actor::Operator, vec![seed("flaky", Kind::Explore, &[], None)]).unwrap().is_empty());
    let extra = id("extra");
    apply(&mut sim, |g| g.stop(&Actor::Operator, &extra));
    apply(&mut sim, |g| g.control(&Actor::Operator, &extra, Control::Replace, None, None));
    let notes = Salvage { from: None, checkpoint: Some("notes from an earlier attempt".into()), summaries: vec![] };
    apply(&mut sim, |g| g.control(&Actor::Operator, &extra, Control::Salvage, None, Some(notes)));
    apply(&mut sim, |g| g.control(&Actor::Operator, &extra, Control::Reassign, Some(AgentId(4)), None));

    let plot = Rc::new(RefCell::new(Plot::default()));
    for w in 1..=4 {
        sim.add_worker(AgentId(w), script(AgentId(w), plot.clone()));
    }
    for _ in 0..120 {
        if sim.graph().all_terminal() {
            break;
        }
        let survey = sim.graph().node(&id("survey")).unwrap().clone();
        if survey.expanded && survey.status == Status::Queued && !plot.borrow().gaps {
            plot.borrow_mut().gaps = true;
            sim.seed(
                Actor::Operator,
                vec![seed("survey.c", Kind::Code, &[], Some("survey")), seed("survey.d", Kind::Explore, &[], Some("survey"))],
            )
            .unwrap();
            // the planner stops work in its own subtree; the operator puts it back
            let planner = Actor::Agent(survey.planner.unwrap());
            apply(&mut sim, |g| g.stop(&planner, &id("survey.d")));
            apply(&mut sim, |g| g.control(&Actor::Operator, &id("survey.d"), Control::Retry, None, None));
        }
        let x = sim.graph().node(&extra).unwrap().clone();
        if x.status == Status::Running && x.checkpoint.is_some() && !plot.borrow().resumed {
            plot.borrow_mut().resumed = true;
            let resumed = sim.graph().running_stale();
            sim.apply(resumed).unwrap();
            apply(&mut sim, |g| g.heartbeat(x.owner.unwrap(), &extra, Some("still scanning".into())));
        }
        if sim.graph().node(&id("flaky")).unwrap().status == Status::Failed {
            plot.borrow_mut().retries += 1;
            apply(&mut sim, |g| g.control(&Actor::Operator, &id("flaky"), Control::Retry, None, None));
        }
        sim.step().unwrap();
    }
    assert!(sim.graph().nodes().iter().all(|n| n.status == Status::Completed), "{}\n{:?}", sim.graph().render_status(), sim.names());
    assert_eq!(&sim.replay().unwrap(), sim.graph(), "the recorded bodies fold back to the same graph");
    sim
}

/// The run as a session log at `path`: a checkpoint of `tree`, a Pass of `INV` checked against
/// `checked`, then every `Task/*` event as tool-tasks' results append it (with the loop's own
/// entries between them), written and read back through `JsonlLog`.
fn written(sim: &Sim, path: &Path, checked: Hash) -> Vec<Entry> {
    let log = JsonlLog::open(path).unwrap();
    let tree = Hash::of(b"tree after the swarm's edits");
    let cp = EntryKind::WorkspaceCheckpoint { id: CheckpointId(1), tree, parent: None, cause: CheckpointCause::Manual };
    log.append(None, Principal::system("checkpoint"), cp).unwrap();
    let check = EntryKind::InvariantChecked { id: INV.into(), mode: CheckMode::Bounded(7), verdict: Verdict::Pass, code_hash: checked };
    log.append(None, Principal::system("spec_check"), check).unwrap();
    for (i, (name, body)) in sim.log().iter().enumerate() {
        if i % 7 == 0 {
            log.append(None, Principal::system("loop"), EntryKind::Note { text: format!("step {i}") }).unwrap();
        }
        let kind = EntryKind::Custom { ns: "Task".into(), name: name.clone(), body: body.clone() };
        log.append(None, Principal::system("tool-tasks"), kind).unwrap();
    }
    log.read_branch(log.head().unwrap()).unwrap()
}
