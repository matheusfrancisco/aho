use std::path::Path;

use super::*;
use aho_config::{ResolveInputs, Schema, resolve};
use world_host::HostWorld;

struct Fx {
    ws: tempfile::TempDir,
    _home: tempfile::TempDir,
    reg: Registry,
}

fn put(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// The host world over a fresh workspace (seeded by `files`) with `context-files` composed.
fn start(files: &[(&str, &str)], sets: &[(&str, &str)]) -> Fx {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    for (rel, text) in files {
        put(ws.path(), rel, text);
    }
    let comp = Composition::new("t").provide::<HostWorld>().consume::<ContextFiles>();
    let mut schema = Schema::default();
    comp.config_keys().into_iter().for_each(|k| schema.add(k));
    let mut inputs = ResolveInputs::default();
    inputs.sets = vec![
        ("workspace.root".into(), ws.path().to_string_lossy().into_owned()),
        ("workspace.shadow_dir".into(), home.path().to_string_lossy().into_owned()),
    ];
    inputs.sets.extend(sets.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let l = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let policy = PolicyInput { preset: "read-only".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
    let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
    assert!(matches!(p.row("context-files").unwrap().state, aho_log::CapState::Active));
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    Fx { ws, _home: home, reg }
}

fn mine<T>(rows: &[(CapabilityId, T)]) -> impl Iterator<Item = &T> {
    rows.iter().filter(|(c, _)| c.name == "context-files").map(|(_, t)| t)
}

/// What the session does before a request: the PreStep hooks, then the dynamic contexts.
async fn step(reg: &Registry) -> Option<String> {
    for h in mine(&reg.hooks).filter(|h| h.on.contains(&HookEvent::PreStep)) {
        assert_eq!((h.run)(HookEvent::PreStep, &json!({"turn": 1, "step": 1})).await, HookOutput::default(), "never a veto");
    }
    mine(&reg.contexts).find_map(|f| f(&State::default()))
}

fn after_call(reg: &Registry, args: Value) {
    let view = ExecView {
        call: CallId("c1".into()),
        tool: "write".into(),
        class: Consequence::WorkspaceWrite,
        args,
        policy: PolicyView {
            epoch: Epoch(1),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: String::new(),
            home: String::new(),
        },
        by: Principal::system("test"),
    };
    let done = ExecOutcome::text("ok");
    for f in mine(&reg.post) {
        assert_eq!(f(&view, &done), PostDecision::Accept { content: None, deferred_context: vec![] }, "the result is untouched");
    }
}

#[tokio::test]
async fn root_files_render_in_order_and_identical_content_is_shown_once() {
    let f = start(
        &[(".claude/CLAUDE.md", "Prefer small diffs.\n"), ("AGENTS.md", "Run cargo test.\n"), ("CLAUDE.md", "Run cargo test.\n")],
        &[],
    );
    let text = step(&f.reg).await.unwrap();
    let agents = text.find("<repo-instructions path=\"AGENTS.md\">\nRun cargo test.\n</repo-instructions>").expect(&text);
    let twin = text.find("<repo-instructions path=\"CLAUDE.md\" same-as=\"AGENTS.md\"/>").expect(&text);
    let dot = text.find("<repo-instructions path=\".claude/CLAUDE.md\">\nPrefer small diffs.\n</repo-instructions>").expect(&text);
    assert!(agents < twin && twin < dot, "configured order: {text}");
    assert_eq!(text.matches("Run cargo test.").count(), 1, "{text}");
    assert!(!text.contains("<nested-instructions>"), "root files are not nested: {text}");
}

#[tokio::test]
async fn a_workspace_without_instruction_files_adds_no_context() {
    let f = start(&[("src/main.rs", "fn main() {}\n"), ("README.md", "hi\n")], &[]);
    assert_eq!(step(&f.reg).await, None);
}

#[tokio::test]
async fn oversized_files_are_cut_per_file_and_in_total_with_a_marker() {
    let f = start(
        &[("AGENTS.md", "0123456789abcdefghij"), ("CLAUDE.md", "ABCDEFGHIJKLMNOPQRST"), ("CLAUDE.local.md", "local rules")],
        &[("context_files.max_bytes", "10")],
    );
    let text = step(&f.reg).await.unwrap();
    assert!(text.contains("path=\"AGENTS.md\">\n0123456789\n[truncated at 10 of 20 bytes]\n</repo-instructions>"), "{text}");
    assert!(!text.contains("abcdefghij"), "{text}");
    assert!(text.contains("path=\"CLAUDE.md\">\nABCDEFGHIJ\n[truncated at 10 of 20 bytes]"), "{text}");
    assert!(text.contains("path=\"CLAUDE.local.md\">\n\n[truncated at 0 of 11 bytes]"), "total budget (2 × max) spent: {text}");
}

#[tokio::test]
async fn truncation_never_splits_a_character() {
    let f = start(&[("AGENTS.md", "ééééé")], &[("context_files.max_bytes", "5")]);
    let text = step(&f.reg).await.unwrap();
    assert!(text.contains(">\néé\n[truncated at 4 of 10 bytes]"), "{text}");
}

#[tokio::test]
async fn an_edit_during_the_session_shows_at_the_next_step() {
    let f = start(&[("AGENTS.md", "Use tabs.\n")], &[]);
    assert!(step(&f.reg).await.unwrap().contains("Use tabs."));
    put(f.ws.path(), "AGENTS.md", "Use spaces.\n");
    let text = step(&f.reg).await.unwrap();
    assert!(text.contains("Use spaces.") && !text.contains("Use tabs."), "{text}");
}

#[tokio::test]
async fn nested_files_are_listed_by_path_and_a_call_naming_one_refreshes_the_list() {
    let f = start(
        &[
            ("AGENTS.md", "root\n"),
            ("crates/x/CLAUDE.md", "x rules\n"),
            ("sub/AGENTS.md", "sub rules\n"),
            ("vendor/AGENTS.md", "ignored\n"),
            (".gitignore", "vendor/\n"),
        ],
        &[],
    );
    let text = step(&f.reg).await.unwrap();
    assert!(text.ends_with("<nested-instructions>\ncrates/x/CLAUDE.md\nsub/AGENTS.md\n</nested-instructions>"), "{text}");
    assert!(!text.contains("sub rules"), "paths only: {text}");

    put(f.ws.path(), "a/b/AGENTS.md", "deep\n");
    after_call(&f.reg, json!({"path": "a/b/AGENTS.md", "content": "deep\n"}));
    let text = step(&f.reg).await.unwrap();
    assert!(text.contains("<nested-instructions>\na/b/AGENTS.md\ncrates/x/CLAUDE.md\n"), "{text}");
}

#[tokio::test]
async fn nested_files_alone_still_produce_the_listing() {
    let f = start(&[("pkg/AGENTS.md", "pkg\n")], &[]);
    assert!(step(&f.reg).await.unwrap().contains("<nested-instructions>\npkg/AGENTS.md\n</nested-instructions>"));
}

#[tokio::test]
async fn disabled_registers_nothing_the_model_sees() {
    let f = start(&[("AGENTS.md", "Run cargo test.\n"), ("sub/AGENTS.md", "x\n")], &[("context_files.enabled", "false")]);
    assert_eq!(step(&f.reg).await, None);
    assert_eq!(mine(&f.reg.hooks).count() + mine(&f.reg.contexts).count(), 0);
}

#[tokio::test]
async fn configured_names_replace_the_defaults() {
    let f = start(&[("AGENTS.md", "agents\n"), ("docs/RULES.md", "rules\n")], &[("context_files.names", "[\"docs/RULES.md\"]")]);
    let text = step(&f.reg).await.unwrap();
    assert!(text.contains("<repo-instructions path=\"docs/RULES.md\">\nrules\n</repo-instructions>"), "{text}");
    assert!(!text.contains("agents"), "{text}");
}
