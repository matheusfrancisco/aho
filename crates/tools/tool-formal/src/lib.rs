//! The model-facing formal tools:
//!
//! | tool | does | logs (via `value.events`) |
//! |---|---|---|
//! | `spec_check` | model-check a TLA+ spec (TLC; `Bounded(n)` or the whole instance) | `InvariantChecked` when `invariant` is given |
//! | `spec_trace` | validate a recorded log / trace against a TLA+ spec | `InvariantChecked{Trace}` when `invariant` is given |
//! | `lean_build` | `lake build` a module; unproved goals come back as goals | `InvariantChecked{Proof}` when `invariant` is given |
//! | `lean_translate` | Aeneas: Rust crate → Lean module bound to a translation hash | none |
//! | `invariant_add` | ask the engineer to approve a claim (approval card) | `InvariantRegistered` on approval |
//! | `invariant_mutate` | check mutants; none caught ⇒ `Vacuous` | `InvariantMutated` per mutant (+ `InvariantChecked{Vacuous}`) |
//!
//! Specs are files (`spec`) or invariant ids in the literate `spec/SPEC.md` (`invariant`/`id`):
//! an id is checked against what SPEC.md says right now (its blocks are materialized into a
//! scratch dir).
//!
//! Every `InvariantChecked` is bound to the code it checked: `code_hash` is the workspace tree
//! (`Snapshot::tree`) when the check started, the quantity `Workspace/Checkpoint` records, so a
//! verdict reads `Stale` once a later checkpoint has other code (`aho_inspect::current_invariants`,
//! `aho_formal::status`). A world without snapshots makes no checkpoints; its verdicts carry
//! `Hash::ZERO` (code unknown) and stand as recorded until one exists.

#![forbid(unsafe_code)]

mod spec_mode;

pub use spec_mode::{SPEC_MODE_SECTION, SPEC_PRESET};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aho_approval::{ApprovalHandle, AskPolicy};
use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_formal::{
    Block, BlockLang, CheckConfig, CheckMode, Diagnostic, FormalError, FormalKind, Formals, LEAN_SEAM, Outcome, SpecDoc, SpecRef, TLA_SEAM,
    Verdict, kind_name, parse_kind, spec_doc, tla, trace_for,
};
use aho_fs::{FsError, FsRead, FsWrite, ScratchDir, SnapshotHandle};
use aho_log::{ApprovalOutcome, Entry, EntryKind, LogStore, OpKind};
use aho_policy::{ApprovalCard, DecisionReason};
use aho_types::*;
use serde_json::json;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormalToolConfig {
    /// The System Spec, relative to the workspace.
    pub spec_doc: String,
    /// Default wall-clock budget for one check.
    pub budget_ms: u64,
}

impl Config for FormalToolConfig {
    const PREFIX: &'static str = "formal.tools";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "formal.tools.spec_doc",
                Ty::String,
                Some(json!("spec/SPEC.md")),
                "The literate System Spec, relative to the workspace; invariant ids resolve here.",
            ),
            ConfigKey::new(
                "formal.tools.budget_ms",
                Ty::Int,
                Some(json!(120_000)),
                "Default wall-clock budget for one spec_check / invariant_mutate run.",
            ),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        Ok(FormalToolConfig {
            spec_doc: cfg_str(l, "formal.tools.spec_doc", "spec/SPEC.md"),
            budget_ms: cfg_u64(l, "formal.tools.budget_ms", 120_000),
        })
    }
}

/// Every formal backend (the `formal:*` services) and, for `invariant_add`, the operator.
pub struct FormalDeps {
    pub formals: Formals,
    /// All document, config and trace reads use the bound world's filesystem.
    pub read: FsRead,
    /// Materialized literate specs are owned scratch files in that same world.
    pub write: FsWrite,
    /// Absent under an async subagent: `invariant_add` then refuses (fail closed).
    pub approval: Option<ApprovalHandle>,
    /// The world's workspace snapshots: the tree a verdict is bound to. Absent when the world
    /// has none (then no checkpoint exists either).
    pub snapshot: Option<SnapshotHandle>,
}

impl Deps for FormalDeps {
    const REQUIRES: &'static [HandleKind] =
        &[HandleKind::Formal, HandleKind::FsRead, HandleKind::FsWrite, HandleKind::Approval, HandleKind::Snapshot];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(FormalDeps {
            formals: Formals::resolve(r)?,
            read: r.handle()?,
            write: r.handle()?,
            approval: r.handle::<ApprovalHandle>().ok(),
            snapshot: r.handle().ok(),
        })
    }
}

pub struct ToolFormal {
    inner: Arc<Inner>,
}

struct Inner {
    formals: Formals,
    read: FsRead,
    write: FsWrite,
    approval: Option<ApprovalHandle>,
    snapshot: Option<SnapshotHandle>,
    cfg: FormalToolConfig,
}

impl ToolFormal {
    /// Direct construction for embedders and tests (the composition path is `build`).
    pub fn new(deps: FormalDeps, cfg: FormalToolConfig) -> ToolFormal {
        ToolFormal {
            inner: Arc::new(Inner {
                formals: deps.formals,
                read: deps.read,
                write: deps.write,
                approval: deps.approval,
                snapshot: deps.snapshot,
                cfg,
            }),
        }
    }
    /// The exec function registered for `tool`.
    pub fn exec(&self, tool: &'static str) -> ToolExec {
        let inner = self.inner.clone();
        Arc::new(move |req: ExecRequest| {
            let inner = inner.clone();
            Box::pin(async move {
                let cancel = req.cancel.clone();
                cancellable(cancel, inner.run(tool, req.view)).await
            })
        })
    }
}

impl Capability for ToolFormal {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = FormalDeps;
    type Config = FormalToolConfig;
    fn build(deps: FormalDeps, cfg: FormalToolConfig) -> Result<Self, BuildError> {
        Ok(ToolFormal::new(deps, cfg))
    }
    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            r.tool(decl, self.exec(decl.name));
        }
    }
}

const PRESENT: Present = Present { call: call_card, result: result_card };

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "spec_check",
        "Model-check a TLA+ spec with TLC. Give `spec` (a .tla file; its sibling .cfg is the model unless `config` names one) or `invariant` (an id in spec/SPEC.md). `bound: n` checks every behavior of up to n steps; omit it to explore the whole finite instance. Results are bounded: evidence, not proof. Returns Pass, Fail with the counterexample trace, or Unknown with the reason.",
        Consequence::SpawnsProcess,
    )
    .schema(spec_check_schema)
    .safe()
    .cancel_on_interrupt()
    .op(OpKind::Process, true, false)
    .present(PRESENT),
    ToolDecl::new(
        "spec_trace",
        "Validate a recorded behavior against a TLA+ spec that defines TraceStep(e): `log` is a session log (.jsonl, optionally `from`/`to` entry ids), or `trace` a JSON/JSONL file of action records. Pass means the log is a behavior of the spec; Fail names the first event the spec does not allow.",
        Consequence::SpawnsProcess,
    )
    .schema(spec_trace_schema)
    .safe()
    .cancel_on_interrupt()
    .op(OpKind::Process, true, false)
    .present(PRESENT),
    ToolDecl::new(
        "lean_build",
        "Build a Lean module (or the whole project) with lake. Pass means every theorem is proved; otherwise the open goals come back with their hypotheses and target. A proof that uses `sorry` is not a pass.",
        Consequence::SpawnsProcess,
    )
    .schema(lean_build_schema)
    .cancel_on_interrupt()
    .op(OpKind::Process, true, false)
    .present(PRESENT),
    ToolDecl::new(
        "lean_translate",
        "Translate functions of a Rust crate (safe subset) to Lean with Charon + Aeneas. Returns the generated module and its translation hash; proofs about it are bound to that exact code.",
        Consequence::SpawnsProcess,
    )
    .schema(lean_translate_schema)
    .cancel_on_interrupt()
    .op(OpKind::Process, true, false)
    .present(PRESENT),
    ToolDecl::new(
        "invariant_add",
        "Propose an invariant to the engineer: an id, its kind (tla or lean), the claim in prose, and what it covers (paths, log kinds). It is registered only if the engineer approves the card; you cannot self-certify a claim about their intent. A changed statement is a new id with `supersedes`.",
        Consequence::TalksToHuman,
    )
    .schema(invariant_add_schema)
    .present(PRESENT),
    ToolDecl::new(
        "invariant_mutate",
        "Check that an invariant means something: mutate the spec's actions (or a Lean file's definitions) and re-check each mutant. At least one must be caught; an invariant that catches none is Vacuous and does not count. Give `id` (in spec/SPEC.md) or `spec` (+ the same model options as spec_check).",
        Consequence::SpawnsProcess,
    )
    .schema(invariant_mutate_schema)
    .safe()
    .cancel_on_interrupt()
    .op(OpKind::Process, true, false)
    .present(PRESENT),
];

declare_capability!(
    ToolFormal,
    "tool-formal",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::AnyOf(&[Requirement::Seam(TLA_SEAM), Requirement::Seam(LEAN_SEAM)])],
    tools = TOOLS,
    unlock = Fix::cli("aho doctor formal --install tla"),
    doc = "spec_check / spec_trace / lean_build / lean_translate / invariant_add / invariant_mutate."
);

// ───────────────────────────── schemas ─────────────────────────────

fn model_props() -> Value {
    json!({
        "spec": {"type": "string", "description": "TLA+ module file (.tla), workspace-relative. Omit when `invariant` names an id in spec/SPEC.md."},
        "config": {"type": "string", "description": "TLC .cfg to use (default: the spec's sibling .cfg, or the cfg block of the invariant)."},
        "invariants": {"type": "array", "items": {"type": "string"}, "description": "Invariant operators to check (replaces the cfg's INVARIANT list)."},
        "properties": {"type": "array", "items": {"type": "string"}, "description": "Temporal/action properties (PROPERTY)."},
        "constants": {"type": "object", "additionalProperties": {"type": ["string", "integer"]}, "description": "CONSTANT values, e.g. {\"N\": 3, \"Procs\": \"{p1, p2}\"}."},
        "bound": {"type": "integer", "minimum": 0, "description": "Check every behavior of up to this many steps; 0 or absent = the whole finite instance."},
        "mode": {"type": "string", "enum": ["bounded", "inductive"], "description": "inductive needs Apalache."},
        "deadlock": {"type": "boolean", "description": "Report states with no successor (default: the cfg's CHECK_DEADLOCK, else off)."},
        "budget_ms": {"type": "integer", "minimum": 1, "description": "Wall-clock budget; past it the verdict is Unknown{timeout}."}
    })
}

fn spec_check_schema(_: &BoundWorld) -> RootSchema {
    let mut p = model_props();
    p["invariant"] = json!({"type": "string", "description": "Registered invariant id: resolves the spec from spec/SPEC.md and records the verdict in the log."});
    RootSchema(json!({"type": "object", "properties": p, "additionalProperties": false}))
}

fn spec_trace_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "spec": {"type": "string", "description": "TLA+ module defining Init, vars and TraceStep(e) (e.g. aho/formal/Log.tla)."},
            "invariant": {"type": "string", "description": "Registered invariant id (resolves the spec from spec/SPEC.md; records the verdict)."},
            "log": {"type": "string", "description": "A session log (.jsonl) to validate; the branch at its head."},
            "from": {"type": "integer", "minimum": 0, "description": "First entry id of the log range (inclusive)."},
            "to": {"type": "integer", "minimum": 0, "description": "Last entry id of the log range (inclusive)."},
            "trace": {"type": "string", "description": "A JSON array or JSONL file of action records ({\"action\": …, …})."}
        },
        "additionalProperties": false
    }))
}

fn lean_build_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "module": {"type": "string", "description": "Lean module to build, e.g. Aho.Fold; omit for the whole project."},
            "invariant": {"type": "string", "description": "Registered invariant id this module proves; records the verdict."}
        },
        "additionalProperties": false
    }))
}

fn lean_translate_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "crate": {"type": "string", "description": "The Rust crate's directory (with Cargo.toml), workspace-relative."},
            "fns": {"type": "array", "items": {"type": "string"}, "description": "Functions to translate (`name` or `crate::path::name`); empty = the whole crate."}
        },
        "required": ["crate"],
        "additionalProperties": false
    }))
}

fn invariant_add_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "Stable id, e.g. inv.order.no-double-charge."},
            "kind": {"type": "string", "enum": ["tla", "lean"]},
            "statement": {"type": "string", "description": "The claim, in the engineer's words."},
            "covers": {"type": "array", "items": {"type": "string"}, "description": "Paths, log kinds or actions it covers."},
            "supersedes": {"type": "string", "description": "Id of the invariant this replaces (a changed statement is a new id)."},
            "why": {"type": "string", "description": "Why the old statement was wrong or too weak."}
        },
        "required": ["id", "kind", "statement"],
        "additionalProperties": false
    }))
}

fn invariant_mutate_schema(_: &BoundWorld) -> RootSchema {
    let mut p = model_props();
    p["id"] =
        json!({"type": "string", "description": "Registered invariant id (resolves the spec from spec/SPEC.md; records every mutant)."});
    p["spec"] = json!({"type": "string", "description": "A .tla module or .lean file to mutate, when `id` is not in spec/SPEC.md."});
    RootSchema(json!({"type": "object", "properties": p, "additionalProperties": false}))
}

// ───────────────────────────── cards ─────────────────────────────

fn call_card(args: &Value) -> Card {
    let subject =
        ["invariant", "id", "spec", "module", "crate"].iter().find_map(|k| args.get(*k).and_then(Value::as_str)).unwrap_or("the project");
    Card::Generic { title: format!("formal: {subject}"), body: args.to_string() }
}

fn result_card(_args: &Value, value: &Value) -> Card {
    let title = if let Some(r) = value.get("registered") {
        format!("registered {}", r.get("id").and_then(Value::as_str).unwrap_or(""))
    } else if let Some(v) = value.get("verdict").and_then(|v| v.get("verdict")).and_then(Value::as_str) {
        v.to_string()
    } else if let Some(m) = value.get("module").and_then(Value::as_str) {
        format!("translated {m}")
    } else {
        "done".into()
    };
    Card::Generic { title, body: value.get("summary").and_then(Value::as_str).unwrap_or("").to_string() }
}

// ───────────────────────────── helpers ─────────────────────────────

/// Drops the work when the call is cancelled; dropping the checker's child kills it.
async fn cancellable(cancel: CancelToken, f: impl Future<Output = ExecOutcome>) -> ExecOutcome {
    tokio::pin!(f);
    loop {
        tokio::select! {
            o = &mut f => return o,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if cancel.is_cancelled() {
                    return ExecOutcome::Cancelled;
                }
            }
        }
    }
}

type Res<T> = Result<T, ExecOutcome>;

fn bad(tool: &str, msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err("BAD_ARGS", format!("{tool}: {}", msg.into()))
}

fn opt_str<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn str_list(args: &Value, k: &str) -> Option<Vec<String>> {
    args.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
}

/// Workspace-relative (or absolute inside the workspace) → an existing path in the bound
/// world. The filesystem provider enforces both lexical and symlink fences.
pub async fn in_workspace_of(read: &FsRead, p: &str) -> Result<PathBuf, ExecOutcome> {
    let stat = read.0.stat(p).await.map_err(|e| path_error(p, e))?;
    Ok(Path::new(&read.0.root()).join(stat.path))
}

fn path_error(p: &str, e: FsError) -> ExecOutcome {
    let code = match &e {
        FsError::OutsideWorkspace(_) => "OUTSIDE_WORKSPACE",
        FsError::NotFound(_) | FsError::DanglingSymlink(_) => "NOT_FOUND",
        _ => e.code(),
    };
    ExecOutcome::err(code, format!("{p}: {e}; nothing was checked"))
}

async fn optional_file(read: &FsRead, path: PathBuf) -> Res<Option<PathBuf>> {
    let shown = path.to_string_lossy();
    match read.0.stat(&shown).await {
        Ok(stat) if !stat.is_dir => Ok(Some(Path::new(&read.0.root()).join(stat.path))),
        Ok(_) | Err(FsError::NotFound(_)) => Ok(None),
        Err(e) => Err(path_error(&shown, e)),
    }
}

fn event(kind: EntryKind) -> Value {
    json!({ "kind": serde_json::to_value(kind).unwrap_or(Value::Null) })
}

fn kind_of_path(p: &Path) -> Option<FormalKind> {
    match p.extension().and_then(|e| e.to_str()) {
        Some("tla") => Some(FormalKind::Tla),
        Some("lean") => Some(FormalKind::Lean),
        _ => None,
    }
}

/// A spec to run, with what the verdict is bound to. Shared with the operator's `aho invariants`
/// verbs so both resolve specs and models the same way.
pub struct Target {
    pub spec: SpecRef,
    pub cfg_file: Option<PathBuf>,
    /// The claim's `mode:` field, used when the call gives no bound.
    pub doc_mode: Option<CheckMode>,
    /// Materialized SPEC.md extracts; dropping the lease also cleans up cancelled calls.
    scratch: Option<Box<dyn ScratchDir>>,
}

impl Target {
    /// Await scratch cleanup before publishing a result; Drop remains the cancellation fallback.
    pub async fn close(mut self) -> Res<()> {
        if let Some(scratch) = &mut self.scratch {
            scratch.close().await.map_err(|e| ExecOutcome::err("SCRATCH", e.to_string()))?;
        }
        Ok(())
    }
}

/// Reads and parses the project's spec document (`formal.tools.spec_doc`, default `spec/SPEC.md`).
pub async fn read_doc(read: &FsRead, spec_doc: &str) -> Result<(SpecDoc, PathBuf), ExecOutcome> {
    let path = in_workspace_of(read, spec_doc).await?;
    let text = read
        .0
        .read(&path.to_string_lossy(), None)
        .await
        .map_err(|e| {
            ExecOutcome::err(
                "NO_SPEC_DOC",
                format!("{} could not be read ({e}); give `spec` (a file) instead of an invariant id", path.display()),
            )
        })?
        .text;
    let doc = SpecDoc::parse(&text).map_err(|e| ExecOutcome::err("SPEC_DOC", e.to_string()))?;
    Ok((doc, path))
}

/// `spec` wins; else the invariant id is looked up in SPEC.md and its blocks materialized.
pub async fn resolve_target(
    read: &FsRead,
    write: &FsWrite,
    spec_doc: &str,
    tool: &str,
    args: &Value,
    id_key: &str,
    want: Option<FormalKind>,
) -> Result<Target, ExecOutcome> {
    let id = opt_str(args, id_key);
    if let Some(p) = opt_str(args, "spec") {
        let path = in_workspace_of(read, p).await?;
        let kind = kind_of_path(&path).ok_or_else(|| bad(tool, format!("{p} is neither a .tla nor a .lean file")))?;
        if let Some(w) = want.filter(|w| *w != kind) {
            return Err(bad(tool, format!("{p} is a {} file; this tool takes {}", kind_name(kind), kind_name(w))));
        }
        let module = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let cfg_file = match opt_str(args, "config") {
            Some(c) => Some(in_workspace_of(read, c).await?),
            None => optional_file(read, path.with_extension("cfg")).await?,
        };
        let doc_mode = match id {
            Some(id) => read_doc(read, spec_doc).await.ok().and_then(|(doc, _)| doc.claim(id).and_then(|c| c.mode())),
            None => None,
        };
        return Ok(Target { spec: SpecRef { kind, path, module }, cfg_file, doc_mode, scratch: None });
    }
    let Some(id) = id else {
        return Err(bad(tool, format!("give `spec` (a file) or `{id_key}` (an invariant id in {})", spec_doc)));
    };
    let (doc, doc_path) = read_doc(read, spec_doc).await?;
    let blocks: Vec<&Block> =
        doc.blocks_for(id).filter(|b| b.lang != BlockLang::Cfg).filter(|b| want.is_none_or(|w| b.kind() == w)).collect();
    let Some(block) = blocks.first() else {
        let what = if doc.claim(id).is_some() { "has no formal block yet (it is Draft)" } else { "is not an invariant in the document" };
        return Err(ExecOutcome::err(
            "NO_FORMAL_BLOCK",
            format!("`{id}` {what}; add a ```tla id={id} or ```lean id={id} block to §6 of {}", spec_doc),
        ));
    };
    let files = doc.extracts().map_err(|e| ExecOutcome::err("SPEC_DOC", e.to_string()))?;
    if read.0.root() != write.0.root() {
        return Err(ExecOutcome::err("SCRATCH", "formal reads and scratch writes must share the same bound workspace"));
    }
    let dir = doc_path.parent().unwrap_or(Path::new("."));
    let mut scratch = write
        .0
        .scratch_dir(&dir.to_string_lossy(), ".aho-spec-")
        .await
        .map_err(|e| ExecOutcome::err("SCRATCH", format!("could not create a scratch dir in {}: {e}", dir.display())))?;
    let staged = async {
        for (rel, text) in &files {
            let p = Path::new(scratch.path()).join(rel);
            write.0.write(&p.to_string_lossy(), text, None).await.map_err(|e| ExecOutcome::err("SCRATCH", e.to_string()))?;
        }
        Ok::<_, ExecOutcome>(())
    }
    .await;
    if let Err(e) = staged {
        scratch.close().await.map_err(|e| ExecOutcome::err("SCRATCH", e.to_string()))?;
        return Err(e);
    }
    let (spec, cfg_file) = match block.lang {
        BlockLang::Tla => {
            let module = doc.tla_module_of(id).unwrap_or_default();
            let path = Path::new(scratch.path()).join("tla").join(format!("{module}.tla"));
            let cfg_rel = PathBuf::from("tla").join(format!("{module}.cfg"));
            let cfg = files.iter().any(|(rel, _)| *rel == cfg_rel).then(|| Path::new(scratch.path()).join(cfg_rel));
            (SpecRef { kind: FormalKind::Tla, path, module }, cfg)
        }
        _ => {
            let module = block.module.clone().unwrap_or_else(|| spec_doc::lean_module_for(id));
            let mut path = Path::new(scratch.path()).join("lean");
            for part in module.split('.') {
                path.push(part);
            }
            path.set_extension("lean");
            (SpecRef { kind: FormalKind::Lean, path, module }, None)
        }
    };
    Ok(Target { spec, cfg_file, doc_mode: doc.claim(id).and_then(|c| c.mode()), scratch: Some(scratch) })
}

/// The model: the cfg file (if any) with the call's overrides on top.
pub async fn check_config(read: &FsRead, tool: &str, args: &Value, t: &Target, default_budget_ms: u64) -> Result<CheckConfig, ExecOutcome> {
    let mut c = match &t.cfg_file {
        Some(p) => {
            let text =
                read.0.read(&p.to_string_lossy(), None).await.map_err(|e| bad(tool, format!("could not read {}: {e}", p.display())))?.text;
            tla::parse_cfg(&text).map_err(|e| bad(tool, e.to_string()))?
        }
        None => CheckConfig::default(),
    };
    if let Some(v) = str_list(args, "invariants") {
        c.invariants = v;
    }
    if let Some(v) = str_list(args, "properties") {
        c.properties = v;
    }
    if let Some(m) = args.get("constants").and_then(Value::as_object) {
        c.constants = m.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect();
    }
    if let Some(d) = args.get("deadlock").and_then(Value::as_bool) {
        c.check_deadlock = d;
    }
    c.mode = match (opt_str(args, "mode"), args.get("bound").and_then(Value::as_u64)) {
        (Some("inductive"), _) => CheckMode::Inductive,
        (Some(m), _) if m != "bounded" => return Err(bad(tool, format!("mode `{m}` is not bounded or inductive"))),
        (_, Some(n)) => CheckMode::Bounded(u32::try_from(n).unwrap_or(u32::MAX)),
        _ => t.doc_mode.filter(|m| matches!(m, CheckMode::Bounded(_) | CheckMode::Inductive)).unwrap_or(CheckMode::Bounded(0)),
    };
    c.budget_ms = args.get("budget_ms").and_then(Value::as_u64).unwrap_or(default_budget_ms);
    Ok(c)
}

impl Inner {
    async fn target(&self, tool: &str, view: &ExecView, id_key: &str, want: Option<FormalKind>) -> Res<Target> {
        resolve_target(&self.read, &self.write, &self.cfg.spec_doc, tool, &view.args, id_key, want).await
    }

    async fn check_config(&self, tool: &str, args: &Value, t: &Target) -> Res<CheckConfig> {
        check_config(&self.read, tool, args, t, self.cfg.budget_ms).await
    }

    /// The code a check starts on: the workspace tree, as `Workspace/Checkpoint` records it.
    /// `Hash::ZERO` (code unknown, so Stale at the first checkpoint) without snapshots or when the
    /// tree cannot be read.
    async fn code(&self) -> Hash {
        match &self.snapshot {
            Some(s) => s.0.tree().await.unwrap_or(Hash::ZERO),
            None => Hash::ZERO,
        }
    }

    async fn run(&self, tool: &'static str, view: ExecView) -> ExecOutcome {
        let r = match tool {
            "spec_check" => self.spec_check(&view).await,
            "spec_trace" => self.spec_trace(&view).await,
            "lean_build" => self.lean_build(&view).await,
            "lean_translate" => self.lean_translate(&view).await,
            "invariant_add" => self.invariant_add(&view).await,
            "invariant_mutate" => self.invariant_mutate(&view).await,
            other => Err(ExecOutcome::err("UNKNOWN_TOOL", format!("tool-formal has no tool `{other}`"))),
        };
        r.unwrap_or_else(|e| e)
    }

    async fn spec_check(&self, view: &ExecView) -> Res<ExecOutcome> {
        let t = self.target("spec_check", view, "invariant", Some(FormalKind::Tla)).await?;
        let result = self.check_target(view, &t).await;
        t.close().await?;
        result
    }

    async fn check_target(&self, view: &ExecView, t: &Target) -> Res<ExecOutcome> {
        let cfg = self.check_config("spec_check", &view.args, t).await?;
        let f = self.formals.get(FormalKind::Tla).map_err(|e| ExecOutcome::err(e.code(), e.to_string()))?;
        let subject = t.spec.module.clone();
        let id = opt_str(&view.args, "invariant");
        let code = self.code().await;
        Ok(match f.check(&t.spec, &cfg).await {
            Ok(o) => verdict_done(&subject, cfg.mode, o, id, code),
            Err(e) => error_done(&subject, cfg.mode, e, id, code),
        })
    }

    async fn spec_trace(&self, view: &ExecView) -> Res<ExecOutcome> {
        let t = self.target("spec_trace", view, "invariant", Some(FormalKind::Tla)).await?;
        let result = self.trace_target(view, &t).await;
        t.close().await?;
        result
    }

    async fn trace_target(&self, view: &ExecView, t: &Target) -> Res<ExecOutcome> {
        let args = &view.args;
        let trace = match (opt_str(args, "log"), opt_str(args, "trace")) {
            (Some(log), None) => {
                let path = in_workspace_of(&self.read, log).await?;
                let text =
                    self.read.0.read(&path.to_string_lossy(), None).await.map_err(|e| bad("spec_trace", format!("{log}: {e}")))?.text;
                let store =
                    aho_log::MemoryLog::from_jsonl(&text).map_err(|e| bad("spec_trace", format!("{log} is not a session log: {e}")))?;
                let Some(head) = store.head() else { return Err(bad("spec_trace", format!("{log} has no entries"))) };
                let entries = store.read_branch(head).map_err(|e| bad("spec_trace", format!("{log}: {e}")))?;
                let from = args.get("from").and_then(Value::as_u64).unwrap_or(0);
                let to = args.get("to").and_then(Value::as_u64).unwrap_or(u64::MAX);
                let slice: Vec<Entry> = entries.into_iter().filter(|e| e.id.0 >= from && e.id.0 <= to).collect();
                trace_for(&t.spec.module, &slice)
            }
            (None, Some(tr)) => {
                let path = in_workspace_of(&self.read, tr).await?;
                let text = self.read.0.read(&path.to_string_lossy(), None).await.map_err(|e| bad("spec_trace", format!("{tr}: {e}")))?.text;
                read_records(&text, &t.spec.module).map_err(|e| bad("spec_trace", format!("{tr}: {e}")))?
            }
            _ => return Err(bad("spec_trace", "give exactly one of `log` (a session .jsonl) or `trace` (action records)")),
        };
        let f = self.formals.get(FormalKind::Tla).map_err(|e| ExecOutcome::err(e.code(), e.to_string()))?;
        let subject = t.spec.module.clone();
        let id = opt_str(args, "invariant");
        let code = self.code().await;
        Ok(match f.trace_validate(&t.spec, &trace).await {
            Ok(o) => verdict_done(&subject, CheckMode::Trace, o, id, code),
            Err(e) => error_done(&subject, CheckMode::Trace, e, id, code),
        })
    }

    async fn lean_build(&self, view: &ExecView) -> Res<ExecOutcome> {
        let module = opt_str(&view.args, "module").unwrap_or("");
        let id = opt_str(&view.args, "invariant");
        let f = self.formals.get(FormalKind::Lean).map_err(|e| ExecOutcome::err(e.code(), e.to_string()))?;
        let subject = if module.is_empty() { "the Lean project".to_string() } else { module.to_string() };
        let code = self.code().await;
        Ok(match f.prove(module).await {
            Ok(o) => verdict_done(&subject, CheckMode::Proof, o, id, code),
            Err(e) => error_done(&subject, CheckMode::Proof, e, id, code),
        })
    }

    async fn lean_translate(&self, view: &ExecView) -> Res<ExecOutcome> {
        let krate = opt_str(&view.args, "crate").ok_or_else(|| bad("lean_translate", "`crate` (the crate's directory) is required"))?;
        let dir = in_workspace_of(&self.read, krate).await?;
        let fns = str_list(&view.args, "fns").unwrap_or_default();
        let f = self.formals.get(FormalKind::Lean).map_err(|e| ExecOutcome::err(e.code(), e.to_string()))?;
        let t = f.translate(&dir.display().to_string(), &fns).await.map_err(|e| ExecOutcome::err(e.code(), e.to_string()))?;
        let summary = format!(
            "Translated {krate}{} to Lean module {} ({}); translation hash {}. Proofs about it hold for exactly this code: re-translate after the Rust changes.",
            if fns.is_empty() { String::new() } else { format!(" ({})", fns.join(", ")) },
            t.module,
            t.path.display(),
            t.hash.short()
        );
        Ok(ExecOutcome::Done {
            value: json!({"module": t.module, "path": t.path, "hash": t.hash, "summary": summary}),
            content: vec![ContentBlock::text(summary.clone())],
            meta: Some(Card::Generic { title: format!("translated {}", t.module), body: summary }),
        })
    }

    async fn invariant_add(&self, view: &ExecView) -> Res<ExecOutcome> {
        let args = &view.args;
        let tool = "invariant_add";
        let id = opt_str(args, "id").ok_or_else(|| bad(tool, "`id` is required"))?;
        if id.len() > 128 || id.chars().any(char::is_whitespace) {
            return Err(bad(tool, format!("`{id}` is not an id: no spaces, at most 128 characters (e.g. inv.order.no-double-charge)")));
        }
        let kind = opt_str(args, "kind").and_then(parse_kind).ok_or_else(|| bad(tool, "`kind` must be tla or lean"))?;
        let statement =
            opt_str(args, "statement").ok_or_else(|| bad(tool, "`statement` (the claim in prose) is required"))?.trim().to_string();
        let covers = str_list(args, "covers").unwrap_or_default();
        let supersedes = opt_str(args, "supersedes");
        let why = opt_str(args, "why");
        let Some(approval) = &self.approval else {
            return Err(ExecOutcome::err(
                "NO_APPROVAL",
                format!(
                    "No approval channel reaches the engineer from here (an async subagent or a headless run), so `{id}` was not registered. An invariant needs their approval; propose it from the main session."
                ),
            ));
        };
        if approval.0.policy() == AskPolicy::Never {
            return Err(ExecOutcome::err(
                "APPROVAL_OFF",
                format!(
                    "Approval prompts are off (policy.ask = never), so `{id}` was not registered and nobody was asked. It needs the engineer; do not retry until prompts are on (`aho set policy.ask ask`)."
                ),
            ));
        }
        let mut rendered = format!("Register invariant `{id}` ({})?\n\n“{statement}”\n", kind_name(kind));
        if !covers.is_empty() {
            rendered.push_str(&format!("\ncovers: {}\n", covers.join(", ")));
        }
        if let Some(old) = supersedes {
            rendered.push_str(&format!("\nsupersedes `{old}`{}\n", why.map(|w| format!(": {w}")).unwrap_or_default()));
        }
        rendered.push_str("\nApprove to pre-register it before the implementation; it counts once it has a formal block that checks and catches a mutant. Reject if it is not what you intend.");
        let card = ApprovalCard {
            tool: tool.into(),
            class: Consequence::TalksToHuman,
            args: args.clone(),
            reason: DecisionReason::ToolCheck("an invariant is a claim about the engineer's intent; only the engineer can make it".into()),
            world: view.policy.world.clone(),
            suggested_cut: None,
            rendered,
        };
        let decided = approval.0.request(card).await;
        match decided.outcome {
            ApprovalOutcome::AllowedOnce => {
                let record =
                    EntryKind::InvariantRegistered { id: id.to_string(), kind, statement: statement.clone(), covers: covers.clone() };
                let mut events = vec![event(record)];
                if let Some(old) = supersedes {
                    events.push(event(EntryKind::Note { text: format!("{id} supersedes {old}: {}", why.unwrap_or("no reason given")) }));
                }
                let summary = format!(
                    "The engineer approved `{id}`; it is registered ({}). Next: give it a formal block, check it, and run invariant_mutate; it counts only once a mutant is caught.",
                    kind_name(kind)
                );
                Ok(ExecOutcome::Done {
                    value: json!({
                        "registered": {"id": id, "kind": kind, "statement": statement, "covers": covers},
                        "supersedes": supersedes,
                        "why": why,
                        "summary": summary,
                        "events": events,
                    }),
                    content: vec![ContentBlock::text(summary.clone())],
                    meta: Some(Card::Generic { title: format!("registered {id}"), body: summary }),
                })
            }
            ApprovalOutcome::Rejected => Err(ExecOutcome::err(
                "REJECTED",
                format!(
                    "The engineer rejected `{id}`; it was not registered. Do not propose it again unchanged; ask what they would claim instead."
                ),
            )),
            ApprovalOutcome::Cancelled => Err(ExecOutcome::err(
                "CANCELLED",
                format!(
                    "The approval for `{id}` was cancelled before anyone answered; it was not registered. Propose it again only if it is still wanted."
                ),
            )),
            ApprovalOutcome::Unavailable => Err(ExecOutcome::err(
                "UNAVAILABLE",
                format!(
                    "Nobody could be asked about `{id}` (no approval channel answered), so it was not registered. Do not retry; tell the engineer it is waiting for them."
                ),
            )),
        }
    }

    async fn invariant_mutate(&self, view: &ExecView) -> Res<ExecOutcome> {
        let t = self.target("invariant_mutate", view, "id", None).await?;
        let result = self.mutate_target(view, &t).await;
        t.close().await?;
        result
    }

    async fn mutate_target(&self, view: &ExecView, t: &Target) -> Res<ExecOutcome> {
        let (cfg, mode) = match t.spec.kind {
            FormalKind::Tla => {
                let c = self.check_config("invariant_mutate", &view.args, t).await?;
                let m = c.mode;
                (c, m)
            }
            FormalKind::Lean => (CheckConfig { mode: CheckMode::Proof, ..CheckConfig::default() }, CheckMode::Proof),
        };
        let f = self.formals.get(t.spec.kind).map_err(|e| ExecOutcome::err(e.code(), e.to_string()))?;
        let code = self.code().await;
        let r = f.mutate(&t.spec, &cfg).await.map_err(|e| ExecOutcome::err(e.code(), e.to_string()))?;
        let id = opt_str(&view.args, "id");
        let caught = r.caught();
        let mut lines = Vec::new();
        for m in &r.mutants {
            let mark = if m.caught {
                "caught"
            } else if matches!(m.verdict, Verdict::Unknown { .. }) {
                "no verdict"
            } else {
                "survived"
            };
            let why = match (&m.violated, &m.verdict) {
                (Some(v), _) => format!("; {v}"),
                (None, Verdict::Unknown { reason }) => format!("; {reason}"),
                _ => String::new(),
            };
            lines.push(format!("- {} [{mark}]: {}{why}", m.name, m.description));
        }
        let summary = if r.mutants.is_empty() {
            format!(
                "No mutants could be made of {} (no guard, comparison or frame in its actions), so it is Vacuous until a mutant exists.",
                t.spec.module
            )
        } else if caught == 0 {
            format!(
                "⚠ Vacuous: {} caught none of {} mutants; the invariant does not constrain what the actions allow. Propose a stronger statement, or a mutant it should catch.",
                t.spec.module,
                r.mutants.len()
            )
        } else {
            format!("{} caught {caught} of {} mutants; the invariant means something.", t.spec.module, r.mutants.len())
        };
        let mut events = Vec::new();
        if let Some(id) = id {
            for m in &r.mutants {
                events.push(event(EntryKind::InvariantMutated { id: id.to_string(), mutant: m.name.clone(), caught: m.caught }));
            }
            if caught == 0 {
                events.push(event(EntryKind::InvariantChecked { id: id.to_string(), mode, verdict: Verdict::Vacuous, code_hash: code }));
            }
        }
        let text = format!("{summary}\n\n{}", lines.join("\n"));
        Ok(ExecOutcome::Done {
            value: json!({"mutants": r.mutants, "caught": caught, "verdict": r.verdict(), "cost_ms": r.cost_ms, "summary": summary, "events": events}),
            content: vec![ContentBlock::text(text)],
            meta: Some(Card::Generic { title: format!("{caught}/{} mutants caught", r.mutants.len()), body: summary }),
        })
    }
}

/// A JSON array, or JSONL, of action records; lines that are session-log entries are mapped
/// with `trace_for(module, …)`.
fn read_records(text: &str, module: &str) -> Result<Vec<Value>, String> {
    let values: Vec<Value> = match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(a)) => a,
        _ => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, l)| serde_json::from_str(l).map_err(|e| format!("line {}: {e}", i + 1)))
            .collect::<Result<_, _>>()?,
    };
    if values.iter().all(|v| v.get("action").is_some()) {
        return Ok(values);
    }
    let entries: Vec<Entry> = values
        .into_iter()
        .enumerate()
        .map(|(i, v)| serde_json::from_value(v).map_err(|e| format!("record {} is neither an action record nor a log entry: {e}", i + 1)))
        .collect::<Result<_, _>>()?;
    Ok(trace_for(module, &entries))
}

fn describe_mode(requested: CheckMode, established: CheckMode) -> String {
    match (requested, established) {
        (CheckMode::Bounded(0), CheckMode::Bounded(d)) => {
            format!("every reachable state of this finite instance (depth {d}); evidence, not proof")
        }
        (_, CheckMode::Bounded(n)) => format!("every behavior of up to {n} steps of this finite instance; evidence, not proof"),
        (_, CheckMode::Inductive) => "all states (inductive invariant)".into(),
        (_, CheckMode::Proof) => "a proof of every theorem".into(),
        (_, CheckMode::Trace) => "the recorded behavior".into(),
    }
}

/// A verdict for the model plus, for a registered invariant, the `InvariantChecked` event.
fn verdict_done(subject: &str, requested: CheckMode, o: Outcome, invariant: Option<&str>, code: Hash) -> ExecOutcome {
    let summary = match &o.verdict {
        Verdict::Pass if o.mode == CheckMode::Trace => format!("Pass: the log is a behavior of {subject} ({} ms).", o.cost_ms),
        Verdict::Pass if o.mode == CheckMode::Proof => format!("Pass: {subject} builds and every theorem is proved ({} ms).", o.cost_ms),
        Verdict::Pass => format!("Pass: no violation in {} of {subject} ({} ms).", describe_mode(requested, o.mode), o.cost_ms),
        Verdict::Fail { counterexample } => format!(
            "Fail: {} (counterexample {}).",
            o.counterexample.as_ref().map(|c| c.violated.as_str()).unwrap_or("a property is violated"),
            counterexample.short()
        ),
        Verdict::Unknown { reason } => format!("Unknown: {reason}."),
        Verdict::Stale => "Stale: the verdict is older than the code.".into(),
        Verdict::Vacuous => "Vacuous: the check proves nothing.".into(),
    };
    let mut text = summary.clone();
    if let Some(cx) = &o.counterexample {
        text.push_str("\n\n");
        text.push_str(&cx.render());
    }
    if !o.goals.is_empty() {
        text.push_str("\n\nOpen goals:\n");
        for g in &o.goals {
            text.push_str(&format!("- {}\n", g.name));
            for h in &g.hypotheses {
                text.push_str(&format!("    {h}\n"));
            }
            text.push_str(&format!("    ⊢ {}\n", g.target));
        }
    }
    let errors: Vec<&Diagnostic> = o.diagnostics.iter().filter(|d| d.severity == "error").take(10).collect();
    if o.goals.is_empty() && !errors.is_empty() {
        text.push_str("\n\nErrors:\n");
        for d in errors {
            text.push_str(&format!("- {}:{}:{}: {}\n", d.file, d.line, d.col, d.message));
        }
    }
    if matches!(o.verdict, Verdict::Unknown { .. }) && !o.output.is_empty() {
        text.push_str("\n\nChecker output (tail):\n");
        text.push_str(&o.output);
    }
    let mut value = serde_json::to_value(&o).unwrap_or(Value::Null);
    if let Some(m) = value.as_object_mut() {
        m.insert("summary".into(), json!(summary));
        let events = match invariant {
            Some(id) => {
                vec![event(EntryKind::InvariantChecked { id: id.to_string(), mode: o.mode, verdict: o.verdict.clone(), code_hash: code })]
            }
            None => Vec::new(),
        };
        m.insert("events".into(), json!(events));
    }
    ExecOutcome::Done {
        value,
        content: vec![ContentBlock::text(text)],
        meta: Some(Card::Generic { title: verdict_word(&o.verdict).into(), body: summary }),
    }
}

/// Errors that are verdicts (`Unknown{timeout|unsupported|tool_missing}`) are recorded as such;
/// the rest are tool errors.
fn error_done(subject: &str, requested: CheckMode, e: FormalError, invariant: Option<&str>, code: Hash) -> ExecOutcome {
    let Some(verdict) = e.verdict() else { return ExecOutcome::err(e.code(), e.to_string()) };
    let summary = format!("Unknown for {subject}: {e}");
    let events = match invariant {
        Some(id) => {
            vec![event(EntryKind::InvariantChecked { id: id.to_string(), mode: requested, verdict: verdict.clone(), code_hash: code })]
        }
        None => Vec::new(),
    };
    ExecOutcome::Done {
        value: json!({"verdict": verdict, "mode": requested, "error": e.code(), "summary": summary, "events": events}),
        content: vec![ContentBlock::text(summary.clone())],
        meta: Some(Card::Generic { title: "unknown".into(), body: summary }),
    }
}

fn verdict_word(v: &Verdict) -> &'static str {
    match v {
        Verdict::Pass => "pass",
        Verdict::Fail { .. } => "fail",
        Verdict::Unknown { .. } => "unknown",
        Verdict::Stale => "stale",
        Verdict::Vacuous => "vacuous",
    }
}
