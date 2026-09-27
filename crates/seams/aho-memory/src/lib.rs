//! Long-term memory. Hybrid dense+BM25 → RRF → listwise rerank with consensus →
//! carry-verified degradation. Injection is a source-labelled Snapshot entry with TTL/overlap
//! suppression ([`Suppressor`]). Extraction runs through an [`Extractor`] with no tools (policy
//! `Nothing`) under `System("memory")`.
//!
//! This crate is the contract: entry shape, the traits a provider implements ([`Memory`]) or
//! plugs in ([`Judge`], [`Extractor`], [`Clock`]), the pure ranking rules ([`rrf`],
//! [`dynamic_gate`]), and the `Memory/*` log kinds with their fold ([`fold`]).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aho_cap::{Handle, HandleKind, SeamId};
use aho_embed::{ModelId, Vector};
use aho_log::{CapDeltaOp, EntryKind};
use aho_types::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const SEAM: SeamId = SeamId("memory");
/// Namespace of every `Custom` entry this seam defines (`Memory/Pruned`, …).
pub const NS: &str = "Memory";

pub const DEFAULT_CONFIDENCE: f32 = 0.5;
/// Maintenance prunes entries below this confidence …
pub const PRUNE_BELOW: f32 = 0.15;
/// … once they are older than this (24 h).
pub const PRUNE_AFTER_MS: u64 = 24 * 60 * 60 * 1000;
/// Same-model cosine at or above which a write merges into the existing entry.
pub const DEDUPE_COSINE: f32 = 0.92;
/// Reciprocal-rank-fusion constant.
pub const RRF_K: f32 = 60.0;
/// Candidate pool floor for fusion and rerank.
pub const MIN_POOL: usize = 50;
/// `Cap/Delta` reason for a skill activated by a semantic hit (beside `PathMatch`).
pub const SEMANTIC_MATCH: &str = "SemanticMatch";

// ───────────────────────────── entries ─────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Stored in the per-project file.
    #[default]
    Project,
    /// Stored in the file shared by every project.
    Global,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    RelatesTo,
    /// This entry replaces the target (a contradiction was detected at write time).
    Supersedes,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub text: String,
    pub tags: Vec<String>,
    pub embedding: Option<Vector>,
    /// Always `embedding.model` when an embedding is present; dense search only compares equal models.
    pub embedding_model: Option<ModelId>,
    pub confidence: f32,
    pub edges: Vec<(u64, Edge)>,
    pub created_at: Timestamp,
    /// Last time this entry was injected into a conversation.
    pub injected_at: Option<Timestamp>,
    #[serde(default)]
    pub scope: Scope,
    /// Synthetic entries stand for a skill; a hit suggests activating it. Never persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
}

impl Entry {
    /// A new, unsaved entry. The store assigns `id` and stamps `created_at` on insert.
    pub fn new(text: impl Into<String>, tags: Vec<String>) -> Entry {
        Entry {
            id: 0,
            text: text.into(),
            tags,
            embedding: None,
            embedding_model: None,
            confidence: DEFAULT_CONFIDENCE,
            edges: Vec::new(),
            created_at: Timestamp(0),
            injected_at: None,
            scope: Scope::Project,
            skill: None,
        }
    }

    pub fn with_embedding(mut self, v: Vector) -> Entry {
        self.embedding_model = Some(v.model.clone());
        self.embedding = Some(v);
        self
    }

    pub fn supersedes(&self) -> impl Iterator<Item = u64> + '_ {
        self.edges.iter().filter(|(_, e)| *e == Edge::Supersedes).map(|(id, _)| *id)
    }
}

/// A search result with its fused score.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub entry: Entry,
    pub score: f32,
}

/// The outcome of a write: a new row, or a merge into a near-duplicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upserted {
    pub id: u64,
    pub merged: bool,
    /// Existing entries this write marked as superseded.
    pub supersedes: Vec<u64>,
}

/// A candidate memory produced by an [`Extractor`].
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub text: String,
    pub tags: Vec<String>,
    pub confidence: f32,
    /// Ids of existing entries this draft contradicts.
    pub supersedes: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "trigger", content = "n", rename_all = "snake_case")]
pub enum Trigger {
    /// Consecutive user-turn embeddings diverged; extract from the previous topic.
    TopicChange,
    EveryNTurns(u32),
    SessionEnd,
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Trigger::TopicChange => f.write_str("topic change"),
            Trigger::EveryNTurns(n) => write!(f, "every {n} turns"),
            Trigger::SessionEnd => f.write_str("session end"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryError {
    Store(String),
    Embed(String),
    Judge(String),
    Extract(String),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::Store(d) => write!(f, "memory store: {d}"),
            MemoryError::Embed(d) => write!(f, "memory embedding: {d}"),
            MemoryError::Judge(d) => write!(f, "memory rerank: {d}"),
            MemoryError::Extract(d) => write!(f, "memory extraction: {d}"),
        }
    }
}

impl std::error::Error for MemoryError {}

// ───────────────────────────── traits ─────────────────────────────

/// Time source; tests inject a [`ManualClock`].
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A clock that only moves when told to.
#[derive(Clone, Debug, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn at(ms: u64) -> ManualClock {
        ManualClock(Arc::new(AtomicU64::new(ms)))
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn advance_s(&self, s: u64) {
        self.0.fetch_add(s * 1000, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

/// Listwise reranker (an LLM behind an `llm:<route>` service in production).
pub trait Judge: Send + Sync {
    /// The ids of `candidates` relevant to `query`, most relevant first; unknown ids are ignored.
    fn rerank<'a>(&'a self, query: &'a str, candidates: &'a [Entry]) -> BoxFuture<'a, Result<Vec<u64>, MemoryError>>;
}

/// Turns a conversation window into memory drafts. `existing` lets it skip known facts and
/// name contradicted ids.
pub trait Extractor: Send + Sync {
    fn extract<'a>(
        &'a self,
        trigger: Trigger,
        window: &'a [Message],
        existing: &'a [Entry],
    ) -> BoxFuture<'a, Result<Vec<Draft>, MemoryError>>;
}

pub trait Memory: Send + Sync {
    /// Gated retrieval for injection: judge-verified, carry-verified, or dynamic-gated, never
    /// unvetted fused output. Once per fresh user turn, one turn of lag, never blocks a model call.
    fn retrieve<'a>(&'a self, focused_query: &'a str, k: usize) -> BoxFuture<'a, Result<Vec<Entry>, MemoryError>>;
    /// Model-issued search: the fused hybrid ranking, top `k`, no gate (the caller judges).
    fn search<'a>(&'a self, query: &'a str, k: usize) -> BoxFuture<'a, Result<Vec<Hit>, MemoryError>>;
    /// Runs the extractor over `window` and writes its drafts (deduped, contradictions marked).
    fn extract<'a>(&'a self, trigger: Trigger, window: &'a [Message]) -> BoxFuture<'a, Result<Vec<Upserted>, MemoryError>>;
    /// Write-time dedupe: a same-model near-duplicate (cosine ≥ [`DEDUPE_COSINE`]) merges.
    fn upsert(&self, entry: Entry) -> BoxFuture<'_, Result<Upserted, MemoryError>>;
    /// Post-retrieval maintenance: link co-verified pairs, boost verified, decay rejected, prune.
    /// Returns the `Memory/*` entries the caller appends; every prune has one.
    fn maintain(&self) -> BoxFuture<'_, Result<Vec<EntryKind>, MemoryError>>;
    /// Registers a skill as a synthetic entry; hits on it become [`skill_suggestions`].
    fn add_synthetic<'a>(&'a self, skill: &'a str, description: &'a str) -> BoxFuture<'a, Result<u64, MemoryError>>;
    /// Takes the queued log entries `accept` admits, in order, and keeps the rest queued. The
    /// queue holds what background work produced (maintenance after an injected retrieval,
    /// extraction, skill activations, store errors). Tools forward the `Memory/*` customs
    /// through `value.events`; `Cap/Delta` activations wait for a caller allowed to append them.
    fn drain_events(&self, accept: &dyn Fn(&EntryKind) -> bool) -> Vec<EntryKind>;
}

#[derive(Clone)]
pub struct MemoryHandle(pub Arc<dyn Memory>);
impl Handle for MemoryHandle {
    const KIND: HandleKind = HandleKind::Custom("memory");
}

// ───────────────────────────── ranking rules ─────────────────────────────

/// Reciprocal rank fusion: `Σ 1 / (k + rank)` over the lists an id appears in (rank from 1;
/// repeats inside one list count once). Sorted by score, then best single rank, then id.
pub fn rrf(lists: &[&[u64]], k: f32) -> Vec<(u64, f32)> {
    let mut acc: BTreeMap<u64, (f32, usize)> = BTreeMap::new();
    for list in lists {
        let mut seen = BTreeSet::new();
        for id in list.iter() {
            if !seen.insert(*id) {
                continue;
            }
            let rank = seen.len();
            let e = acc.entry(*id).or_insert((0.0, usize::MAX));
            e.0 += 1.0 / (k + rank as f32);
            e.1 = e.1.min(rank);
        }
    }
    let mut out: Vec<(u64, f32, usize)> = acc.into_iter().map(|(id, (s, best))| (id, s, best)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
    out.into_iter().map(|(id, s, _)| (id, s)).collect()
}

/// Without a judge: keep top-1, then successors only while score ≥ 0.90×top and ≥ 0.95×prev.
/// Input order does not matter (sorted by score, stable); non-finite scores are dropped.
pub fn dynamic_gate(scored: &[(u64, f32)]) -> Vec<u64> {
    let mut s: Vec<(u64, f32)> = scored.iter().copied().filter(|(_, x)| x.is_finite()).collect();
    s.sort_by(|a, b| b.1.total_cmp(&a.1));
    let Some(&(first, top)) = s.first() else { return Vec::new() };
    let mut out = vec![first];
    let mut prev = top;
    for &(id, score) in &s[1..] {
        if score >= 0.90 * top && score >= 0.95 * prev {
            out.push(id);
            prev = score;
        } else {
            break;
        }
    }
    out
}

// ───────────────────────────── suppression ─────────────────────────────

/// Producer-side suppression rules.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Suppression {
    pub pending_stale_s: u64,
    pub identical_within_s: u64,
    pub overlap_ratio: f32,
    pub overlap_within_s: u64,
    pub injected_ttl_s: u64,
}

impl Default for Suppression {
    fn default() -> Self {
        Suppression { pending_stale_s: 120, identical_within_s: 90, overlap_ratio: 0.8, overlap_within_s: 180, injected_ttl_s: 45 * 60 }
    }
}

/// The reason a pending injection was dropped.
#[derive(Clone, Debug, PartialEq)]
pub enum Suppressed {
    Empty,
    /// Computed more than `pending_stale_s` ago.
    Stale {
        age_ms: u64,
    },
    /// Same payload injected within `identical_within_s`.
    Identical,
    /// At least `overlap_ratio` of the ids were injected within `overlap_within_s`.
    Overlap {
        ratio: f32,
    },
    /// Every id is still inside its injected TTL.
    AllRecentlyInjected,
}

#[derive(Clone, Debug)]
struct Injection {
    at: Timestamp,
    ids: BTreeSet<u64>,
    payloads: [Hash; 2],
}

/// Decides whether a computed retrieval may be injected, and records what was.
#[derive(Clone, Debug)]
pub struct Suppressor {
    cfg: Suppression,
    history: VecDeque<Injection>,
    injected: BTreeMap<u64, Timestamp>,
}

impl Suppressor {
    pub fn new(cfg: Suppression) -> Suppressor {
        Suppressor { cfg, history: VecDeque::new(), injected: BTreeMap::new() }
    }

    /// `ids` in rank order; `render` turns an id list into the payload text. On success returns
    /// the injected ids (those outside their TTL) and their rendered payload.
    pub fn admit(
        &mut self,
        computed_at: Timestamp,
        ids: &[u64],
        render: impl Fn(&[u64]) -> String,
        now: Timestamp,
    ) -> Result<(Vec<u64>, String), Suppressed> {
        let ms = |s: u64| s.saturating_mul(1000);
        if ids.is_empty() {
            return Err(Suppressed::Empty);
        }
        let age_ms = now.0.saturating_sub(computed_at.0);
        if age_ms > ms(self.cfg.pending_stale_s) {
            return Err(Suppressed::Stale { age_ms });
        }
        let keep_for = ms(self.cfg.identical_within_s.max(self.cfg.overlap_within_s));
        self.history.retain(|h| now.0.saturating_sub(h.at.0) < keep_for);
        let ttl = ms(self.cfg.injected_ttl_s);
        self.injected.retain(|_, at| now.0.saturating_sub(at.0) < ttl);

        let candidate = render(ids);
        let candidate_hash = Hash::of(candidate.as_bytes());
        let within = |h: &Injection, s: u64| now.0.saturating_sub(h.at.0) < ms(s);
        if self.history.iter().any(|h| within(h, self.cfg.identical_within_s) && h.payloads.contains(&candidate_hash)) {
            return Err(Suppressed::Identical);
        }
        let set: BTreeSet<u64> = ids.iter().copied().collect();
        for h in self.history.iter().filter(|h| within(h, self.cfg.overlap_within_s)) {
            let ratio = set.intersection(&h.ids).count() as f32 / set.len() as f32;
            if ratio >= self.cfg.overlap_ratio {
                return Err(Suppressed::Overlap { ratio });
            }
        }
        let kept: Vec<u64> = ids.iter().copied().filter(|id| !self.injected.contains_key(id)).collect();
        if kept.is_empty() {
            return Err(Suppressed::AllRecentlyInjected);
        }
        let text = if kept.len() == ids.len() { candidate } else { render(&kept) };
        self.history.push_back(Injection {
            at: now,
            ids: kept.iter().copied().collect(),
            payloads: [candidate_hash, Hash::of(text.as_bytes())],
        });
        for id in &kept {
            self.injected.insert(*id, now);
        }
        Ok((kept, text))
    }
}

// ───────────────────────────── skills ─────────────────────────────

/// `Cap/Delta{Activated, reason: SemanticMatch}` for every distinct skill among `hits`.
pub fn skill_suggestions(hits: &[Entry]) -> Vec<EntryKind> {
    let mut seen = BTreeSet::new();
    hits.iter()
        .filter_map(|e| e.skill.as_deref())
        .filter(|s| seen.insert(s.to_string()))
        .map(|s| EntryKind::CapDelta { id: s.to_string(), op: CapDeltaOp::Activated, reason: SEMANTIC_MATCH.to_string() })
        .collect()
}

// ───────────────────────────── log kinds ─────────────────────────────

/// Counters for one `maintain()` pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintainStats {
    pub linked: u64,
    pub boosted: u64,
    pub decayed: u64,
    pub pruned: u64,
}

impl MaintainStats {
    pub fn is_empty(&self) -> bool {
        *self == MaintainStats::default()
    }
}

fn custom(name: &str, body: Value) -> EntryKind {
    EntryKind::Custom { ns: NS.to_string(), name: name.to_string(), body }
}

/// `Memory/Pruned`: one per removed entry, with enough to audit why.
pub fn pruned_event(e: &Entry, now: Timestamp) -> EntryKind {
    custom(
        "Pruned",
        json!({
            "id": e.id,
            "scope": e.scope,
            "text": e.text,
            "confidence": e.confidence,
            "created_at": e.created_at.0,
            "age_ms": now.0.saturating_sub(e.created_at.0),
        }),
    )
}

pub fn maintained_event(s: &MaintainStats) -> EntryKind {
    custom("Maintained", json!(s))
}

pub fn extracted_event(trigger: Trigger, ids: &[u64]) -> EntryKind {
    custom("Extracted", json!({ "trigger": trigger, "ids": ids }))
}

pub fn extract_failed_event(trigger: Trigger, error: &str) -> EntryKind {
    custom("ExtractFailed", json!({ "trigger": trigger, "error": error }))
}

/// `Memory/Error`: background work (maintenance, retrieval, injection bookkeeping) failed.
pub fn error_event(during: &str, error: &str) -> EntryKind {
    custom("Error", json!({ "during": during, "error": error }))
}

/// The `Memory` projection: pruned ids, maintenance totals, extraction counters. A `Pruned`
/// that breaks the prune rule (confidence ≥ [`PRUNE_BELOW`], younger than 24 h, pruned twice)
/// fails the fold; `aho session verify` audits prunes through this.
pub fn fold(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut s = prev.cloned().unwrap_or_else(
        || json!({ "pruned": [], "maintained": MaintainStats::default(), "extracted": 0, "extract_failures": 0, "errors": 0 }),
    );
    let u64_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).ok_or_else(|| format!("`{k}` must be a non-negative integer"));
    match name {
        "Pruned" => {
            let id = u64_of(body, "id")?;
            let conf = body.get("confidence").and_then(Value::as_f64).ok_or("`confidence` must be a number")?;
            let age = u64_of(body, "age_ms")?;
            if conf >= f64::from(PRUNE_BELOW) {
                return Err(format!("entry {id} pruned at confidence {conf:.3}, not below {PRUNE_BELOW}"));
            }
            if age < PRUNE_AFTER_MS {
                return Err(format!("entry {id} pruned at age {age} ms, younger than {PRUNE_AFTER_MS} ms"));
            }
            let list = s["pruned"].as_array_mut().ok_or("projection `pruned` is not a list")?;
            if list.iter().any(|v| v.as_u64() == Some(id)) {
                return Err(format!("entry {id} pruned twice"));
            }
            list.push(json!(id));
        }
        "Maintained" => {
            let add: MaintainStats = serde_json::from_value(body.clone()).map_err(|e| format!("bad Maintained body: {e}"))?;
            let cur: MaintainStats = serde_json::from_value(s["maintained"].clone()).map_err(|e| format!("bad projection: {e}"))?;
            s["maintained"] = json!(MaintainStats {
                linked: cur.linked + add.linked,
                boosted: cur.boosted + add.boosted,
                decayed: cur.decayed + add.decayed,
                pruned: cur.pruned + add.pruned,
            });
        }
        "Extracted" => {
            serde_json::from_value::<Trigger>(body["trigger"].clone()).map_err(|e| format!("bad trigger: {e}"))?;
            let ids = body["ids"].as_array().ok_or("`ids` must be a list")?;
            if ids.iter().any(|v| v.as_u64().is_none()) {
                return Err("`ids` must hold entry ids".into());
            }
            s["extracted"] = json!(s["extracted"].as_u64().unwrap_or(0) + ids.len() as u64);
        }
        "ExtractFailed" => {
            serde_json::from_value::<Trigger>(body["trigger"].clone()).map_err(|e| format!("bad trigger: {e}"))?;
            body["error"].as_str().ok_or("`error` must be text")?;
            s["extract_failures"] = json!(s["extract_failures"].as_u64().unwrap_or(0) + 1);
        }
        "Error" => {
            body["during"].as_str().ok_or("`during` must be text")?;
            body["error"].as_str().ok_or("`error` must be text")?;
            s["errors"] = json!(s["errors"].as_u64().unwrap_or(0) + 1);
        }
        other => return Err(format!("unknown Memory event `{other}`")),
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1000;

    fn render(ids: &[u64]) -> String {
        ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(",")
    }

    #[test]
    fn rrf_rewards_agreement_between_lists() {
        let dense = [1, 2, 3];
        let bm25 = [3, 4, 1];
        let f = rrf(&[&dense, &bm25], RRF_K);
        let ids: Vec<u64> = f.iter().map(|x| x.0).collect();
        // 1: 1/61+1/63, 3: 1/63+1/61 (tie; best rank 1 for both → id order), 2: 1/62, 4: 1/62
        assert_eq!(ids, vec![1, 3, 2, 4]);
        assert!((f[0].1 - (1.0 / 61.0 + 1.0 / 63.0)).abs() < 1e-7);
        assert!(f[1].1 > f[2].1 * 1.9, "two-list hits beat one-list hits");
    }

    #[test]
    fn rrf_counts_repeats_once_and_handles_empty_lists() {
        let f = rrf(&[&[7, 7, 8], &[]], RRF_K);
        assert_eq!(f, vec![(7, 1.0 / 61.0), (8, 1.0 / 62.0)]);
        assert!(rrf(&[], RRF_K).is_empty());
    }

    #[test]
    fn dynamic_gate_keeps_top_then_stops_at_first_break() {
        assert!(dynamic_gate(&[]).is_empty());
        assert_eq!(dynamic_gate(&[(9, 0.01)]), vec![9], "top-1 always kept");
        // top rule: 0.96, 0.92, 0.90 are all ≥ 0.9·1.0 and each ≥ 0.95·prev; 0.85 < 0.9·top stops the walk
        let s = [(1, 1.0), (2, 0.96), (3, 0.92), (4, 0.90), (5, 0.85)];
        assert_eq!(dynamic_gate(&s), vec![1, 2, 3, 4]);
        // prev rule: 0.99 then 0.93 (< 0.95·0.99 = 0.9405) stops even though ≥ 0.9·top
        assert_eq!(dynamic_gate(&[(1, 1.0), (2, 0.99), (3, 0.93)]), vec![1, 2]);
        // unsorted input and NaN
        assert_eq!(dynamic_gate(&[(2, 0.95), (3, f32::NAN), (1, 1.0)]), vec![1, 2]);
    }

    #[test]
    fn suppression_stale_pending_is_discarded() {
        let mut s = Suppressor::new(Suppression::default());
        assert_eq!(s.admit(Timestamp(0), &[1], render, Timestamp(121 * S)), Err(Suppressed::Stale { age_ms: 121 * S }));
        assert!(s.admit(Timestamp(0), &[1], render, Timestamp(120 * S)).is_ok(), "exactly 120 s is not older than 120 s");
    }

    #[test]
    fn suppression_windows_in_order_identical_overlap_ttl() {
        let mut s = Suppressor::new(Suppression::default());
        let at = |t: u64| Timestamp(t * S);
        assert_eq!(s.admit(at(0), &[1, 2], render, at(0)), Ok((vec![1, 2], "#1,#2".into())));
        assert_eq!(s.admit(at(60), &[1, 2], render, at(60)), Err(Suppressed::Identical));
        // past 90 s the payload may repeat, but 100 % id overlap within 180 s still blocks it
        assert_eq!(s.admit(at(100), &[1, 2], render, at(100)), Err(Suppressed::Overlap { ratio: 1.0 }));
        // past 180 s the only thing left is the per-id 45-min TTL
        assert_eq!(s.admit(at(200), &[1, 2], render, at(200)), Err(Suppressed::AllRecentlyInjected));
        assert_eq!(s.admit(at(2700), &[1, 2], render, at(2700)), Ok((vec![1, 2], "#1,#2".into())), "TTL expires at 45 min");
    }

    #[test]
    fn suppression_overlap_threshold_and_ttl_filtering() {
        let mut s = Suppressor::new(Suppression::default());
        let at = |t: u64| Timestamp(t * S);
        s.admit(at(0), &[1, 2, 3, 4, 5], render, at(0)).unwrap();
        assert_eq!(s.admit(at(10), &[1, 2, 3, 4, 9], render, at(10)), Err(Suppressed::Overlap { ratio: 0.8 }));
        // 3/5 overlap passes; ids still inside their TTL are filtered out of the payload
        assert_eq!(s.admit(at(10), &[1, 2, 3, 9, 10], render, at(10)), Ok((vec![9, 10], "#9,#10".into())));
        assert_eq!(s.admit(at(10), &[], render, at(10)), Err(Suppressed::Empty));
    }

    #[test]
    fn skill_hits_become_semantic_match_activations_once_each() {
        let mut a = Entry::new("Deploys to fly.io", vec![]);
        a.skill = Some("deploy".into());
        let b = a.clone();
        let plain = Entry::new("unrelated", vec![]);
        let ev = skill_suggestions(&[a, plain, b]);
        assert_eq!(ev, vec![EntryKind::CapDelta { id: "deploy".into(), op: CapDeltaOp::Activated, reason: "SemanticMatch".into() }]);
    }

    fn body(k: &EntryKind) -> (&str, &Value) {
        match k {
            EntryKind::Custom { ns, name, body } if ns == NS => (name.as_str(), body),
            other => panic!("not a Memory entry: {other:?}"),
        }
    }

    #[test]
    fn fold_accepts_valid_prunes_and_rejects_rule_breaks() {
        let mut e = Entry::new("old", vec![]);
        e.id = 4;
        e.confidence = 0.1;
        e.created_at = Timestamp(0);
        let now = Timestamp(PRUNE_AFTER_MS + 1);
        let ev = pruned_event(&e, now);
        let (name, b) = body(&ev);
        let s = fold(None, name, b).unwrap();
        assert_eq!(s["pruned"], json!([4]));
        assert!(fold(Some(&s), name, b).unwrap_err().contains("pruned twice"));

        e.confidence = 0.2;
        let over = pruned_event(&e, now);
        let (n, b) = body(&over);
        assert!(fold(None, n, b).unwrap_err().contains("not below"));
        e.confidence = 0.1;
        let young = pruned_event(&e, Timestamp(1000));
        let (n, b) = body(&young);
        assert!(fold(None, n, b).unwrap_err().contains("younger"));
        assert!(fold(None, "Bogus", &json!({})).is_err());
    }

    #[test]
    fn fold_accumulates_maintenance_and_extraction() {
        let m = maintained_event(&MaintainStats { linked: 1, boosted: 2, decayed: 3, pruned: 0 });
        let (n, b) = body(&m);
        let s = fold(None, n, b).unwrap();
        let s = fold(Some(&s), n, b).unwrap();
        assert_eq!(s["maintained"], json!({"linked": 2, "boosted": 4, "decayed": 6, "pruned": 0}));
        let x = extracted_event(Trigger::EveryNTurns(12), &[1, 2]);
        let (n, b) = body(&x);
        let s = fold(Some(&s), n, b).unwrap();
        assert_eq!(s["extracted"], json!(2));
        let f = extract_failed_event(Trigger::SessionEnd, "route down");
        let (n, b) = body(&f);
        assert_eq!(fold(Some(&s), n, b).unwrap()["extract_failures"], json!(1));
        assert!(fold(None, "Extracted", &json!({"trigger": {"trigger": "nope"}, "ids": []})).is_err());
    }

    #[test]
    fn entries_round_trip_and_skill_is_not_serialized_when_absent() {
        let e = Entry::new("uses nextest", vec!["build".into()]).with_embedding(Vector { model: ModelId::new("m"), dims: vec![1.0] });
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("skill").is_none());
        assert_eq!(v["embedding_model"], json!("m"));
        assert_eq!(serde_json::from_value::<Entry>(v).unwrap(), e);
    }
}
