#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::CapState;
use aho_memory::{ManualClock, MemoryHandle};
use aho_types::{Card, Consequence, ConsequenceSet, ContentBlock};
use embed_local::{EmbedLocal, HashNgram};
use memory_local::{GLOBAL_BASE, LocalMemory, MemoryLocal, Settings, StorePaths};
use serde_json::json;
use tool_memory::*;

fn handle(dir: &std::path::Path) -> MemoryHandle {
    let m = LocalMemory::open(
        &StorePaths::new(dir, "/p"),
        "/p",
        Arc::new(HashNgram::new(16)),
        Arc::new(ManualClock::at(1_000)),
        Settings::default(),
    )
    .unwrap();
    MemoryHandle(Arc::new(m))
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(ContentBlock::as_text).collect(),
        other => panic!("not done: {other:?}"),
    }
}

fn code(o: &ExecOutcome) -> &str {
    match o {
        ExecOutcome::Err { code, .. } => code,
        other => panic!("not an error: {other:?}"),
    }
}

#[tokio::test]
async fn save_then_search_round_trip_with_merge_and_global_scope() {
    let dir = tempfile::tempdir().unwrap();
    let h = handle(dir.path());
    let a = save(&h, &json!({"text": "CI runs `cargo nextest run --workspace` on every push", "tags": ["ci"]})).await;
    assert_eq!(text(&a), "Saved memory #1.");
    let again = save(&h, &json!({"text": "CI runs `cargo nextest run --workspace` on every push."})).await;
    assert!(text(&again).starts_with("Merged into existing memory #1"));
    let g = save(&h, &json!({"text": "The operator prefers terse answers", "tags": ["global"]})).await;
    let ExecOutcome::Done { value, .. } = &g else { panic!() };
    assert_eq!(value["id"], json!(GLOBAL_BASE + 1));
    assert_eq!(value["scope"], json!("global"));

    let r = search(&h, &json!({"query": "how does CI run nextest", "k": 3})).await;
    let ExecOutcome::Done { value, content, meta } = &r else { panic!("{r:?}") };
    assert_eq!(value["hits"][0]["id"], json!(1));
    assert_eq!(value["hits"][0]["tags"], json!(["ci", "agent"]));
    assert!(value.get("events").is_none(), "nothing queued, nothing forwarded");
    assert!(content[0].as_text().unwrap().starts_with("#1 [ci, agent] (confidence 0.65): CI runs"));
    assert!(matches!(meta, Some(Card::Search { hits, .. }) if hits[0].starts_with("#1 ")));

    let none = search(&h, &json!({"query": "quantum chromodynamics"})).await;
    assert_eq!(text(&none), "No saved memory matches `quantum chromodynamics`.");
}

#[tokio::test]
async fn bad_arguments_are_reported_not_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let h = handle(dir.path());
    assert_eq!(code(&search(&h, &json!({})).await), "invalid_args");
    assert_eq!(code(&search(&h, &json!({"query": "x", "k": 0})).await), "invalid_args");
    assert_eq!(code(&search(&h, &json!({"query": "x", "k": MAX_K + 1})).await), "invalid_args");
    assert_eq!(code(&save(&h, &json!({"text": "  "})).await), "invalid_args");
    assert_eq!(code(&save(&h, &json!({"text": "x", "tags": [1]})).await), "invalid_args");
    assert_eq!(code(&save(&h, &json!({"text": "x", "tags": "ci"})).await), "invalid_args");
}

#[tokio::test]
async fn queued_memory_entries_ride_on_the_tool_result_but_activations_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let m = Arc::new(
        LocalMemory::open(
            &StorePaths::new(dir.path(), "/p"),
            "/p",
            Arc::new(HashNgram::new(16)),
            Arc::new(ManualClock::at(1_000)),
            Settings::default(),
        )
        .unwrap(),
    );
    let h = MemoryHandle(m.clone());
    save(&h, &json!({"text": "the staging database listens on port 5433"})).await;
    // an extraction queues Memory/Extracted; a skill hit queues a Cap/Delta (as the context producer does)
    h.0.extract(aho_memory::Trigger::SessionEnd, &[aho_types::Message::user("remember: prod runs on fly.io")]).await.unwrap();
    h.0.add_synthetic("deploy-fly", "Deploy the app to fly.io with flyctl").await.unwrap();
    let hits = h.0.retrieve("deploy the app to fly.io", 5).await.unwrap();
    m.emit(aho_memory::skill_suggestions(&hits));

    let r = search(&h, &json!({"query": "staging database"})).await;
    let ExecOutcome::Done { value, .. } = &r else { panic!("{r:?}") };
    let events = value["events"].as_array().expect("events forwarded");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["ns"], json!("Memory"));
    assert_eq!(events[0]["name"], json!("Extracted"));
    assert_eq!(events[0]["body"]["trigger"], json!({"trigger": "session_end"}));
    assert!(aho_memory::fold(None, "Extracted", &events[0]["body"]).is_ok(), "the registered fold accepts what we forward");

    let again = save(&h, &json!({"text": "another fact entirely about lunch"})).await;
    let ExecOutcome::Done { value, .. } = &again else { panic!() };
    assert!(value.get("events").is_none(), "forwarded once");
    let left = h.0.drain_events(&|_| true);
    assert!(
        matches!(left.as_slice(), [aho_log::EntryKind::CapDelta { id, .. }] if id == "deploy-fly"),
        "activation stays for the session: {left:?}"
    );
}

fn loadout(comp: &Composition, sets: &[(&str, &str)]) -> aho_config::Loadout {
    let mut schema = Schema::default();
    comp.config_keys().into_iter().for_each(|k| schema.add(k));
    let mut i = ResolveInputs::default();
    i.sets = sets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    resolve(&i, &schema, false).unwrap()
}

#[test]
fn plans_within_its_ceiling_and_flags_save_above_read_only() {
    let comp = Composition::new("t").provide::<EmbedLocal>().provide::<MemoryLocal>().consume::<ToolMemory>();
    let l = loadout(&comp, &[]);
    let env = Env::empty(Os::current());
    let workspace =
        PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite]) };
    let p =
        plan(&comp.manifests(), &l, &env, &workspace, vec![]).expect("memory_save's WorkspaceWrite is within the FsWrite-derived ceiling");
    assert!(matches!(p.row("tool-memory").unwrap().state, CapState::Active));
    let read_only = PolicyInput { preset: "read-only".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
    let p = plan(&comp.manifests(), &l, &env, &read_only, vec![]).unwrap();
    let row = p.row("tool-memory").unwrap();
    assert!(row.over_preset.contains(Consequence::WorkspaceWrite), "memory_save is flagged above a read-only preset");
    assert!(!row.over_preset.contains(Consequence::ReadOnly));
}

#[test]
fn does_not_build_without_a_world_granting_fs_write() {
    let dir = tempfile::tempdir().unwrap();
    let comp = Composition::new("t").provide::<EmbedLocal>().provide::<MemoryLocal>().consume::<ToolMemory>();
    let home = dir.path().display().to_string();
    let l = loadout(&comp, &[("memory.home", &home), ("memory.project", "p")]);
    let policy =
        PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite]) };
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    let (reg, broken) = comp.start(&p, &l);
    assert!(reg.handle::<MemoryHandle>().is_some(), "the provider still starts");
    assert!(broken.iter().any(|(id, e)| id.name == "tool-memory" && e.contains("FsWrite")), "{broken:?}");
    assert!(reg.tool("memory_save").is_none());
}
