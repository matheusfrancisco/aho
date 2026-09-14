//! [`LocalMemory`]: the store plus retrieval, write-time dedupe/contradiction, maintenance,
//! synthetic skill entries, and extraction. Locks are never held across an `.await`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use aho_embed::{Embed, Vector, similarity};
use aho_log::EntryKind;
use aho_memory::*;
use aho_types::{BoxFuture, Message, Timestamp};

use crate::bm25;
use crate::llm::render_window;
use crate::rules::RuleExtractor;
use crate::store::{SKILL_BASE, Store, StorePaths, scope_of};

/// Dense hits below this cosine are noise for fusion (the lexical embedder shares letter
/// trigrams between unrelated English texts).
pub const DENSE_FLOOR: f32 = 0.2;
/// Consecutive user-turn embeddings below this cosine mark a topic change.
pub const TOPIC_COSINE: f32 = 0.3;
const BOOST: f32 = 0.05;
const DECAY: f32 = 0.85;
/// A merged duplicate is a second sighting of the same fact.
const REINFORCE: f32 = 0.05;
/// Co-verified this many times → `RelatesTo` both ways.
const LINK_AFTER: u32 = 2;
const CONTRADICTION_MIN_COSINE: f32 = 0.5;
const SUPERSEDED_FACTOR: f32 = 0.5;
/// Existing entries shown to an extractor for dedup.
const EXISTING_FOR_EXTRACT: usize = 30;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Max entries injected per turn.
    pub k: usize,
    /// Fusion/rerank candidate pool (never below [`MIN_POOL`]).
    pub pool: usize,
    /// With a judge: rerank at most every this many retrievals (always on the first and on a topic change).
    pub rerank_every: u64,
    /// Rerank ballots per retrieval; ids chosen by a majority survive.
    pub votes: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { k: 5, pool: MIN_POOL, rerank_every: 3, votes: 1 }
    }
}

#[derive(Default)]
struct Recall {
    count: u64,
    last_rerank: Option<u64>,
    last_query: Option<Vector>,
    /// The last judge-verified set; the only ids a degraded retrieval may return.
    verified: BTreeSet<u64>,
}

#[derive(Default)]
struct Feedback {
    verified: BTreeSet<u64>,
    rejected: BTreeSet<u64>,
    co: BTreeMap<(u64, u64), u32>,
}

struct Inner {
    project: Store,
    global: Store,
    skills: Vec<Entry>,
    recall: Recall,
    feedback: Feedback,
}

impl Inner {
    fn stored(&self) -> impl Iterator<Item = &Entry> {
        self.project.entries.iter().chain(&self.global.entries)
    }
    fn all(&self) -> impl Iterator<Item = &Entry> {
        self.stored().chain(&self.skills)
    }
    fn store(&self, s: Scope) -> &Store {
        match s {
            Scope::Project => &self.project,
            Scope::Global => &self.global,
        }
    }
    fn store_mut(&mut self, s: Scope) -> &mut Store {
        match s {
            Scope::Project => &mut self.project,
            Scope::Global => &mut self.global,
        }
    }
    fn get(&self, id: u64) -> Option<&Entry> {
        match scope_of(id) {
            Some(s) => self.store(s).entries.iter().find(|e| e.id == id),
            None => self.skills.iter().find(|e| e.id == id),
        }
    }
    /// A persisted entry, marking its store dirty.
    fn stored_mut(&mut self, id: u64) -> Option<&mut Entry> {
        let store = self.store_mut(scope_of(id)?);
        let i = store.entries.iter().position(|e| e.id == id)?;
        store.dirty = true;
        Some(&mut store.entries[i])
    }
    fn flush(&mut self) -> Result<(), MemoryError> {
        let a = self.project.flush();
        let b = self.global.flush();
        a.and(b)
    }
}

const NEGATIONS: &[&str] = &[
    "not", "no", "never", "cannot", "without", "dont", "doesnt", "didnt", "isnt", "arent", "wasnt", "werent", "wont", "cant", "shouldnt",
    "mustnt", "neither", "nor",
];

fn plain_words(t: &str) -> Vec<String> {
    t.to_lowercase().replace(['\'', '’'], "").split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

fn negated(t: &str) -> bool {
    plain_words(t).iter().any(|w| NEGATIONS.contains(&w.as_str()))
}

fn content_terms(t: &str) -> BTreeSet<String> {
    plain_words(t)
        .into_iter()
        .filter(|w| !bm25::is_stopword(w) && !NEGATIONS.contains(&w.as_str()) && w != "longer")
        .map(|w| if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") { w[..w.len() - 1].to_string() } else { w })
        .collect()
}

/// Opposite polarity over (nearly) the same content: "uses tabs" vs "does not use tabs".
pub fn contradicts(a: &str, b: &str) -> bool {
    if negated(a) == negated(b) {
        return false;
    }
    let (ca, cb) = (content_terms(a), content_terms(b));
    let smaller = ca.len().min(cb.len());
    smaller >= 2 && ca.intersection(&cb).count() * 5 >= smaller * 4
}

fn doc_terms(e: &Entry) -> Vec<String> {
    let mut t = bm25::terms(&e.text);
    for tag in &e.tags {
        t.extend(bm25::terms(tag));
    }
    t
}

pub struct LocalMemory {
    embed: Arc<dyn Embed>,
    judge: Option<Arc<dyn Judge>>,
    extractors: Vec<Arc<dyn Extractor>>,
    clock: Arc<dyn Clock>,
    settings: Settings,
    inner: Mutex<Inner>,
    outbox: Mutex<Vec<EntryKind>>,
}

impl LocalMemory {
    /// Opens (or starts empty) both store files. The rule-based extractor is always on.
    pub fn open(
        paths: &StorePaths,
        project: &str,
        embed: Arc<dyn Embed>,
        clock: Arc<dyn Clock>,
        settings: Settings,
    ) -> Result<LocalMemory, MemoryError> {
        let inner = Inner {
            project: Store::open(paths.project.clone(), Scope::Project, Some(project.to_string()))?,
            global: Store::open(paths.global.clone(), Scope::Global, None)?,
            skills: Vec::new(),
            recall: Recall::default(),
            feedback: Feedback::default(),
        };
        let settings = Settings {
            pool: settings.pool.max(MIN_POOL),
            votes: settings.votes.max(1),
            rerank_every: settings.rerank_every.max(1),
            ..settings
        };
        Ok(LocalMemory {
            embed,
            judge: None,
            extractors: vec![Arc::new(RuleExtractor)],
            clock,
            settings,
            inner: Mutex::new(inner),
            outbox: Mutex::new(Vec::new()),
        })
    }

    pub fn with_judge(mut self, judge: Arc<dyn Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    pub fn with_extractor(mut self, x: Arc<dyn Extractor>) -> Self {
        self.extractors.push(x);
        self
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn has_judge(&self) -> bool {
        self.judge.is_some()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queues log entries for the session (`Memory::drain_events`).
    pub fn emit(&self, events: impl IntoIterator<Item = EntryKind>) {
        self.outbox.lock().unwrap_or_else(|p| p.into_inner()).extend(events);
    }

    /// Every persisted entry (both scopes), store order.
    pub fn entries(&self) -> Vec<Entry> {
        self.lock().stored().cloned().collect()
    }

    pub fn get(&self, id: u64) -> Option<Entry> {
        self.lock().get(id).cloned()
    }

    pub async fn embed_one(&self, text: &str) -> Result<Vector, MemoryError> {
        let mut v = self.embed.embed(&[text.to_string()]).await.map_err(|e| MemoryError::Embed(e.to_string()))?;
        v.pop().ok_or_else(|| MemoryError::Embed("the embedder returned no vector".into()))
    }

    /// Dense (same model, above [`DENSE_FLOOR`]) and BM25 rankings fused by RRF, cut to the pool.
    fn fused(&self, inner: &Inner, query: &str, qvec: Option<&Vector>, with_skills: bool) -> Vec<(u64, f32)> {
        let pool = self.settings.pool;
        let docs: Vec<&Entry> = if with_skills { inner.all().collect() } else { inner.stored().collect() };
        let mut dense: Vec<(u64, f32)> = match qvec {
            Some(q) => docs
                .iter()
                .filter_map(|e| e.embedding.as_ref().and_then(|v| similarity(q, v)).map(|s| (e.id, s)))
                .filter(|(_, s)| *s >= DENSE_FLOOR)
                .collect(),
            None => Vec::new(),
        };
        dense.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        dense.truncate(pool);
        let tokenized: Vec<(u64, Vec<String>)> = docs.iter().map(|e| (e.id, doc_terms(e))).collect();
        let mut lexical = bm25::rank(query, &tokenized);
        lexical.truncate(pool);
        let d: Vec<u64> = dense.iter().map(|x| x.0).collect();
        let l: Vec<u64> = lexical.iter().map(|x| x.0).collect();
        let mut fused = rrf(&[&d, &l], RRF_K);
        fused.truncate(pool);
        fused
    }

    async fn consensus(&self, judge: &dyn Judge, query: &str, candidates: &[Entry]) -> Result<Vec<u64>, MemoryError> {
        let mut ballots: Vec<Vec<u64>> = Vec::new();
        let mut last_err = None;
        for _ in 0..self.settings.votes {
            match judge.rerank(query, candidates).await {
                Ok(b) => ballots.push(b),
                Err(e) => last_err = Some(e),
            }
        }
        if ballots.is_empty() {
            return Err(last_err.unwrap_or_else(|| MemoryError::Judge("no rerank ballot was cast".into())));
        }
        let need = ballots.len() / 2 + 1;
        let mut tally: BTreeMap<u64, (usize, usize, usize)> = BTreeMap::new(); // votes, rank sum, first seen
        let mut order = 0;
        for b in &ballots {
            let mut seen = BTreeSet::new();
            for (pos, id) in b.iter().enumerate() {
                if seen.insert(*id) {
                    let t = tally.entry(*id).or_insert_with(|| {
                        order += 1;
                        (0, 0, order)
                    });
                    t.0 += 1;
                    t.1 += pos;
                }
            }
        }
        let mut kept: Vec<(u64, (usize, usize, usize))> = tally.into_iter().filter(|(_, t)| t.0 >= need).collect();
        kept.sort_by(|a, b| b.1.0.cmp(&a.1.0).then((a.1.1 * b.1.0).cmp(&(b.1.1 * a.1.0))).then(a.1.2.cmp(&b.1.2)));
        Ok(kept.into_iter().map(|(id, _)| id).collect())
    }

    /// The gated retrieval behind [`Memory::retrieve`], with the query already embedded.
    pub async fn retrieve_with(&self, query: &str, qvec: Option<Vector>, k: usize) -> Result<Vec<Entry>, MemoryError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let (fused, candidates, due) = {
            let mut inner = self.lock();
            let fused = self.fused(&inner, query, qvec.as_ref(), true);
            let topic_changed = match (&inner.recall.last_query, &qvec) {
                (Some(a), Some(b)) => similarity(a, b).is_some_and(|s| s < TOPIC_COSINE),
                _ => false,
            };
            inner.recall.count += 1;
            if qvec.is_some() {
                inner.recall.last_query = qvec.clone();
            }
            let count = inner.recall.count;
            let due = self.judge.is_some()
                && (topic_changed || inner.recall.last_rerank.is_none_or(|last| count - last >= self.settings.rerank_every));
            let candidates: Vec<Entry> = fused.iter().filter_map(|(id, _)| inner.get(*id).cloned()).collect();
            (fused, candidates, due)
        };
        if fused.is_empty() {
            return Ok(Vec::new());
        }
        let carry = |verified: &BTreeSet<u64>| fused.iter().map(|x| x.0).filter(|id| verified.contains(id)).collect::<Vec<u64>>();
        let mut chosen = match (&self.judge, due) {
            (None, _) => dynamic_gate(&fused),
            (Some(judge), true) => match self.consensus(judge.as_ref(), query, &candidates).await {
                Ok(verified) => {
                    let mut inner = self.lock();
                    let top: Vec<u64> = fused.iter().take(k).map(|x| x.0).collect();
                    inner.feedback.rejected.extend(top.into_iter().filter(|id| !verified.contains(id)));
                    inner.feedback.verified.extend(verified.iter().copied());
                    inner.recall.verified = verified.iter().copied().collect();
                    inner.recall.last_rerank = Some(inner.recall.count);
                    verified
                }
                Err(e) => {
                    self.emit([error_event("rerank", &format!("{e}; injected only previously verified memories"))]);
                    carry(&self.lock().recall.verified)
                }
            },
            (Some(_), false) => carry(&self.lock().recall.verified),
        };
        chosen.truncate(k);
        let mut inner = self.lock();
        let stored: Vec<u64> = chosen.iter().copied().filter(|id| scope_of(*id).is_some()).collect();
        for (i, a) in stored.iter().enumerate() {
            for b in &stored[i + 1..] {
                *inner.feedback.co.entry(((*a).min(*b), (*a).max(*b))).or_insert(0) += 1;
            }
        }
        Ok(chosen.iter().filter_map(|id| inner.get(*id).cloned()).collect())
    }

    /// The model-facing ranking: fused hybrid scores, persisted entries only, top `k`.
    pub async fn search_hits(&self, query: &str, k: usize) -> Vec<Hit> {
        let qvec = self.embed_one(query).await.ok();
        let inner = self.lock();
        self.fused(&inner, query, qvec.as_ref(), false)
            .into_iter()
            .take(k)
            .filter_map(|(id, score)| inner.get(id).map(|e| Hit { entry: e.clone(), score }))
            .collect()
    }

    /// Write with explicit contradiction ids (from an extractor) on top of detected ones.
    pub async fn write(&self, mut entry: Entry, explicit_supersedes: &[u64]) -> Result<Upserted, MemoryError> {
        entry.text = entry.text.trim().to_string();
        if entry.text.is_empty() {
            return Err(MemoryError::Store("refusing to store an empty memory; pass the fact to remember as `text`".into()));
        }
        let model = self.embed.model();
        if entry.embedding.as_ref().map(|v| &v.model) != Some(&model) {
            entry.embedding = None;
            entry.embedding_model = None;
            // An embedder failure still stores the entry: BM25 keeps it reachable.
            if let Ok(v) = self.embed_one(&entry.text).await {
                entry = entry.with_embedding(v);
            }
        }
        entry.confidence = entry.confidence.clamp(0.0, 1.0);
        entry.skill = None;
        let mut tags: Vec<String> = Vec::new();
        for t in entry.tags.drain(..).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
        entry.tags = tags;
        let now = self.clock.now();

        let mut inner = self.lock();
        let mut contradicted: BTreeSet<u64> = BTreeSet::new();
        let mut duplicate: Option<(u64, f32)> = None;
        if let Some(v) = &entry.embedding {
            for e in &inner.store(entry.scope).entries {
                let Some(s) = e.embedding.as_ref().and_then(|w| similarity(v, w)) else { continue };
                if s >= CONTRADICTION_MIN_COSINE && contradicts(&entry.text, &e.text) {
                    contradicted.insert(e.id);
                } else if s >= DEDUPE_COSINE && duplicate.is_none_or(|(_, best)| s > best) {
                    duplicate = Some((e.id, s));
                }
            }
        }
        contradicted.extend(explicit_supersedes.iter().copied().filter(|id| scope_of(*id).is_some() && inner.get(*id).is_some()));
        if let Some((id, _)) = duplicate {
            contradicted.remove(&id);
        }
        for id in &contradicted {
            if let Some(e) = inner.stored_mut(*id) {
                e.confidence *= SUPERSEDED_FACTOR;
            }
        }
        let supersedes: Vec<u64> = contradicted.into_iter().collect();
        let (id, merged) = match duplicate {
            Some((id, _)) => {
                let target = inner.stored_mut(id).ok_or_else(|| MemoryError::Store(format!("entry {id} vanished during a merge")))?;
                for t in entry.tags {
                    if !target.tags.contains(&t) {
                        target.tags.push(t);
                    }
                }
                target.confidence = (target.confidence.max(entry.confidence) + REINFORCE).min(1.0);
                let edges = supersedes.iter().map(|s| (*s, Edge::Supersedes)).chain(entry.edges);
                for edge in edges {
                    if edge.0 != id && !target.edges.contains(&edge) {
                        target.edges.push(edge);
                    }
                }
                (id, true)
            }
            None => {
                let store = inner.store_mut(entry.scope);
                entry.id = store.alloc();
                entry.created_at = now;
                entry.injected_at = None;
                for s in &supersedes {
                    if !entry.edges.contains(&(*s, Edge::Supersedes)) {
                        entry.edges.push((*s, Edge::Supersedes));
                    }
                }
                let id = entry.id;
                store.entries.push(entry);
                store.dirty = true;
                (id, false)
            }
        };
        inner.flush()?;
        Ok(Upserted { id, merged, supersedes })
    }

    /// Records that `ids` were injected at `at` (persisted as `injected_at`).
    pub fn mark_injected(&self, ids: &[u64], at: Timestamp) -> Result<(), MemoryError> {
        let mut inner = self.lock();
        for id in ids {
            if let Some(e) = inner.stored_mut(*id) {
                e.injected_at = Some(at);
            }
        }
        inner.flush()
    }

    /// Boost verified, decay rejected, link co-verified pairs, prune; one `Memory/Pruned` per prune.
    pub fn maintain_now(&self) -> Result<Vec<EntryKind>, MemoryError> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let verified = std::mem::take(&mut inner.feedback.verified);
        let rejected = std::mem::take(&mut inner.feedback.rejected);
        let mut stats = MaintainStats::default();
        for id in &verified {
            if let Some(e) = inner.stored_mut(*id) {
                e.confidence = (e.confidence + BOOST).min(1.0);
                stats.boosted += 1;
            }
        }
        for id in rejected.difference(&verified) {
            if let Some(e) = inner.stored_mut(*id) {
                e.confidence *= DECAY;
                stats.decayed += 1;
            }
        }
        let ready: Vec<(u64, u64)> = inner.feedback.co.iter().filter(|(_, n)| **n >= LINK_AFTER).map(|(p, _)| *p).collect();
        for (a, b) in ready {
            inner.feedback.co.remove(&(a, b));
            let mut linked = false;
            for (from, to) in [(a, b), (b, a)] {
                if inner.get(to).is_none() {
                    continue;
                }
                if let Some(e) = inner.stored_mut(from)
                    && !e.edges.iter().any(|(t, _)| *t == to)
                {
                    e.edges.push((to, Edge::RelatesTo));
                    linked = true;
                }
            }
            stats.linked += u64::from(linked);
        }
        let mut events = Vec::new();
        let mut gone = BTreeSet::new();
        for scope in [Scope::Project, Scope::Global] {
            let store = inner.store_mut(scope);
            let before = store.entries.len();
            store.entries.retain(|e| {
                let prune = e.confidence < PRUNE_BELOW && now.0.saturating_sub(e.created_at.0) > PRUNE_AFTER_MS;
                if prune {
                    events.push(pruned_event(e, now));
                    gone.insert(e.id);
                }
                !prune
            });
            if store.entries.len() != before {
                store.dirty = true;
            }
        }
        if !gone.is_empty() {
            stats.pruned = gone.len() as u64;
            for scope in [Scope::Project, Scope::Global] {
                let store = inner.store_mut(scope);
                for e in &mut store.entries {
                    let n = e.edges.len();
                    e.edges.retain(|(t, _)| !gone.contains(t));
                    if e.edges.len() != n {
                        store.dirty = true;
                    }
                }
            }
            inner.recall.verified.retain(|id| !gone.contains(id));
            inner.feedback.co.retain(|(a, b), _| !gone.contains(a) && !gone.contains(b));
        }
        inner.flush()?;
        if !stats.is_empty() {
            events.push(maintained_event(&stats));
        }
        Ok(events)
    }

    pub async fn add_synthetic_entry(&self, skill: &str, description: &str) -> Result<u64, MemoryError> {
        let skill = skill.trim();
        if skill.is_empty() {
            return Err(MemoryError::Store("a synthetic entry needs a skill name".into()));
        }
        let text = format!("{skill}: {}", description.trim());
        let v = self.embed_one(&text).await.ok();
        let now = self.clock.now();
        let mut inner = self.lock();
        if let Some(e) = inner.skills.iter_mut().find(|e| e.skill.as_deref() == Some(skill)) {
            e.text = text;
            e.embedding_model = v.as_ref().map(|v| v.model.clone());
            e.embedding = v;
            return Ok(e.id);
        }
        let mut e = Entry::new(text, vec!["skill".into()]);
        e.id = SKILL_BASE + inner.skills.len() as u64 + 1;
        e.confidence = 1.0;
        e.created_at = now;
        e.skill = Some(skill.to_string());
        if let Some(v) = v {
            e = e.with_embedding(v);
        }
        let id = e.id;
        inner.skills.push(e);
        Ok(id)
    }

    /// Runs every extractor over `window` and writes the drafts. Returns the writes (an error
    /// only when every extractor failed and nothing was written) plus the `Memory/*` entries.
    pub async fn extract_logged(&self, trigger: Trigger, window: &[Message]) -> (Result<Vec<Upserted>, MemoryError>, Vec<EntryKind>) {
        let existing: Vec<Entry> =
            self.search_hits(&render_window(window), EXISTING_FOR_EXTRACT).await.into_iter().map(|h| h.entry).collect();
        let mut events = Vec::new();
        let mut drafts = Vec::new();
        let mut failed = 0;
        let mut last_err = None;
        for x in &self.extractors {
            match x.extract(trigger, window, &existing).await {
                Ok(d) => drafts.extend(d),
                Err(e) => {
                    failed += 1;
                    events.push(extract_failed_event(trigger, &e.to_string()));
                    last_err = Some(e);
                }
            }
        }
        let mut written = Vec::new();
        for d in drafts {
            let mut e = Entry::new(d.text, d.tags);
            e.confidence = d.confidence;
            match self.write(e, &d.supersedes).await {
                Ok(u) => written.push(u),
                Err(err) => {
                    events.push(extract_failed_event(trigger, &err.to_string()));
                    last_err = Some(err);
                }
            }
        }
        if !written.is_empty() {
            events.push(extracted_event(trigger, &written.iter().map(|u| u.id).collect::<Vec<_>>()));
        }
        let result = match last_err {
            Some(e) if written.is_empty() && failed > 0 => Err(e),
            _ => Ok(written),
        };
        (result, events)
    }
}

impl Memory for LocalMemory {
    fn retrieve<'a>(&'a self, focused_query: &'a str, k: usize) -> BoxFuture<'a, Result<Vec<Entry>, MemoryError>> {
        Box::pin(async move {
            let qvec = self.embed_one(focused_query).await.ok();
            self.retrieve_with(focused_query, qvec, k).await
        })
    }

    fn search<'a>(&'a self, query: &'a str, k: usize) -> BoxFuture<'a, Result<Vec<Hit>, MemoryError>> {
        Box::pin(async move { Ok(self.search_hits(query, k).await) })
    }

    fn extract<'a>(&'a self, trigger: Trigger, window: &'a [Message]) -> BoxFuture<'a, Result<Vec<Upserted>, MemoryError>> {
        Box::pin(async move {
            let (result, events) = self.extract_logged(trigger, window).await;
            self.emit(events);
            result
        })
    }

    fn upsert(&self, entry: Entry) -> BoxFuture<'_, Result<Upserted, MemoryError>> {
        Box::pin(async move { self.write(entry, &[]).await })
    }

    fn maintain(&self) -> BoxFuture<'_, Result<Vec<EntryKind>, MemoryError>> {
        let r = self.maintain_now();
        Box::pin(async move { r })
    }

    fn add_synthetic<'a>(&'a self, skill: &'a str, description: &'a str) -> BoxFuture<'a, Result<u64, MemoryError>> {
        Box::pin(self.add_synthetic_entry(skill, description))
    }

    fn drain_events(&self, accept: &dyn Fn(&EntryKind) -> bool) -> Vec<EntryKind> {
        let mut q = self.outbox.lock().unwrap_or_else(|p| p.into_inner());
        let (taken, kept): (Vec<EntryKind>, Vec<EntryKind>) = std::mem::take(&mut *q).into_iter().partition(|e| accept(e));
        *q = kept;
        taken
    }
}
