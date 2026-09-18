use std::collections::HashSet;
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ConfigKey, Layer, Loadout, Origin, ResolveInputs, Schema, Sourced, Ty, Where, resolve};
use aho_log::{CapState, Entry, EntryKind, LogStore, MemoryLog, OpKind, ProcessIdentity, State, Verdict};
use serde_json::json;

use crate::*;

// ───────────────────────────── test capabilities ─────────────────────────────

#[derive(Clone)]
struct FsH(#[allow(dead_code)] Arc<String>);
impl Handle for FsH {
    const KIND: HandleKind = HandleKind::FsRead;
}

struct Fs;
impl Capability for Fs {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(Fs)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(FsH(Arc::new("fs".into())));
    }
}
declare_capability!(
    Fs,
    "fs-test",
    kind = Kind::Provider,
    provides = [SeamId("fs")],
    requires = [],
    tools = &[],
    provides_handles = [HandleKind::FsRead]
);

struct ReadDeps(#[allow(dead_code)] FsH);
impl Deps for ReadDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::FsRead];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(ReadDeps(r.handle()?))
    }
}
static READ_TOOLS: &[ToolDecl] = &[ToolDecl::new("read", "Read a file", Consequence::ReadOnly).safe()];
struct ToolRead;
impl Capability for ToolRead {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ReadDeps;
    type Config = ();
    fn build(_: ReadDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolRead)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(
    ToolRead,
    "tool-read",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(SeamId("fs"))],
    tools = READ_TOOLS
);

struct Gh;
impl Capability for Gh {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(Gh)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(Gh, "mcp.github", kind = Kind::Tool, provides = [], requires = [Requirement::Credential("github")], tools = &[]);

struct VmDeps;
impl Deps for VmDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::VmControl];
    fn resolve(_: &Resolver) -> Result<Self, MissingDep> {
        Ok(VmDeps)
    }
}
static VM_TOOLS: &[ToolDecl] = &[ToolDecl::new("vm_start", "Start a VM", Consequence::ControlsVM)];
struct ToolVm;
impl Capability for ToolVm {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = VmDeps;
    type Config = ();
    fn build(_: VmDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolVm)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(
    ToolVm,
    "tool-vm",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Cap("vm-net-relay"), Requirement::Binary("qemu-system-aarch64"), Requirement::Platform(Os::Linux)],
    tools = VM_TOOLS
);

fn manifests() -> Vec<&'static Manifest> {
    vec![Fs::MANIFEST, ToolRead::MANIFEST, Gh::MANIFEST, ToolVm::MANIFEST]
}

fn loadout(disable: &[&str]) -> Loadout {
    let mut s = Schema::default();
    for k in core_keys() {
        s.add(k);
    }
    let i = ResolveInputs { disable: disable.iter().map(|d| d.to_string()).collect(), ..ResolveInputs::default() };
    resolve(&i, &s, false).unwrap()
}

fn workspace() -> PolicyInput {
    PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite]) }
}

fn plan_with(disable: &[&str], discovered: Vec<Discovered>) -> Plan {
    plan(&manifests(), &loadout(disable), &Env::empty(Os::MacOs), &workspace(), discovered).unwrap()
}

fn log_of(kinds: Vec<(Principal, EntryKind)>) -> Vec<Entry> {
    let log = MemoryLog::new();
    for (by, k) in kinds {
        log.append(None, by, k).unwrap();
    }
    log.read_branch(log.head().unwrap()).unwrap()
}

fn fold(entries: &[Entry]) -> State {
    aho_log::fold(entries, &Default::default()).unwrap()
}

fn sys() -> Principal {
    Principal::system("test")
}

// ───────────────────────────── tree ─────────────────────────────

#[test]
fn disabled_node_renders_its_glyph_and_the_winning_origin() {
    let p = plan_with(&["tool-read"], vec![]);
    let out = render_tree("t", &tree(&p), &p);
    let line = out.lines().find(|l| l.contains("tool-read")).unwrap();
    assert!(line.starts_with("⊘ tool-read"), "{out}");
    assert!(line.contains("Disabled"), "{line}");
    assert!(line.contains("[disabled ← Arg --disable tool-read]"), "{line}");
    let why = why(&p, "tool-read").unwrap();
    assert_eq!(why.fix.as_deref(), Some("aho enable tool-read"));
}

#[test]
fn locked_node_renders_every_fix_on_its_own_line() {
    let p = plan_with(&[], vec![]);
    let nodes = tree(&p);
    let vm = nodes.iter().find(|n| n.id == "tool-vm").unwrap();
    assert!(matches!(vm.state, CapState::Locked { .. }));
    assert_eq!(vm.missing.len(), 3, "{:?}", vm.missing);
    let out = render_tree("t", &nodes, &p);
    assert!(out.starts_with(&format!("t @ plan {}  (preset: workspace, world: host, fingerprint ", p.hash.short())), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    let at = lines.iter().position(|l| l.starts_with("◌ tool-vm")).unwrap();
    let block = &lines[at..at + vm.missing.len()];
    for (m, line) in vm.missing.iter().zip(block) {
        let fix = m.fix.as_deref().unwrap();
        assert!(line.contains(&m.requirement) && line.contains(&format!("fix: `{fix}`")), "{line} lacks {m:?}");
    }
    assert!(block.iter().any(|l| l.contains("fix: `aho enable vm-net-relay`")));
    assert!(block.iter().any(|l| l.contains("fix: `./install-system-deps.sh`")));
    let preset_line = lines[at..].iter().find(|l| l.contains("⚠ Preset(workspace does not allow ControlsVM)")).unwrap();
    assert!(preset_line.contains("fix: `aho preset lab  |  aho allow tool-vm`"), "{preset_line}");
    // continuation lines align under the first requirement
    let col = block[0].find("Cap(").unwrap();
    assert!(block[1..].iter().all(|l| l.chars().take_while(|c| *c == ' ').count() == block[0][..col].chars().count()));
}

#[test]
fn discovered_tools_follow_their_locked_parent_and_inherit_its_fix() {
    let d = Discovered {
        id: "mcp__github__create_issue".into(),
        kind: Kind::Tool,
        origin: CapOrigin::UserInstalled,
        source_path: None,
        classes: ConsequenceSet::of(&[Consequence::ReadOnly]),
        requires: vec![],
        broken: None,
        doc: "Create an issue".into(),
    };
    let p = plan_with(&[], vec![d]);
    let nodes = tree(&p);
    let gh = nodes.iter().find(|n| n.id == "mcp.github").unwrap();
    assert_eq!(gh.children.len(), 1);
    let child = &gh.children[0];
    assert!(matches!(child.state, CapState::Locked { .. }));
    assert_eq!(child.missing[0].requirement, "(follows mcp.github)");
    assert_eq!(child.action.as_deref(), Some("aho credential set github"));
    let out = render_tree("t", &nodes, &p);
    assert!(out.contains("  ◌ mcp__github__create_issue"), "{out}");

    let cards = inventory(&p, Some("mcp"));
    assert_eq!(cards.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["mcp.github", "mcp__github__create_issue"]);
    assert_eq!(cards[0].fix.as_deref(), Some("aho credential set github"));
    let inv = render_inventory(&cards);
    assert!(inv.contains("origin: user") && inv.contains("fix:          aho credential set github"), "{inv}");
}

#[test]
fn why_is_the_json_the_agent_reads() {
    let p = plan_with(&[], vec![]);
    let w = serde_json::to_value(why(&p, "mcp.github").unwrap()).unwrap();
    assert_eq!(w["state"]["state"], json!("locked"));
    assert_eq!(w["missing"][0]["requirement"], json!("Credential(github)"));
    assert_eq!(w["fix"], json!("aho credential set github"));
    assert_eq!(w["preset"], json!("workspace"));
    let active = why(&p, "tool-read").unwrap();
    assert!(active.fix.is_none());
    assert_eq!(active.classes, ConsequenceSet::of(&[Consequence::ReadOnly]));
    assert!(why(&p, "no-such-cap").is_none());
}

#[test]
fn inventory_equip_warns_when_a_capability_exceeds_the_preset() {
    let d = Discovered {
        id: "skill.deploy".into(),
        kind: Kind::Template,
        origin: CapOrigin::Bundled,
        source_path: Some("./.aho/skills/deploy".into()),
        classes: ConsequenceSet::of(&[Consequence::NetworkEgress]),
        requires: vec![],
        broken: None,
        doc: String::new(),
    };
    let mut schema = Schema::default();
    for key in core_keys() {
        schema.add(key);
    }
    schema.add(aho_config::ConfigKey::new("skills", aho_config::Ty::Table, None, ""));
    let inputs = ResolveInputs { sets: vec![("skills.deploy.enabled".into(), "true".into())], ..ResolveInputs::default() };
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let p = plan(&manifests(), &loadout, &Env::empty(Os::MacOs), &workspace(), vec![d]).unwrap();
    let cards = inventory(&p, Some("skill"));
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].state, CapState::Active, "over-preset is a note: calls needing NetworkEgress ask or are denied per call");
    let nodes = tree(&p);
    let n = nodes.iter().flat_map(|n| std::iter::once(n).chain(n.children.iter())).find(|n| n.id == "skill.deploy").unwrap();
    assert!(n.over_preset.as_ref().unwrap().requirement.contains("does not allow NetworkEgress"));
}

// ───────────────────────────── explain ─────────────────────────────

fn file(path: &str, line: u32) -> Origin {
    Origin::new(Layer::FileProject, Where::File { path: path.into(), line })
}

#[test]
fn explain_over_a_folded_state_applies_the_broader_scope_rule_itself() {
    let sourced = Sourced {
        value: json!("fast"),
        winner: Origin::new(Layer::Env, Where::Env { var: "AHO_EDIT_MODE".into() }),
        shadowed: vec![(file("./aho.toml", 7), json!("careful")), (Origin::new(Layer::Default, Where::Const), json!("safe"))],
    };
    let mut state = State::default();
    state.config.insert("edit.mode".into(), sourced);
    let e = explain(&state, "edit.mode");
    assert!(e.shadowed[0].broader_scope_wins, "Env (user scope) beat a project file");
    assert!(!e.shadowed[1].broader_scope_wins, "Default is broader than Env");
    let out = render_explain(&e);
    assert!(out.starts_with("edit.mode = \"fast\"\n  winner   Env AHO_EDIT_MODE"), "{out}");
    assert!(out.lines().nth(2).unwrap().contains("File ./aho.toml:7 = \"careful\"") && out.contains("⚠ broader-scope-wins"), "{out}");
    assert_eq!(render_explain(&explain(&state, "nope")), "nope is not set by any layer\n");
}

#[test]
fn explain_over_the_log_shows_bootstrap_and_later_writes() {
    let first = Sourced { value: json!("a"), winner: file("./aho.toml", 1), shadowed: vec![] };
    let later = Sourced {
        value: json!("b"),
        winner: Origin::new(Layer::Runtime("operator".into()), Where::Entry { id: EntryId(3) }),
        shadowed: vec![(file("./aho.toml", 1), json!("a"))],
    };
    let entries = log_of(vec![
        (sys(), EntryKind::ConfigResolved { key: "k".into(), sourced: first }),
        (sys(), EntryKind::PlanSealed { plan_hash: Hash::ZERO, fingerprint: Hash::ZERO, schema_hash: Hash::ZERO }),
        (Principal::Operator, EntryKind::ConfigResolved { key: "k".into(), sourced: later }),
    ]);
    let e = explain(&entries[..], "k");
    assert_eq!(e.value, Some(json!("b")));
    assert_eq!(e.history.len(), 2);
    assert!(e.history[0].bootstrap && !e.history[1].bootstrap);
    let out = render_explain(&e);
    assert!(out.contains("entry 1      ConfigResolved   (bootstrap)"), "{out}");
    assert!(out.contains("entry 3      ConfigResolved   by: operator"), "{out}");
}

// ───────────────────────────── catalog / invariants ─────────────────────────────

#[test]
fn catalog_lists_every_key_and_the_schema_hash() {
    let mut s = Schema::default();
    for k in core_keys() {
        s.add(k);
    }
    s.add(ConfigKey::new("provider.openai.key", Ty::String, Some(json!("sk-default")), "API key | for OpenAI").secret());
    let md = catalog_config(&s);
    for path in s.keys.keys() {
        assert!(md.contains(&format!("| `{path}` |")), "{path} missing from catalog");
    }
    assert!(md.contains(&s.hash().to_hex()));
    assert!(md.contains("string (secret) | (secret) | `AHO_PROVIDER_OPENAI_KEY` | API key \\| for OpenAI |"), "{md}");
    assert!(!md.contains("sk-default"));
    assert_eq!(md.lines().filter(|l| l.starts_with("| `")).count(), s.keys.len());
}

#[test]
fn a_verdict_older_than_the_code_is_stale_never_pass() {
    let (v1, v2) = (Hash::of(b"code v1"), Hash::of(b"code v2"));
    let checked =
        |id: &str, verdict: Verdict| EntryKind::InvariantChecked { id: id.into(), mode: aho_log::CheckMode::Proof, verdict, code_hash: v1 };
    let state = fold(&log_of(vec![
        (sys(), checked("inv.a", Verdict::Pass)),
        (sys(), checked("inv.b", Verdict::Fail { counterexample: Hash::of(b"cx") })),
    ]));
    let same = invariants(&state, v1);
    assert!(same.iter().all(|r| !r.stale));
    assert_eq!(same[0].verdict, Verdict::Pass);
    let moved = invariants(&state, v2);
    assert!(moved.iter().all(|r| r.stale && r.verdict == Verdict::Stale));
    assert_eq!(moved[0].recorded, Verdict::Pass);
    let out = render_invariants(&moved);
    assert!(out.contains("inv.a") && out.contains("code changed since the check (was Pass)"), "{out}");
}

// ───────────────────────────── session projections ─────────────────────────────

fn identity(pid: u32) -> ProcessIdentity {
    ProcessIdentity { pid, start_time: Timestamp(1_000) }
}

fn crashed_session(pid: u32) -> State {
    let call = CallId("c1".into());
    fold(&log_of(vec![
        (sys(), EntryKind::SessionMeta { title: None, tags: vec![], last_prompt: None, process: Some(identity(pid)) }),
        (Principal::Operator, EntryKind::TurnStart { turn: TurnId(1) }),
        (Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: "fix the parser".into(), client_key: None }),
        (
            sys(),
            EntryKind::AssistantMessage {
                content: vec![
                    ContentBlock::text("Writing the fix."),
                    ContentBlock::ToolUse { id: call.clone(), name: "write".into(), args: json!({"path": "src/parse.rs"}) },
                ],
                usage: Usage::default(),
                stop: StopReason::ToolUse,
            },
        ),
        (
            sys(),
            EntryKind::ToolCall {
                call: call.clone(),
                tool: "write".into(),
                args: json!({"path": "src/parse.rs"}),
                class: Consequence::WorkspaceWrite,
            },
        ),
        (sys(), EntryKind::OpStarted { op: OpId(7), call, kind: OpKind::Io, rerunnable: false, external_effect: true }),
    ]))
}

#[test]
fn catch_up_after_a_crash_names_what_the_operator_must_do() {
    let c = catch_up(&crashed_session(42));
    assert_eq!(c.status, SessionStatus::Crashed);
    assert!(c.needs_catch_up);
    assert_eq!(c.last_prompt.as_deref(), Some("fix the parser"));
    assert_eq!(c.last_response.as_deref(), Some("Writing the fix."));
    assert_eq!(c.files_touched, vec!["src/parse.rs".to_string()]);
    assert_eq!(c.tool_counts, vec![("write".to_string(), 1)]);
    assert!(c.needs_from_user.iter().any(|n| n == "1 tool call(s) had no result when the session stopped"), "{:?}", c.needs_from_user);
    let md = render_catch_up(&c);
    assert!(md.starts_with("## Catch-up — Crashed") && md.contains("**Files touched:** `src/parse.rs`"), "{md}");
    let (live, _) = session_status(&crashed_session(42), true);
    assert_eq!(live, SessionStatus::Active);
}

#[test]
fn orphans_are_reported_only_after_two_missing_scans() {
    let sessions = vec![(SessionId(1), crashed_session(42)), (SessionId(2), crashed_session(43))];
    let alive: HashSet<ProcessIdentity> = [identity(43)].into_iter().collect();
    let now = Timestamp(61_000);

    let first = orphans(&sessions, &alive, &HashSet::new(), now);
    assert!(first.orphans.is_empty(), "one missing scan is a flap, not a death");
    assert_eq!(first.unconfirmed, vec![(SessionId(1), identity(42))]);
    assert!(first.missing.contains(&identity(42)) && !first.missing.contains(&identity(43)));

    let second = orphans(&sessions, &alive, &first.missing, now);
    assert!(second.unconfirmed.is_empty());
    assert!(second.orphans.iter().all(|o| o.owner == SessionId(1) && o.age_s == 60));
    let kinds: Vec<&OrphanKind> = second.orphans.iter().map(|o| &o.kind).collect();
    assert!(kinds.contains(&&OrphanKind::OpenTurn { turn: TurnId(1) }));
    assert!(kinds.contains(&&OrphanKind::Operation { op: OpId(7), call: CallId("c1".into()), external_effect: true }));
    assert!(!kinds.iter().any(|k| matches!(k, OrphanKind::ToolCall { .. })), "a call covered by an open op is reported once");
    assert!(render_orphans(&second).contains("operation 7 (call c1) still open, has an external effect"));

    let recovered = orphans(&sessions, &[identity(42), identity(43)].into_iter().collect(), &first.missing, now);
    assert!(recovered.orphans.is_empty() && recovered.missing.is_empty());
}

#[test]
fn status_lists_active_capabilities_loudest_first_and_flags_config_drift() {
    let p = plan_with(&[], vec![]);
    let state = fold(&log_of(vec![
        (sys(), EntryKind::CapActivated { id: "fs-test".into(), manifest_hash: Hash::ZERO, deps: vec![], config_hash: Hash::ZERO }),
        (sys(), EntryKind::CapActivated { id: "tool-read".into(), manifest_hash: Hash::ZERO, deps: vec![], config_hash: Hash::ZERO }),
        (sys(), EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: p.allowed, preset: "workspace".into(), approval_ref: None }),
        (sys(), EntryKind::PlanSealed { plan_hash: Hash::of(b"older"), fingerprint: p.fingerprint, schema_hash: Hash::ZERO }),
        (sys(), EntryKind::LeaseGranted { lease: LeaseId(1), subject: "vm:ubuntu".into(), expires: Timestamp(10_000) }),
    ]));
    let s = status(&p, &state, Timestamp(4_000));
    assert_eq!(s.active.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), vec!["tool-read", "fs-test"]);
    assert!(!s.plan_matches_log);
    assert_eq!(s.leases[0].remaining_s, 6);
    let out = render_status(&s);
    assert!(out.contains("config changed since") && out.contains("lease 1  vm:ubuntu  expires in 6s"), "{out}");
}
