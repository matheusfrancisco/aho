#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aho_cap::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_embed::{ModelId, Vector};
use aho_log::{CapDeltaOp, CapState, EntryKind, Item, State};
use aho_memory::*;
use aho_types::{BoxFuture, Consequence, ConsequenceSet, ContentBlock, EntryId, Form, Timestamp};
use embed_local::{EmbedLocal, HashNgram, hash_ngram_vector};
use memory_local::*;

const H: u64 = 60 * 60;
const T0: u64 = 1_000_000_000;
const PROJECT: &str = "/work/proj";

fn open_with(dir: &Path, clock: &ManualClock, settings: Settings) -> LocalMemory {
    LocalMemory::open(&StorePaths::new(dir, PROJECT), PROJECT, Arc::new(HashNgram::new(64)), Arc::new(clock.clone()), settings).unwrap()
}

fn open(dir: &Path, clock: &ManualClock) -> LocalMemory {
    open_with(dir, clock, Settings::default())
}

fn save(m: &LocalMemory, text: &str) -> Upserted {
    block(m.upsert(Entry::new(text, vec![]))).unwrap()
}

fn block<T>(f: BoxFuture<'_, T>) -> T {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
}

/// Stands in for the rerank model: answers from a script, repeating the last line.
struct ScriptedJudge {
    script: Mutex<Vec<Result<Vec<u64>, MemoryError>>>,
    calls: AtomicUsize,
}

impl ScriptedJudge {
    fn new(script: Vec<Result<Vec<u64>, MemoryError>>) -> Arc<ScriptedJudge> {
        Arc::new(ScriptedJudge { script: Mutex::new(script), calls: AtomicUsize::new(0) })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Judge for ScriptedJudge {
    fn rerank<'a>(&'a self, _q: &'a str, _c: &'a [Entry]) -> BoxFuture<'a, Result<Vec<u64>, MemoryError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut s = self.script.lock().unwrap_or_else(|p| p.into_inner());
        let r = if s.len() > 1 { s.remove(0) } else { s[0].clone() };
        Box::pin(async move { r })
    }
}

fn ids(es: &[Entry]) -> Vec<u64> {
    es.iter().map(|e| e.id).collect()
}

fn user(state: &mut State, id: u64, text: &str) {
    state.items.push((EntryId(id), Item::User { form: Form::Prompt, text: text.into() }));
}

fn assistant(state: &mut State, id: u64, text: &str) {
    state.items.push((EntryId(id), Item::Assistant { content: vec![ContentBlock::text(text)] }));
}

fn memory_events<'a>(ev: &'a [EntryKind], name: &str) -> Vec<&'a aho_types::Value> {
    ev.iter()
        .filter_map(|e| match e {
            EntryKind::Custom { ns, name: n, body } if ns == NS && n == name => Some(body),
            _ => None,
        })
        .collect()
}

#[test]
fn write_time_merge_dedupes_near_duplicates_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(T0);
    let m = open(dir.path(), &clock);
    let a = block(m.upsert(Entry::new("The build uses cargo nextest for tests", vec!["build".into()]))).unwrap();
    assert_eq!(a, Upserted { id: 1, merged: false, supersedes: vec![] });
    let b = block(m.upsert(Entry::new("the build uses cargo nextest for tests.", vec!["ci".into(), "build".into()]))).unwrap();
    assert_eq!(b, Upserted { id: 1, merged: true, supersedes: vec![] });
    let c = save(&m, "Deploys go through fly.io with flyctl");
    assert!(!c.merged);

    let all = m.entries();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].tags, vec!["build".to_string(), "ci".to_string()]);
    assert!((all[0].confidence - (DEFAULT_CONFIDENCE + 0.05)).abs() < 1e-6, "a second sighting reinforces");
    assert_eq!(all[0].embedding_model, Some(ModelId::new(embed_local::MODEL)));
    assert_eq!(all[0].created_at, Timestamp(T0));

    let file = StorePaths::new(dir.path(), PROJECT).project;
    assert!(file.starts_with(dir.path().join(".aho/memory")) && file.exists());
    let reopened = open(dir.path(), &clock);
    assert_eq!(reopened.entries(), all, "store round-trips through the JSON file");
    assert_eq!(save(&reopened, "a third, unrelated fact about lunch").id, 3, "ids keep counting after reopen");
    assert!(block(m.upsert(Entry::new("   ", vec![]))).is_err(), "empty memories are refused");
}

#[test]
fn global_entries_live_in_the_shared_file() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(T0);
    let m = open(dir.path(), &clock);
    let mut e = Entry::new("Operator prefers terse answers", vec!["global".into()]);
    e.scope = Scope::Global;
    let g = block(m.upsert(e)).unwrap();
    assert_eq!(g.id, GLOBAL_BASE + 1);
    assert!(StorePaths::new(dir.path(), PROJECT).global.exists());
    // another project sees it
    let other = LocalMemory::open(
        &StorePaths::new(dir.path(), "/work/other"),
        "/work/other",
        Arc::new(HashNgram::new(0)),
        Arc::new(clock),
        Settings::default(),
    )
    .unwrap();
    assert_eq!(ids(&other.entries()), vec![GLOBAL_BASE + 1]);
}

#[test]
fn contradiction_is_marked_superseded_not_merged() {
    let dir = tempfile::tempdir().unwrap();
    let m = open(dir.path(), &ManualClock::at(T0));
    let old = save(&m, "The project uses tabs for indentation");
    let new = save(&m, "The project does not use tabs for indentation");
    assert!(!new.merged);
    assert_eq!(new.supersedes, vec![old.id]);
    let n = m.get(new.id).unwrap();
    assert_eq!(n.supersedes().collect::<Vec<_>>(), vec![old.id]);
    assert!((m.get(old.id).unwrap().confidence - DEFAULT_CONFIDENCE * 0.5).abs() < 1e-6);
    // unrelated negation is not a contradiction
    let x = save(&m, "Never commit directly to main");
    assert!(x.supersedes.is_empty());
}

#[test]
fn cross_model_entries_are_reachable_only_through_bm25() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(dir.path(), PROJECT);
    // A store written under another embedding model.
    let mut foreign = Entry::new("rotate the signing keys every month", vec![]);
    foreign.id = 1;
    foreign.created_at = Timestamp(T0);
    foreign = foreign
        .with_embedding(Vector { model: ModelId::new("all-MiniLM-L6-v2"), dims: hash_ngram_vector("rotate the signing keys every month") });
    std::fs::create_dir_all(paths.project.parent().unwrap()).unwrap();
    std::fs::write(&paths.project, serde_json::to_vec(&serde_json::json!({"version": 1, "next_id": 1, "entries": [foreign]})).unwrap())
        .unwrap();

    let m = open(dir.path(), &ManualClock::at(T0));
    let native = save(&m, "rotate the signing keys every month");
    assert!(!native.merged, "no cosine across models, so no merge");
    let hits = block(m.search("rotate signing keys", 5)).unwrap();
    let score = |id: u64| hits.iter().find(|h| h.entry.id == id).map(|h| h.score).unwrap();
    // identical text ties in BM25; the id tie-break ranks the foreign entry first there
    assert!((score(native.id) - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-6, "dense rank 1 + bm25 rank 2");
    assert!((score(1) - 1.0 / 61.0).abs() < 1e-6, "bm25 only: no dense contribution across models");
}

#[test]
fn bm25_and_fusion_rank_the_identifier_match_first_and_gate_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let m = open(dir.path(), &ManualClock::at(T0));
    let a = save(&m, "parse_config_v2 fails when the TOML has trailing commas");
    save(&m, "the config parser is documented in the wiki");
    let cat = save(&m, "my cat enjoys sleeping on sunny windowsills");
    let got = block(m.retrieve("why does parse_config_v2 choke on trailing commas", 5)).unwrap();
    assert_eq!(got[0].id, a.id);
    assert!(!ids(&got).contains(&cat.id));
    let hits = block(m.search("parse_config_v2", 5)).unwrap();
    assert_eq!(hits[0].entry.id, a.id);
    assert!(block(m.retrieve("quantum chromodynamics lecture", 5)).unwrap().is_empty(), "nothing relevant, nothing injected");
}

#[test]
fn judge_verifies_then_carry_limits_degraded_retrievals_to_the_verified_set() {
    let dir = tempfile::tempdir().unwrap();
    let judge = ScriptedJudge::new(vec![Ok(vec![2]), Err(MemoryError::Judge("route down".into())), Ok(vec![2]), Ok(vec![3])]);
    let m = open_with(dir.path(), &ManualClock::at(T0), Settings { rerank_every: 3, ..Settings::default() }).with_judge(judge.clone());
    save(&m, "the staging database listens on port 5433"); // 1: best fused match, never verified
    save(&m, "the staging database password lives in 1password"); // 2
    save(&m, "holiday photos are stored in the beach album"); // 3
    let q = "staging database port";
    let got = |m: &LocalMemory, q: &str| ids(&block(m.retrieve(q, 5)).unwrap());
    assert_eq!(got(&m, q), vec![2], "first retrieval reranks");
    assert_eq!(got(&m, q), vec![2], "between reranks: carry the verified set");
    assert_eq!(got(&m, q), vec![2]);
    assert_eq!(judge.calls(), 1);
    assert_eq!(got(&m, q), vec![2], "due again, judge fails: degrade to carry, never to unvetted #1");
    assert_eq!(judge.calls(), 2);
    let errors = memory_events(&m.drain_events(&|_| true), "Error").into_iter().cloned().collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["during"], serde_json::json!("rerank"));
    assert_eq!(got(&m, q), vec![2], "a failed rerank does not reset the schedule");
    assert_eq!(judge.calls(), 3);
    got(&m, q);
    assert_eq!(judge.calls(), 3);
    assert_eq!(got(&m, "where are the holiday beach photos stored"), vec![3]);
    assert_eq!(judge.calls(), 4, "a topic change forces a rerank");
}

#[test]
fn consensus_keeps_majority_ids() {
    let dir = tempfile::tempdir().unwrap();
    let judge = ScriptedJudge::new(vec![Ok(vec![1, 2]), Ok(vec![2]), Ok(vec![2, 1, 3])]);
    let m = open_with(dir.path(), &ManualClock::at(T0), Settings { votes: 3, ..Settings::default() }).with_judge(judge);
    save(&m, "the staging database listens on port 5433");
    save(&m, "the staging database password lives in 1password");
    save(&m, "staging deploys happen on fridays");
    assert_eq!(ids(&block(m.retrieve("staging database", 5)).unwrap()), vec![2, 1], "3 votes for #2, 2 for #1, 1 for #3");
}

#[test]
fn decay_then_prune_after_24h_is_logged_and_foldable() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(T0);
    let judge = ScriptedJudge::new(vec![Ok(vec![])]); // rejects everything it is shown
    let m = open_with(dir.path(), &clock, Settings { rerank_every: 1, ..Settings::default() }).with_judge(judge);
    let x = save(&m, "the staging database listens on port 5433");
    clock.advance_s(25 * H);
    let mut young = Entry::new("a low-confidence guess about staging logs", vec![]);
    young.confidence = 0.1;
    let young = block(m.upsert(young)).unwrap();

    let mut pruned = Vec::new();
    for round in 1..=20 {
        block(m.retrieve("staging database port", 5)).unwrap();
        let ev = block(m.maintain()).unwrap();
        pruned.extend(memory_events(&ev, "Pruned").into_iter().cloned());
        if m.get(x.id).is_none() {
            assert_eq!(round, 8, "0.5 × 0.85^8 < 0.15");
            break;
        }
    }
    assert_eq!(pruned.len(), 1);
    assert_eq!(pruned[0]["id"], serde_json::json!(x.id));
    let state = aho_memory::fold(None, "Pruned", &pruned[0]).unwrap();
    assert_eq!(state["pruned"], serde_json::json!([x.id]));
    assert!(m.get(young.id).is_some(), "below threshold but younger than 24 h: kept");
    assert!(open(dir.path(), &clock).get(x.id).is_none(), "prune reached the file");
}

#[test]
fn verified_pairs_are_boosted_and_linked_after_two_co_verifications() {
    let dir = tempfile::tempdir().unwrap();
    let judge = ScriptedJudge::new(vec![Ok(vec![1, 2])]);
    let m = open_with(dir.path(), &ManualClock::at(T0), Settings { rerank_every: 1, ..Settings::default() }).with_judge(judge);
    save(&m, "the staging database listens on port 5433");
    save(&m, "the staging database password lives in 1password");
    block(m.retrieve("staging database", 5)).unwrap();
    let first = block(m.maintain()).unwrap();
    assert!(m.get(1).unwrap().edges.is_empty(), "one co-verification is not enough");
    block(m.retrieve("staging database", 5)).unwrap();
    block(m.maintain()).unwrap();
    assert_eq!(m.get(1).unwrap().edges, vec![(2, Edge::RelatesTo)]);
    assert_eq!(m.get(2).unwrap().edges, vec![(1, Edge::RelatesTo)]);
    assert!((m.get(1).unwrap().confidence - 0.6).abs() < 1e-5, "boosted twice");
    let stats = memory_events(&first, "Maintained");
    assert_eq!(stats[0]["boosted"], serde_json::json!(2));
}

#[test]
fn producer_injects_once_per_fresh_turn_under_suppression_windows() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(T0);
    let m = Arc::new(open(dir.path(), &clock));
    let fact = save(&m, "the staging database listens on port 5433");
    let p = Producer::new(m.clone(), Suppression::default(), 12);
    let q = "which port does the staging database listen on?";

    let mut s = State::default();
    user(&mut s, 1, q);
    let text = p.context(&s).expect("fresh turn, fresh retrieval, injected on this prompt");
    assert!(text.contains(&format!("#{}", fact.id)) && text.contains("5433"), "{text}");
    assert_eq!(m.get(fact.id).unwrap().injected_at, Some(Timestamp(T0)));
    assert_eq!(p.context(&s), None, "a tool continuation of the same turn gets nothing");

    let mut id = 1;
    let mut turn = |secs: u64| {
        clock.set(T0 + secs * 1000);
        id += 2;
        assistant(&mut s, id - 1, "ok");
        user(&mut s, id, q);
        p.context(&s)
    };
    assert_eq!(turn(30), None, "identical payload within 90 s");
    assert_eq!(turn(100), None, "full id overlap within 180 s");
    assert_eq!(turn(200), None, "every id inside its 45-min TTL");
    assert!(turn(2700).is_some(), "TTL expired");
}

#[tokio::test]
async fn producer_discards_a_pending_retrieval_older_than_120s() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(T0);
    let m = Arc::new(open(dir.path(), &clock));
    m.upsert(Entry::new("the staging database listens on port 5433", vec![])).await.unwrap();
    let p = Producer::new(m.clone(), Suppression::default(), 12);
    let mut s = State::default();
    user(&mut s, 1, "staging database port?");
    let job = p.observe(&s).unwrap();
    p.run(job).await;
    clock.advance_s(121);
    assert_eq!(p.take_snapshot(), None);

    user(&mut s, 3, "staging database port, again?");
    let job = p.observe(&s).unwrap();
    p.run(job).await;
    clock.advance_s(119);
    assert!(p.take_snapshot().is_some(), "within 120 s it is still fresh");
}

#[test]
fn topic_tracker_fires_on_change_after_four_turns_and_every_n() {
    let v = |x: f32, y: f32| Vector { model: ModelId::new("m"), dims: vec![x, y] };
    let (a, b) = (v(1.0, 0.0), v(0.0, 1.0));
    let mut t = TopicTracker::new(12);
    assert_eq!(t.observe(Some(&a)), None);
    assert_eq!(t.observe(Some(&b)), None, "changed, but the previous topic had 1 turn");
    for _ in 0..3 {
        assert_eq!(t.observe(Some(&b)), None);
    }
    assert_eq!(t.observe(Some(&a)), Some(Trigger::TopicChange), "4 turns on b, then a");
    assert_eq!(t.observe(Some(&v(1.0, 0.1))), None, "same topic");
    let other = Vector { model: ModelId::new("other"), dims: vec![0.0, 1.0] };
    assert_eq!(t.observe(Some(&other)), None, "cross-model vectors never signal a change");
    for _ in 0..8 {
        assert_eq!(t.observe(None), None);
    }
    assert_eq!(t.observe(None), Some(Trigger::EveryNTurns(12)), "turn 12 of the topic");
}

#[test]
fn topic_change_extracts_the_previous_topic_window_only() {
    let dir = tempfile::tempdir().unwrap();
    let m = Arc::new(open(dir.path(), &ManualClock::at(T0)));
    let p = Producer::new(m.clone(), Suppression::default(), 12);
    let mut s = State::default();
    let rust = [
        "the rust borrow checker rejects the parser lifetimes",
        "remember: the parser module borrows the input buffer for its whole lifetime",
        "the borrow checker still rejects the parser lifetimes after the rewrite",
        "why does the borrow checker reject these parser lifetimes",
    ];
    let mut id = 0;
    for text in rust {
        id += 1;
        user(&mut s, id, text);
        p.context(&s);
        id += 1;
        assistant(&mut s, id, "looking");
    }
    assert!(m.entries().is_empty(), "no trigger yet");
    user(&mut s, id + 1, "remember: vacation photos go in the beach album\nplan the sunset holiday photo album");
    p.context(&s);
    let texts: Vec<String> = m.entries().into_iter().map(|e| e.text).collect();
    assert_eq!(
        texts,
        vec!["the parser module borrows the input buffer for its whole lifetime".to_string()],
        "the new topic's turn is not in the window"
    );
    let ev = m.drain_events(&|_| true);
    let x = memory_events(&ev, "Extracted");
    assert_eq!(x[0]["trigger"], serde_json::json!({"trigger": "topic_change"}));

    // session end picks up the rest
    block(Box::pin(p.session_end()));
    assert_eq!(m.entries().len(), 2);
    assert!(memory_events(&m.drain_events(&|_| true), "Extracted")[0]["trigger"] == serde_json::json!({"trigger": "session_end"}));
}

#[test]
fn skills_are_synthetic_entries_whose_hits_suggest_activation() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(T0);
    let m = Arc::new(open(dir.path(), &clock));
    let sid = block(m.add_synthetic("deploy-fly", "Deploy the app to fly.io with flyctl")).unwrap();
    assert!(sid > SKILL_BASE);
    assert!(m.entries().is_empty(), "synthetic entries are not stored");
    assert!(block(m.search("deploy fly.io", 5)).unwrap().is_empty(), "nor offered to model search");

    let p = Producer::new(m.clone(), Suppression::default(), 12);
    let mut s = State::default();
    user(&mut s, 1, "how do I deploy the app to fly.io?");
    let text = p.context(&s).unwrap();
    assert!(text.contains("Skills that match this request") && text.contains("deploy-fly"), "{text}");
    let customs = m.drain_events(&|e| matches!(e, EntryKind::Custom { .. }));
    assert!(customs.iter().all(|e| matches!(e, EntryKind::Custom { .. })));
    let ev = m.drain_events(&|_| true);
    assert!(
        ev.contains(&EntryKind::CapDelta { id: "deploy-fly".into(), op: CapDeltaOp::Activated, reason: SEMANTIC_MATCH.into() }),
        "activation stays queued past a customs-only drain: {ev:?}"
    );
    assert!(m.drain_events(&|_| true).is_empty(), "drained once");
}

fn loadout(comp: &Composition, sets: &[(&str, &str)]) -> aho_config::Loadout {
    let mut schema = Schema::default();
    comp.config_keys().into_iter().for_each(|k| schema.add(k));
    let mut i = ResolveInputs::default();
    i.sets = sets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    resolve(&i, &schema, false).unwrap()
}

fn policy() -> PolicyInput {
    PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite]) }
}

#[test]
fn capability_starts_on_embed_local_and_registers_handle_context_fold_and_hook() {
    let dir = tempfile::tempdir().unwrap();
    let comp = Composition::new("t").provide::<EmbedLocal>().provide::<MemoryLocal>();
    let home = dir.path().display().to_string();
    let l = loadout(&comp, &[("memory.home", &home), ("memory.project", "p")]);
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy(), vec![]).unwrap();
    assert!(matches!(p.row("memory-local").unwrap().state, CapState::Active));
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    let h = reg.handle::<MemoryHandle>().expect("memory handle");
    block(h.0.upsert(Entry::new("the staging database listens on port 5433", vec![]))).unwrap();
    assert!(StorePaths::new(dir.path(), "p").project.exists());
    assert_eq!(reg.contexts.len(), 1);
    assert!(reg.entry_kinds.contains_key(NS));
    assert!(reg.hooks.iter().any(|(_, h)| h.on.contains(&HookEvent::SessionEnd)));

    let mut s = State::default();
    user(&mut s, 1, "staging database port");
    assert!((reg.contexts[0].1)(&s).is_some_and(|t| t.contains("5433")));
}

#[test]
fn capability_is_locked_without_an_embedder_and_rejects_a_small_pool() {
    let comp = Composition::new("t").provide::<MemoryLocal>();
    let l = loadout(&comp, &[]);
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy(), vec![]).unwrap();
    assert!(matches!(&p.row("memory-local").unwrap().state, CapState::Locked { missing } if missing[0].contains("Seam(embed)")));

    let l = loadout(&comp, &[("memory.pool", "10")]);
    assert!(matches!(MemoryConfig::from_loadout(&l), Err(ConfigError::Type { key, .. }) if key == "memory.pool"));
}
