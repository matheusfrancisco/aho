//! `13`/`14` — TLA+ and Lean as capabilities. Verdicts are closed (`aho_log::Verdict`); a
//! verdict older than the code is `Stale`; an invariant that catches no mutant is `Vacuous` and
//! does not count.
//!
//! Two seams, because the two checkers answer different questions and are installed separately:
//! [`TLA_SEAM`] (TLC today, Apalache as an alternative binding) and [`LEAN_SEAM`]. Each provider
//! registers its [`FormalHandle`] as the named service [`service_name`]`(kind)`; consumers collect
//! them with [`Formals::resolve`] (the Registry holds one handle per kind, and there are two).
//!
//! Beside the seam this crate holds what every provider and tool shares: the trace mapping from
//! log entries to TLA+ action records ([`trace_of`], and [`trace_for`] per spec module), TLA+
//! value rendering/parsing ([`tla`]), and the literate System Spec of doc 14 ([`spec_doc`]).

#![forbid(unsafe_code)]

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::{Handle, HandleKind, MissingDep, Resolver, SeamId};
use aho_types::{BoxFuture, Hash, Value};
use serde::{Deserialize, Serialize};

pub use aho_log::{CheckMode, FormalKind, Verdict};

pub mod spec_doc;
pub mod tla;
mod trace;
pub use spec_doc::{
    Block, BlockLang, Claim, ExtractError, ExtractReport, Freshness, Ledger, SpecDoc, SpecError, SpecSection, Status, extract, freshness,
    render_generated, render_ledger, status,
};
pub use trace::{TASKS_MODULE, trace_for, trace_of};

/// Model checking of TLA+ modules (TLC, Apalache).
pub const TLA_SEAM: SeamId = SeamId("tla");
/// Lean 4 proofs and Aeneas translation.
pub const LEAN_SEAM: SeamId = SeamId("lean");

/// Registry service under which a provider puts its [`FormalHandle`].
pub const fn service_name(kind: FormalKind) -> &'static str {
    match kind {
        FormalKind::Tla => "formal:tla",
        FormalKind::Lean => "formal:lean",
    }
}

pub const fn kind_name(kind: FormalKind) -> &'static str {
    match kind {
        FormalKind::Tla => "tla",
        FormalKind::Lean => "lean",
    }
}

pub fn parse_kind(s: &str) -> Option<FormalKind> {
    match s.to_ascii_lowercase().as_str() {
        "tla" | "tla+" | "tlaplus" => Some(FormalKind::Tla),
        "lean" | "lean4" => Some(FormalKind::Lean),
        _ => None,
    }
}

/// A spec file the checker reads. `path` is a host path (the tool resolves workspace-relative
/// paths before calling); `module` is the TLA+ module / Lean module name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecRef {
    pub kind: FormalKind,
    pub path: PathBuf,
    pub module: String,
}

impl SpecRef {
    /// A TLA+ module file; the module name is the file stem (TLC requires they match).
    pub fn tla(path: impl Into<PathBuf>) -> SpecRef {
        let path = path.into();
        let module = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        SpecRef { kind: FormalKind::Tla, path, module }
    }
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }
}

/// What to check and how hard. For TLC, `Bounded(n)` explores every behavior of at most `n`
/// steps of the finite instance fixed by `constants`; `Bounded(0)` explores the whole instance
/// (the outcome then reports the depth actually reached).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckConfig {
    pub mode: CheckMode,
    pub init: String,
    pub next: String,
    pub invariants: Vec<String>,
    /// Temporal / action properties (`PROPERTY`).
    pub properties: Vec<String>,
    /// `(name, TLA+ value)` — `("N", "3")`, `("Procs", "{p1, p2}")` (model values allowed).
    pub constants: Vec<(String, String)>,
    /// State constraints (`CONSTRAINT`), by operator name.
    pub constraints: Vec<String>,
    pub check_deadlock: bool,
    /// Wall-clock budget; past it the checker is killed and the verdict is `Unknown{timeout}`.
    pub budget_ms: u64,
}

impl Default for CheckConfig {
    fn default() -> Self {
        CheckConfig {
            mode: CheckMode::Bounded(0),
            init: "Init".into(),
            next: "Next".into(),
            invariants: Vec::new(),
            properties: Vec::new(),
            constants: Vec::new(),
            constraints: Vec::new(),
            check_deadlock: false,
            budget_ms: 120_000,
        }
    }
}

/// A behavior that violates something: one JSON object per state,
/// `{"step": n, "action": "...", "vars": {name: value}, "tla": {name: "<TLA+ text>"}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Counterexample {
    pub states: Vec<Value>,
    pub violated: String,
}

impl Counterexample {
    /// Content-addressed reference recorded in `Verdict::Fail{counterexample}`.
    pub fn hash(&self) -> Hash {
        Hash::of_json(self)
    }

    /// The model-readable trace: one block per state, the violated property named first.
    pub fn render(&self) -> String {
        let mut out = format!("Violated: {}\n", self.violated);
        for s in &self.states {
            let step = s.get("step").and_then(Value::as_u64).unwrap_or(0);
            let action = s.get("action").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("State {step}: <{action}>\n"));
            if let Some(vars) = s.get("tla").and_then(Value::as_object) {
                for (k, v) in vars {
                    out.push_str(&format!("  {k} = {}\n", v.as_str().unwrap_or("")));
                }
            }
        }
        out
    }
}

/// An unproved Lean goal: the declaration it belongs to, the local context, and the target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub name: String,
    pub hypotheses: Vec<String>,
    pub target: String,
}

/// A located checker message that is not a goal (parse/type errors, warnings).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub severity: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub verdict: Verdict,
    /// The mode actually established (e.g. `Bounded(depth reached)` for an exhaustive TLC run).
    pub mode: CheckMode,
    pub counterexample: Option<Counterexample>,
    pub goals: Vec<Goal>,
    pub diagnostics: Vec<Diagnostic>,
    /// Hash of every input the verdict depends on (spec text, config, trace).
    pub spec_hash: Hash,
    pub cost_ms: u64,
    /// Tail of the checker's own output, for the model when the verdict is `Unknown`.
    pub output: String,
}

impl Outcome {
    pub fn unknown(reason: impl Into<String>, mode: CheckMode, spec_hash: Hash, cost_ms: u64, output: String) -> Outcome {
        Outcome {
            verdict: Verdict::Unknown { reason: reason.into() },
            mode,
            counterexample: None,
            goals: Vec::new(),
            diagnostics: Vec::new(),
            spec_hash,
            cost_ms,
            output,
        }
    }
}

/// Aeneas output: the Lean module generated from a crate, bound to the translation hash (13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Translation {
    pub module: String,
    pub path: PathBuf,
    pub hash: Hash,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutantOutcome {
    /// Stable, distinct name, e.g. `drop-guard:Result#2`.
    pub name: String,
    /// What was changed, in words.
    pub description: String,
    /// The checker rejected the mutant (a `Fail` verdict).
    pub caught: bool,
    pub verdict: Verdict,
    /// Which property rejected it, when caught.
    pub violated: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MutationReport {
    pub mutants: Vec<MutantOutcome>,
    pub cost_ms: u64,
}

impl MutationReport {
    pub fn caught(&self) -> usize {
        self.mutants.iter().filter(|m| m.caught).count()
    }
    /// `Vacuous` when no mutant was caught (13: it does not count), else `Pass`.
    pub fn verdict(&self) -> Verdict {
        if self.caught() == 0 { Verdict::Vacuous } else { Verdict::Pass }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormalError {
    /// A checker binary or file is not there; `fix` is the command that installs it.
    ToolMissing {
        tool: String,
        fix: String,
    },
    Timeout {
        after_ms: u64,
    },
    /// The backend cannot answer this kind of question (TLC and `Inductive`, Lean and traces).
    Unsupported(String),
    /// The request itself is wrong (no such spec file, a trace value TLA+ cannot hold).
    Invalid(String),
    /// The checker failed in a way that is not a verdict.
    Tool(String),
}

impl FormalError {
    /// The closed verdict this error stands for, when it stands for one (13:
    /// `Unknown{Timeout|Unsupported|ToolMissing}`); `None` for request/tool failures.
    pub fn verdict(&self) -> Option<Verdict> {
        let reason = match self {
            FormalError::ToolMissing { tool, .. } => format!("tool_missing: {tool}"),
            FormalError::Timeout { after_ms } => format!("timeout: {after_ms} ms"),
            FormalError::Unsupported(what) => format!("unsupported: {what}"),
            FormalError::Invalid(_) | FormalError::Tool(_) => return None,
        };
        Some(Verdict::Unknown { reason })
    }
    pub fn code(&self) -> &'static str {
        match self {
            FormalError::ToolMissing { .. } => "FORMAL_TOOL_MISSING",
            FormalError::Timeout { .. } => "FORMAL_TIMEOUT",
            FormalError::Unsupported(_) => "FORMAL_UNSUPPORTED",
            FormalError::Invalid(_) => "FORMAL_INVALID",
            FormalError::Tool(_) => "FORMAL_TOOL",
        }
    }
}

impl fmt::Display for FormalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormalError::ToolMissing { tool, fix } => {
                write!(f, "{tool} is not available, so nothing was checked. Install it with `{fix}`.")
            }
            FormalError::Timeout { after_ms } => write!(
                f,
                "the checker ran past its {after_ms} ms budget and was stopped; the verdict is Unknown. Raise the budget or shrink the model's constants."
            ),
            FormalError::Unsupported(what) => write!(f, "{what}"),
            FormalError::Invalid(what) => write!(f, "{what}"),
            FormalError::Tool(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for FormalError {}

pub trait Formal: Send + Sync {
    fn kind(&self) -> FormalKind;
    fn check<'a>(&'a self, spec: &'a SpecRef, cfg: &'a CheckConfig) -> BoxFuture<'a, Result<Outcome, FormalError>>;
    /// Real logs are behaviors of the spec: `trace` (see [`trace_of`]) must be a behavior.
    fn trace_validate<'a>(&'a self, spec: &'a SpecRef, trace: &'a [Value]) -> BoxFuture<'a, Result<Outcome, FormalError>>;
    /// Build a proof module (`None`-like `""` = the whole project).
    fn prove<'a>(&'a self, module: &'a str) -> BoxFuture<'a, Result<Outcome, FormalError>>;
    /// Aeneas: safe-Rust subset → Lean; the proof is bound to the translation hash.
    fn translate<'a>(&'a self, krate: &'a str, fns: &'a [String]) -> BoxFuture<'a, Result<Translation, FormalError>>;
    /// Must catch at least one mutant or the invariant is `Vacuous`.
    fn mutate<'a>(&'a self, spec: &'a SpecRef, cfg: &'a CheckConfig) -> BoxFuture<'a, Result<MutationReport, FormalError>>;
}

/// `SpawnsProcess` ceiling (checkers run inside the world).
#[derive(Clone)]
pub struct FormalHandle(pub Arc<dyn Formal>);

impl Handle for FormalHandle {
    const KIND: HandleKind = HandleKind::Formal;
}

impl fmt::Debug for FormalHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FormalHandle({})", kind_name(self.0.kind()))
    }
}

/// Every active formal backend, keyed by kind.
#[derive(Clone, Debug, Default)]
pub struct Formals {
    pub tla: Option<FormalHandle>,
    pub lean: Option<FormalHandle>,
}

impl Formals {
    /// Collects the `formal:*` services. Errors (as a missing `Formal` handle) only when no
    /// backend at all is active; a missing single backend is reported per call by [`Formals::get`].
    pub fn resolve(r: &Resolver) -> Result<Formals, MissingDep> {
        let f = Formals {
            tla: r.service::<FormalHandle>(service_name(FormalKind::Tla)),
            lean: r.service::<FormalHandle>(service_name(FormalKind::Lean)),
        };
        if f.tla.is_none() && f.lean.is_none() { Err(MissingDep(HandleKind::Formal)) } else { Ok(f) }
    }

    pub fn get(&self, kind: FormalKind) -> Result<&dyn Formal, FormalError> {
        let h = match kind {
            FormalKind::Tla => &self.tla,
            FormalKind::Lean => &self.lean,
        };
        h.as_ref().map(|h| h.0.as_ref()).ok_or_else(|| FormalError::ToolMissing {
            tool: match kind {
                FormalKind::Tla => "The TLA+ checker (formal-tlc)".into(),
                FormalKind::Lean => "The Lean toolchain (formal-lean)".into(),
            },
            fix: match kind {
                FormalKind::Tla => "aho doctor formal --install tla".into(),
                FormalKind::Lean => "aho doctor formal --install lean".into(),
            },
        })
    }
}

/// Last `max` bytes of checker output on a char boundary, for `Outcome::output`.
pub fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_closed_unknown_verdicts_only_when_they_are_verdicts() {
        let miss = FormalError::ToolMissing { tool: "TLC".into(), fix: "aho doctor formal --install tla".into() };
        assert!(matches!(miss.verdict(), Some(Verdict::Unknown { reason }) if reason.starts_with("tool_missing")));
        assert!(
            matches!(FormalError::Timeout { after_ms: 5 }.verdict(), Some(Verdict::Unknown { reason }) if reason.starts_with("timeout"))
        );
        assert!(
            matches!(FormalError::Unsupported("x".into()).verdict(), Some(Verdict::Unknown { reason }) if reason.starts_with("unsupported"))
        );
        assert_eq!(FormalError::Invalid("bad".into()).verdict(), None);
        assert!(miss.to_string().contains("aho doctor formal --install tla"));
    }

    #[test]
    fn a_report_with_no_caught_mutant_is_vacuous() {
        let m = |caught| MutantOutcome { name: "m".into(), description: String::new(), caught, verdict: Verdict::Pass, violated: None };
        assert_eq!(MutationReport { mutants: vec![m(false), m(false)], cost_ms: 0 }.verdict(), Verdict::Vacuous);
        assert_eq!(MutationReport { mutants: vec![m(false), m(true)], cost_ms: 0 }.verdict(), Verdict::Pass);
        assert_eq!(MutationReport::default().verdict(), Verdict::Vacuous, "no mutants at all proves nothing");
    }

    #[test]
    fn missing_backend_names_its_install_fix() {
        let f = Formals::default();
        let e = f.get(FormalKind::Lean).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e.contains("aho doctor formal --install lean"), "{e}");
    }

    #[test]
    fn tail_keeps_char_boundaries() {
        assert_eq!(tail("abc", 10), "abc");
        let t = tail("ééééé", 3);
        assert!(t.starts_with('…') && t.ends_with('é'));
    }
}
