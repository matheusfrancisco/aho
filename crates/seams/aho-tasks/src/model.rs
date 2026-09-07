//! Plain data of the task graph: ids, kinds, artifacts, nodes, settings, actors.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use aho_types::{AgentId, Hash, Principal, Value};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::TaskError;

/// The single gate over every top-level task; re-opened when a reseed widens its scope.
pub const ROOT_GATE: &str = "gate:root";
/// Longest id a caller may choose.
pub const MAX_ID_LEN: usize = 64;
/// How many recent tool-call summaries a node keeps for `Salvage`.
pub const RECENT_CAP: usize = 8;

// ───────────────────────────────── ids ─────────────────────────────────

/// A caller-chosen task id (`auth-scan`, `t3`, `api/refactor`). Auto-inserted gates are
/// `<parent>/critique`, `<parent>/verify` and `gate:root`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new(s: impl Into<String>) -> TaskId {
        TaskId(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Ids for work nodes: 1–64 chars of `[A-Za-z0-9._:/-]`, outside the reserved gate names.
    pub fn check_user(&self) -> Result<(), TaskError> {
        let s = self.0.as_str();
        let why = if s.is_empty() {
            Some("it is empty")
        } else if s.len() > MAX_ID_LEN {
            Some("it is longer than 64 characters")
        } else if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-')) {
            Some("only letters, digits and . _ : / - are allowed")
        } else if s.starts_with("gate:") || s.ends_with("/critique") || s.ends_with("/verify") {
            Some("`gate:*`, `*/critique` and `*/verify` are reserved for gates the graph inserts")
        } else {
            None
        };
        match why {
            Some(why) => Err(TaskError::InvalidId { id: s.to_string(), why }),
            None => Ok(()),
        }
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TaskId {
    fn from(s: &str) -> TaskId {
        TaskId(s.to_string())
    }
}

/// Id of the gate `kind` inserted between `parent`'s children and its synthesis join.
pub fn gate_id(parent: &TaskId, kind: GateKind) -> TaskId {
    match kind {
        GateKind::Critique => TaskId(format!("{parent}/critique")),
        GateKind::Verify => TaskId(format!("{parent}/verify")),
        GateKind::Root => TaskId(ROOT_GATE.to_string()),
    }
}

// ───────────────────────────────── kinds ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Explore,
    Code,
    Synthesize,
    Gate(GateKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GateKind {
    Critique,
    Verify,
    Root,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Explore => "explore",
            Kind::Code => "code",
            Kind::Synthesize => "synthesize",
            Kind::Gate(GateKind::Critique) => "gate:critique",
            Kind::Gate(GateKind::Verify) => "gate:verify",
            Kind::Gate(GateKind::Root) => "gate:root",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "explore" => Kind::Explore,
            "code" => Kind::Code,
            "synthesize" => Kind::Synthesize,
            "gate:critique" => Kind::Gate(GateKind::Critique),
            "gate:verify" => Kind::Gate(GateKind::Verify),
            "gate:root" => Kind::Gate(GateKind::Root),
            _ => return None,
        })
    }
    /// Kinds a caller may seed, expand into or inject; gates are only ever inserted by the graph.
    pub fn parse_work(s: &str) -> Result<Kind, TaskError> {
        match Kind::parse(s.trim()) {
            Some(k) if !k.is_gate() => Ok(k),
            _ => Err(TaskError::InvalidKind(s.to_string())),
        }
    }
    pub fn is_gate(self) -> bool {
        matches!(self, Kind::Gate(_))
    }
    pub fn gate(self) -> Option<GateKind> {
        match self {
            Kind::Gate(g) => Some(g),
            _ => None,
        }
    }
    /// Code/synthesize nodes hand over files; in deep mode their edge must be a contract.
    pub fn needs_contract(self) -> bool {
        matches!(self, Kind::Code | Kind::Synthesize)
    }
    /// Which gate reviews children of this kind: Verify for code, Critique for everything else.
    pub fn gate_for(self) -> GateKind {
        match self {
            Kind::Code => GateKind::Verify,
            _ => GateKind::Critique,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Kind::parse(&s).ok_or_else(|| D::Error::custom(format!("unknown task kind `{s}`")))
    }
}

/// Growth accounting: seeded work vs work the swarm grew itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Seed,
    Expand,
    Gap,
    Gate,
}

/// Stored status. `Blocked` is deliberately absent: it is computed from dependencies (`Derived`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    /// Was running when the session resumed; its worker has not shown a sign of life since.
    RunningStale,
    Completed,
    Failed,
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::RunningStale => "running (stale)",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
        }
    }
    pub fn is_running(self) -> bool {
        matches!(self, Status::Running | Status::RunningStale)
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Completed | Status::Failed | Status::Stopped)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    /// Artifact-or-nothing: the worker's turn ended twice with the node still running.
    NoArtifact,
    /// The dead-assignee reclaim cap was reached.
    ReclaimCap,
}

impl FailReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FailReason::NoArtifact => "no artifact after a requeue",
            FailReason::ReclaimCap => "reclaim cap reached",
        }
    }
}

/// Operator control ops (12: `Task/Control{op, by: Operator}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    /// Failed/Stopped → queued again with counters reset.
    Retry,
    /// Queued/stale → queued and pinned to a named agent.
    Reassign,
    /// Stale/failed/stopped → a fresh worker; progress notes are dropped.
    Replace,
    /// Forwards the dead worker's last tool summaries + checkpoint as a Snapshot to the new assignee.
    Salvage,
}

impl Control {
    pub fn as_str(self) -> &'static str {
        match self {
            Control::Retry => "retry",
            Control::Reassign => "reassign",
            Control::Replace => "replace",
            Control::Salvage => "salvage",
        }
    }
    pub fn parse(s: &str) -> Option<Control> {
        Some(match s {
            "retry" => Control::Retry,
            "reassign" => Control::Reassign,
            "replace" => Control::Replace,
            "salvage" => Control::Salvage,
            _ => return None,
        })
    }
    /// Stored statuses each op may act on (the safe-state table).
    pub fn safe_from(self) -> &'static [Status] {
        match self {
            Control::Retry => &[Status::Failed, Status::Stopped],
            Control::Reassign => &[Status::Queued, Status::RunningStale],
            Control::Replace => &[Status::RunningStale, Status::Failed, Status::Stopped],
            Control::Salvage => &[Status::Queued, Status::RunningStale, Status::Failed, Status::Stopped],
        }
    }
}

// ───────────────────────────────── confidence ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }

    /// A fraction in `[0, 1]`: ≥ 0.8 high, ≥ 0.5 medium, below that low.
    pub fn from_score(x: f64) -> Option<Confidence> {
        if !(0.0..=1.0).contains(&x) {
            return None;
        }
        Some(if x >= 0.8 {
            Confidence::High
        } else if x >= 0.5 {
            Confidence::Medium
        } else {
            Confidence::Low
        })
    }

    /// `0.9`, `9` (of 10), `85` (of 100) — the scale is inferred from the magnitude.
    fn from_scaled(x: f64) -> Option<Confidence> {
        if x < 0.0 || !x.is_finite() {
            return None;
        }
        let f = if x <= 1.0 {
            x
        } else if x <= 10.0 {
            x / 10.0
        } else if x <= 100.0 {
            x / 100.0
        } else {
            return None;
        };
        Confidence::from_score(f)
    }

    /// Model-facing parse. Numbers, `"7/10"`, `"85%"`, `"0.9"`, words (`high`, `fairly sure`),
    /// and negations (`not confident` → low, `not entirely sure` → medium, `not low` → medium).
    /// Several signals resolve to the lowest one (`medium-high` → medium). `None` = unreadable.
    pub fn parse_lenient(v: &Value) -> Option<Confidence> {
        match v {
            Value::Number(n) => n.as_f64().and_then(Confidence::from_scaled),
            Value::String(s) => Confidence::parse_text(s),
            _ => None,
        }
    }

    pub fn parse_text(s: &str) -> Option<Confidence> {
        let t = s.trim().to_lowercase().replace("n't", " not").replace("out of", "/");
        if t.is_empty() {
            return None;
        }
        // Every readable signal counts; the lowest wins. Unreadable ones (`not moderate`, `250`) add nothing.
        let mut levels: Vec<Confidence> = Vec::new();

        // numeric signals: `7/10`, `7 / 10`, `85%`, `0.9`
        let compact: String = t.split('/').map(str::trim).collect::<Vec<_>>().join("/");
        for tok in compact.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '=')) {
            let tok = tok.trim_matches(|c: char| matches!(c, '.' | ':' | '~'));
            if tok.is_empty() || !tok.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
                continue;
            }
            let level = if let Some((a, b)) = tok.split_once('/') {
                match (a.parse::<f64>(), b.parse::<f64>()) {
                    (Ok(a), Ok(b)) if b > 0.0 => Confidence::from_score(a / b),
                    _ => None,
                }
            } else if let Some(p) = tok.strip_suffix('%') {
                p.parse::<f64>().ok().and_then(|p| Confidence::from_score(p / 100.0))
            } else {
                tok.parse::<f64>().ok().and_then(Confidence::from_scaled)
            };
            levels.extend(level);
        }

        // word signals, each with a negation scope of the three words before it
        const NEGATORS: &[&str] = &["not", "no", "never", "hardly", "barely", "scarcely", "without", "lacking"];
        const HIGH: &[&str] = &[
            "high",
            "highly",
            "confident",
            "certain",
            "certainly",
            "sure",
            "strong",
            "strongly",
            "definite",
            "definitely",
            "verified",
            "conclusive",
        ];
        const MEDIUM: &[&str] = &[
            "medium",
            "moderate",
            "moderately",
            "fair",
            "fairly",
            "somewhat",
            "reasonably",
            "reasonable",
            "partial",
            "partially",
            "mid",
            "mixed",
            "decent",
        ];
        const LOW: &[&str] = &[
            "low",
            "weak",
            "unsure",
            "uncertain",
            "unconfident",
            "doubtful",
            "guess",
            "guessing",
            "speculative",
            "tentative",
            "unverified",
            "shaky",
            "unclear",
            "poor",
        ];
        const SOFTENERS: &[&str] = &["entirely", "fully", "completely", "totally", "absolutely", "quite", "100%", "100"];
        let words: Vec<&str> = t.split(|c: char| !(c.is_alphanumeric() || c == '%')).filter(|w| !w.is_empty()).collect();
        for (i, w) in words.iter().enumerate() {
            let base = if HIGH.contains(w) {
                Confidence::High
            } else if MEDIUM.contains(w) {
                Confidence::Medium
            } else if LOW.contains(w) {
                Confidence::Low
            } else {
                continue;
            };
            let window = &words[i.saturating_sub(3)..i];
            let level = match (base, window.iter().rposition(|x| NEGATORS.contains(x))) {
                (b, None) => Some(b),
                // `not entirely sure` is hedged, `not confident` / `not very sure` is low
                (Confidence::High, Some(at)) => {
                    Some(if window[at + 1..].iter().any(|x| SOFTENERS.contains(x)) { Confidence::Medium } else { Confidence::Low })
                }
                (Confidence::Low, Some(_)) => Some(Confidence::Medium),
                (Confidence::Medium, Some(_)) => None,
            };
            levels.extend(level);
        }
        levels.into_iter().min()
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ───────────────────────────────── artifacts ─────────────────────────────────

/// The dependency edge is the data channel. `what_i_did_not_check` is mandatory: the negative space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HandoffArtifact {
    pub findings: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_cases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
    pub confidence: Confidence,
    #[serde(default)]
    pub what_i_did_not_check: Vec<String>,
    /// The edge as an interface: what this node hands the dependent, in what form, and how the dependent
    /// can check it arrived intact. Absent for explore nodes; required for code/synthesize nodes in deep mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<Contract>,
    /// Gates only: every done task in scope this verdict covers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub covers: Vec<TaskId>,
}

/// A dependency edge is a contract, not a message: filenames, formats, units, versions, acceptance checks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    pub outputs: Vec<Output>,
    /// e.g. "lengths in mm", "coordinates Y-up", "timestamps UTC".
    #[serde(default)]
    pub units: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Commands or predicates the dependent runs before trusting the inputs; failure re-opens the producer.
    pub acceptance: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Output {
    pub path: String,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<Hash>,
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() { vec![] } else { vec![s.to_string()] }
        }
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.trim().to_string(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    }
}

fn opt_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

impl HandoffArtifact {
    /// Model-facing parse: list fields take a string or an array, `confidence` is lenient.
    /// Rejects thin artifacts (empty findings, empty `what_i_did_not_check`, unreadable confidence).
    pub fn parse(v: &Value) -> Result<HandoffArtifact, TaskError> {
        let Some(o) = v.as_object() else {
            return Err(TaskError::BadArtifact("the artifact must be an object with findings, confidence and what_i_did_not_check".into()));
        };
        let findings = match o.get("findings") {
            Some(Value::String(s)) => s.trim().to_string(),
            Some(Value::Array(_)) => string_list(o.get("findings")).join("\n"),
            _ => String::new(),
        };
        let confidence = o.get("confidence").and_then(Confidence::parse_lenient).ok_or(TaskError::ThinArtifact("confidence"))?;
        let contract = match o.get("contract") {
            None | Some(Value::Null) => None,
            Some(c) => Some(Contract::parse(c)?),
        };
        let a = HandoffArtifact {
            findings,
            evidence: string_list(o.get("evidence")),
            edge_cases: string_list(o.get("edge_cases")),
            validation: opt_string(o.get("validation")),
            open_questions: string_list(o.get("open_questions")),
            confidence,
            what_i_did_not_check: string_list(o.get("what_i_did_not_check")),
            contract,
            covers: string_list(o.get("covers")).into_iter().map(TaskId).collect(),
        };
        if let Some(why) = a.thin_reason() {
            return Err(TaskError::ThinArtifact(why));
        }
        Ok(a)
    }

    /// The field that makes this artifact too thin to hand over, if any.
    pub fn thin_reason(&self) -> Option<&'static str> {
        if self.findings.trim().is_empty() {
            return Some("findings");
        }
        if self.what_i_did_not_check.iter().all(|s| s.trim().is_empty()) {
            return Some("what_i_did_not_check");
        }
        if let Some(c) = &self.contract {
            return c.thin_reason();
        }
        None
    }
}

impl Contract {
    pub fn parse(v: &Value) -> Result<Contract, TaskError> {
        let Some(o) = v.as_object() else {
            return Err(TaskError::BadArtifact("`contract` must be an object with outputs, units, version and acceptance".into()));
        };
        let mut outputs = Vec::new();
        if let Some(Value::Array(items)) = o.get("outputs") {
            for (i, it) in items.iter().enumerate() {
                let path = it.get("path").and_then(Value::as_str).unwrap_or("").trim().to_string();
                let format = it.get("format").and_then(Value::as_str).unwrap_or("").trim().to_string();
                let hash =
                    match it.get("hash") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(h)) if h.trim().is_empty() => None,
                        Some(Value::String(h)) => Some(Hash::from_hex(h.trim().trim_start_matches("blake3:")).ok_or_else(|| {
                            TaskError::BadArtifact(format!("contract.outputs[{i}].hash must be a 64-character hex digest"))
                        })?),
                        Some(_) => return Err(TaskError::BadArtifact(format!("contract.outputs[{i}].hash must be a hex string"))),
                    };
                outputs.push(Output { path, format, hash });
            }
        }
        Ok(Contract {
            outputs,
            units: string_list(o.get("units")),
            version: opt_string(o.get("version")),
            acceptance: string_list(o.get("acceptance")),
        })
    }

    pub fn thin_reason(&self) -> Option<&'static str> {
        if self.outputs.is_empty() || self.outputs.iter().any(|o| o.path.trim().is_empty() || o.format.trim().is_empty()) {
            return Some("contract.outputs");
        }
        if self.acceptance.iter().all(|s| s.trim().is_empty()) {
            return Some("contract.acceptance");
        }
        None
    }
}

// ───────────────────────────────── salvage ─────────────────────────────────

/// What a replacement worker inherits from a dead one (12: Salvage → Snapshot).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Salvage {
    #[serde(default, with = "agent_serde::opt", skip_serializing_if = "Option::is_none")]
    pub from: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    #[serde(default)]
    pub summaries: Vec<String>,
}

impl Salvage {
    pub fn is_empty(&self) -> bool {
        self.checkpoint.as_deref().map(str::trim).unwrap_or("").is_empty() && self.summaries.is_empty()
    }
}

// ───────────────────────────────── nodes ─────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: TaskId,
    /// Insertion order; ties in dispatch break on it.
    pub seq: u64,
    pub content: String,
    pub kind: Kind,
    pub origin: Origin,
    pub status: Status,
    /// Current holder while running.
    #[serde(default, with = "agent_serde::opt", skip_serializing_if = "Option::is_none")]
    pub owner: Option<AgentId>,
    /// The last agent that held it (completed it, died on it, released it).
    #[serde(default, with = "agent_serde::opt", skip_serializing_if = "Option::is_none")]
    pub last_owner: Option<AgentId>,
    /// Who expanded this node; its synthesis join prefers that agent (planner affinity).
    #[serde(default, with = "agent_serde::opt", skip_serializing_if = "Option::is_none")]
    pub planner: Option<AgentId>,
    /// Set by an operator Reassign: only this agent may start it.
    #[serde(default, with = "agent_serde::opt", skip_serializing_if = "Option::is_none")]
    pub pinned: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<TaskId>,
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<HandoffArtifact>,
    #[serde(default)]
    pub reclaims: u32,
    #[serde(default)]
    pub no_artifact_requeues: u32,
    #[serde(default)]
    pub reopened: u32,
    /// Gates: the scope (deps) snapshot taken when the gate started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dispatched_scope: Vec<TaskId>,
    /// Gate-injected work: the gate that injected it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injected_by: Option<TaskId>,
    /// Gate-injected work: the low-confidence siblings it addresses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salvage: Option<Salvage>,
    /// Why a dependent re-opened this node (failed acceptance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailReason>,
}

impl Node {
    pub(crate) fn new(spec: &crate::NodeSpec, seq: u64) -> Node {
        Node {
            id: spec.id.clone(),
            seq,
            content: spec.content.clone(),
            kind: spec.kind,
            origin: spec.origin,
            status: Status::Queued,
            owner: None,
            last_owner: None,
            planner: None,
            pinned: None,
            parent: spec.parent.clone(),
            depends_on: spec.depends_on.clone(),
            expanded: false,
            artifact: None,
            reclaims: 0,
            no_artifact_requeues: 0,
            reopened: 0,
            dispatched_scope: vec![],
            injected_by: None,
            addresses: vec![],
            checkpoint: None,
            recent: vec![],
            salvage: None,
            feedback: None,
            failure: None,
        }
    }

    pub fn confidence(&self) -> Option<Confidence> {
        self.artifact.as_ref().map(|a| a.confidence)
    }

    pub(crate) fn push_recent(&mut self, s: String) {
        self.recent.push(s);
        if self.recent.len() > RECENT_CAP {
            let drop = self.recent.len() - RECENT_CAP;
            self.recent.drain(..drop);
        }
    }
}

// ───────────────────────────────── settings ─────────────────────────────────

/// Swarm knobs, logged as `Task/Configured` so the fold validates with the values in force.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Deep mode: code/synthesize artifacts must carry a `contract`.
    pub deep: bool,
    /// Hard member cap: distinct agents that may ever take a node.
    pub max_members: u32,
    /// Live-worker budget: distinct agents holding a running node at once.
    pub max_live_workers: u32,
    /// Dead-assignee reclaims per node before it fails.
    pub max_reclaims: u32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { deep: false, max_members: 16, max_live_workers: 4, max_reclaims: 2 }
    }
}

impl Settings {
    pub fn check(&self) -> Result<(), TaskError> {
        if self.max_members == 0 || self.max_live_workers == 0 {
            return Err(TaskError::BadEvent("swarm caps must be at least 1 (swarm.max_members, swarm.max_live_workers)".into()));
        }
        Ok(())
    }
}

// ───────────────────────────────── actors ─────────────────────────────────

/// Who caused a task event. The custom fold never sees the entry's principal, so bodies carry it.
/// Serialized as `operator`, `engineer`, `agent:<n>`, `system:<name>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Actor {
    Operator,
    Engineer,
    Agent(AgentId),
    System(String),
}

impl Actor {
    pub fn of(p: &Principal) -> Actor {
        match p {
            Principal::Operator => Actor::Operator,
            Principal::Engineer => Actor::Engineer,
            Principal::Agent { id, .. } => Actor::Agent(*id),
            Principal::System { name } => Actor::System(name.clone()),
        }
    }
    pub fn agent(&self) -> Option<AgentId> {
        match self {
            Actor::Agent(a) => Some(*a),
            _ => None,
        }
    }
    pub fn is_human(&self) -> bool {
        matches!(self, Actor::Operator | Actor::Engineer)
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Actor::Operator => f.write_str("operator"),
            Actor::Engineer => f.write_str("engineer"),
            Actor::Agent(a) => write!(f, "agent:{}", a.0),
            Actor::System(n) => write!(f, "system:{n}"),
        }
    }
}

impl FromStr for Actor {
    type Err = String;
    fn from_str(s: &str) -> Result<Actor, String> {
        match s {
            "operator" => Ok(Actor::Operator),
            "engineer" => Ok(Actor::Engineer),
            _ => {
                if let Some(n) = s.strip_prefix("system:") {
                    return Ok(Actor::System(n.to_string()));
                }
                parse_agent(s).map(Actor::Agent).ok_or_else(|| format!("unknown actor `{s}`"))
            }
        }
    }
}

impl Serialize for Actor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Actor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(D::Error::custom)
    }
}

/// `agent:12` or `12`.
pub fn parse_agent(s: &str) -> Option<AgentId> {
    let n = s.trim();
    let n = n.strip_prefix("agent:").unwrap_or(n);
    n.parse::<u128>().ok().map(AgentId)
}

pub fn agent_label(a: AgentId) -> String {
    format!("agent:{}", a.0)
}

/// `AgentId` is a `u128`; JSON values hold at most 64-bit integers, so ids travel as `agent:<n>`.
pub mod agent_serde {
    use super::*;

    pub fn serialize<S: Serializer>(a: &AgentId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&agent_label(*a))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<AgentId, D::Error> {
        let s = String::deserialize(d)?;
        parse_agent(&s).ok_or_else(|| D::Error::custom(format!("`{s}` is not an agent id")))
    }

    pub mod opt {
        use super::*;
        pub fn serialize<S: Serializer>(a: &Option<AgentId>, s: S) -> Result<S::Ok, S::Error> {
            match a {
                Some(a) => s.serialize_some(&agent_label(*a)),
                None => s.serialize_none(),
            }
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<AgentId>, D::Error> {
            match Option::<String>::deserialize(d)? {
                None => Ok(None),
                Some(s) => parse_agent(&s).map(Some).ok_or_else(|| D::Error::custom(format!("`{s}` is not an agent id"))),
            }
        }
    }

    pub mod set {
        use super::*;
        pub fn serialize<S: Serializer>(a: &BTreeSet<AgentId>, s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(a.iter().map(|x| agent_label(*x)))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeSet<AgentId>, D::Error> {
            let v = Vec::<String>::deserialize(d)?;
            v.iter().map(|s| parse_agent(s).ok_or_else(|| D::Error::custom(format!("`{s}` is not an agent id")))).collect()
        }
    }
}
