use std::collections::BTreeMap;
use std::sync::Arc;

use aho_approval::{Decided, NeverApproval};
use aho_cap::{BoundWorld, Composition, Registry};
use aho_config::{FileSnapshot, Layer};
use aho_inspect::{Missing, TreeNode};
use aho_llm::LlmRoute;
use aho_log::{CapState, CheckpointCause, EntryKind, GrantScope, LogStore, MemoryLog, RewindMode, ToolOutcome, Transition};
use aho_policy::{ApprovalCard, DecisionReason};
use aho_session::{Boot, Session};
use aho_types::*;
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

use crate::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Draws through ratatui's `TestBackend` (the same `draw` the terminal loop uses).
fn screen(ui: &UiState, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| {
        let mut probe = Probe::default();
        let area = f.area();
        draw(ui, area, f.buffer_mut(), &mut probe);
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    buffer_lines(&buf, buf.area).join("\n")
}

fn region(text: &str, from: &str, rows: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|l| l.contains(from)).unwrap_or(0);
    lines[start..(start + rows).min(lines.len())].join("\n")
}

fn vm_card(cut: Option<&str>) -> ApprovalCard {
    ApprovalCard {
        tool: "vm_start".into(),
        class: Consequence::ControlsVM,
        args: json!({"name": "ubuntu-24"}),
        reason: DecisionReason::Preset("dev".into()),
        world: BoundWorld { name: "namespace".into(), confines: true, enforced_by: "linux namespaces".into() },
        suggested_cut: cut.map(str::to_string),
        rendered: "preset `dev` does not allow ControlsVM".into(),
    }
}

fn log_with(kinds: Vec<(Principal, EntryKind)>) -> Vec<aho_log::Entry> {
    let log = MemoryLog::new();
    kinds.into_iter().map(|(by, k)| log.append(None, by, k).unwrap()).collect()
}

fn model() -> Principal {
    Principal::system("model")
}

#[test]
fn approval_modal_renders_the_card_fields_and_answers_with_the_doc_keys() {
    let mut ui = UiState::default();
    ui.state.policy.epoch = Epoch(3);
    ui.approval = Some(ApprovalView::new(&vm_card(Some("preset lab")), Some("tool-vm".into()), &ui.state));
    let text = screen(&ui, 90, 26);
    let card = region(&text, "╭ approval", 10);
    println!("--- approval modal (TestBackend 90x26) ---\n{card}");
    for needle in [
        "tool-vm.vm_start",
        "class: ControlsVM",
        "args: { name: \"ubuntu-24\" }",
        "why:  preset `dev` does not allow ControlsVM      (reason: Preset)",
        "world: namespace (linux namespaces)  ·  lease: none yet",
        "[a] allow once   [s] allow for session → opens epoch 4",
        "[p] aho preset lab",
        "[r] reject",
        "[Esc] cancel",
    ] {
        assert!(card.contains(needle), "missing `{needle}` in\n{card}");
    }

    // The modal owns the keyboard: typing does not reach the draft.
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('x'))), None);
    assert!(ui.local.draft.is_empty());
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('a'))), Some(Action::Answer(Decided::allowed(GrantScope::Once))));
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('s'))), Some(Action::Answer(Decided::allowed(GrantScope::Session))));
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('r'))), Some(Action::Answer(Decided::rejected())));
    assert_eq!(on_key(&mut ui, key(KeyCode::Esc)), Some(Action::Answer(Decided::cancelled())));
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('p'))), Some(Action::PresetCut("lab".into())));
}

#[test]
fn approval_without_a_preset_cut_offers_no_preset_key() {
    let mut ui = UiState::default();
    ui.approval = Some(ApprovalView::new(&vm_card(None), None, &ui.state));
    let text = screen(&ui, 90, 26);
    assert!(!text.contains("[p]"), "{text}");
    assert!(text.contains("vm_start") && text.contains("opens epoch 2"), "{text}");
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('p'))), None);
    assert!(notice(&ui).contains("no single policy change"));

    let long = "remove rule `ask write(*)` (File /Users/someone/projects/a-rather-long-workspace-name/aho.toml:10)";
    let mut rule = vm_card(Some(long));
    rule.reason = DecisionReason::Rule { origin: "./aho.toml:10".into(), rule: "ask write(*)".into() };
    ui.approval = Some(ApprovalView::new(&rule, None, &ui.state));
    let (buf, frame) = capture(&ui, 100, 30);
    let text = buffer_lines(&buf, buf.area).join("\n");
    assert!(
        text.contains("[p] aho remove rule") && text.contains("[r] reject") && text.contains("[Esc] cancel"),
        "a long cut never pushes the refusal keys off the card:\n{text}"
    );
    assert!(frame.anomalies.is_empty(), "{:?}", frame.anomalies);
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('p'))), None, "a rule cut is not applied from the card");
    assert!(notice(&ui).contains("is not a preset change"));
}

fn node(id: &str, kind: &str, state: CapState, missing: Vec<Missing>, action: Option<&str>) -> TreeNode {
    TreeNode {
        id: id.into(),
        kind: kind.into(),
        origin: "bundled".into(),
        source: None,
        state,
        because: String::new(),
        bound: vec![],
        classes: ConsequenceSet::EMPTY,
        ceiling: ConsequenceSet::EMPTY,
        enforced_by: String::new(),
        settings: vec![],
        missing,
        action: action.map(str::to_string),
        over_preset: None,
        doc: String::new(),
        children: vec![],
    }
}

#[test]
fn tree_is_a_panel_in_front_of_the_transcript_with_the_state_glyphs_and_each_fix_and_esc_closes_it() {
    let mut ui = UiState::default();
    let mut mcp = node(
        "mcp.github",
        "mcp",
        CapState::Locked { missing: vec!["Credential(github)".into()] },
        vec![Missing { requirement: "Credential(github)".into(), fix: Some("aho credential set github".into()) }],
        Some("aho credential set github"),
    );
    mcp.children.push(node("mcp__github__*", "tool", CapState::Locked { missing: vec![] }, vec![], None));
    ui.tree = vec![
        node("tool-fs", "tool", CapState::Active, vec![], None),
        node("tool-terminal", "tool", CapState::Unlocked, vec![], Some("aho enable tool-terminal")),
        mcp,
        node("skill.pdf-extract", "skill", CapState::Broken { err: "frontmatter invalid at line 3".into() }, vec![], None),
        node("web-search", "tool", CapState::Disabled { why: "--disable web-search".into() }, vec![], Some("aho enable web-search")),
    ];
    let closed = screen(&ui, 120, 24);
    assert!(!closed.contains("tool-fs"), "no sidebar: the tree is not on screen until asked for\n{closed}");

    assert_eq!(typed(&mut ui, "/tree"), None, "a view opens here; nothing reaches the session");
    assert_eq!(front(&ui), "panel");
    let text = screen(&ui, 120, 24);
    println!("--- /tree panel (TestBackend 120x24) ---\n{text}");
    for needle in [
        "╭ tree ",
        "● tool-fs",
        "○ tool-terminal",
        "aho enable tool-terminal",
        "◌ mcp.github",
        "Credential(github)",
        "fix: aho credential set github",
        "  ◌ mcp__github__*",
        "✕ skill.pdf-extract",
        "frontmatter invalid at line 3",
        "⊘ web-search",
        "esc close",
    ] {
        assert!(text.contains(needle), "missing `{needle}` in\n{text}");
    }
    // The panel owns the keyboard, then Esc hands it back to the prompt.
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('x'))), None);
    assert!(ui.local.draft.is_empty());
    on_key(&mut ui, key(KeyCode::Esc));
    assert_eq!(front(&ui), "none");
    assert!(!screen(&ui, 120, 24).contains("mcp.github"));
    // Panels stack: the top one closes first.
    typed(&mut ui, "/jobs");
    ui.overlays.push(Overlay::Panel(Panel::view(View::Invariants)));
    on_key(&mut ui, key(KeyCode::Char('q')));
    assert!(matches!(ui.overlays.last(), Some(Overlay::Panel(p)) if p.title == "jobs"));
}

fn notice(ui: &UiState) -> String {
    ui.notice.as_ref().map(|n| n.text.clone()).unwrap_or_default()
}

#[test]
fn placeholder_card_flips_to_the_result_in_place() {
    let call = CallId("c1".into());
    let entries = log_with(vec![
        (Principal::system("loop"), EntryKind::TurnStart { turn: TurnId(1) }),
        (Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: "list files".into(), client_key: None }),
        (
            model(),
            EntryKind::AssistantMessage {
                content: vec![
                    ContentBlock::text("checking"),
                    ContentBlock::ToolUse { id: call.clone(), name: "bash".into(), args: json!({"command": "ls"}) },
                ],
                usage: Usage::default(),
                stop: StopReason::ToolUse,
            },
        ),
        (
            model(),
            EntryKind::ToolCall {
                call: call.clone(),
                tool: "bash".into(),
                args: json!({"command": "ls"}),
                class: Consequence::SpawnsProcess,
            },
        ),
        (
            Principal::system("ops"),
            EntryKind::OpStarted {
                op: OpId(4),
                call: call.clone(),
                kind: aho_log::OpKind::Process,
                rerunnable: false,
                external_effect: false,
            },
        ),
        (
            Principal::system("tools"),
            EntryKind::ToolResult {
                call: call.clone(),
                outcome: ToolOutcome::Ok,
                value: json!({}),
                content: vec![ContentBlock::text("a.txt\nb.txt")],
                spill_ref: None,
                meta: Some(Card::Terminal { command: "ls".into(), output: "a.txt\nb.txt".into(), exit: Some(0) }),
                late: true,
            },
        ),
    ]);
    let caps = BTreeMap::from([("bash".to_string(), "tool-bash".to_string())]);
    let mut ui = UiState { tool_caps: caps, ..UiState::default() };
    ui.refold(&entries[..5], &Default::default()).unwrap();
    let before = screen(&ui, 100, 20);
    assert!(before.contains("▍ tool-bash.bash · SpawnsProcess ⋯ running (op 4)"), "{before}");
    let blocks = ui.transcript.blocks.len();
    let card_at = ui.transcript.blocks.iter().position(|b| matches!(b.body, Body::Tool(_))).unwrap();

    assert!(ui.transcript.continues(&entries[5]));
    ui.apply(&entries[5], &Default::default()).unwrap();
    let after = screen(&ui, 100, 20);
    assert!(after.contains("▍ tool-bash.bash · SpawnsProcess ✓ $ ls  → exit 0 (late)"), "{after}");
    assert!(after.contains("b.txt") && !after.contains("⋯ running"), "{after}");
    assert_eq!(ui.transcript.blocks.len(), blocks, "the result flips the card; it does not add a block");
    assert!(matches!(&ui.transcript.blocks[card_at].body, Body::Tool(c) if c.result.is_some()));
}

#[test]
fn rewind_lists_the_checkpoints_then_offers_the_three_modes_plan_first() {
    let entries = log_with(vec![
        (Principal::system("loop"), EntryKind::TurnStart { turn: TurnId(1) }),
        (Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: "implement A".into(), client_key: None }),
        (
            model(),
            EntryKind::AssistantMessage { content: vec![ContentBlock::text("done")], usage: Usage::default(), stop: StopReason::EndTurn },
        ),
        (
            Principal::system("checkpoint"),
            EntryKind::WorkspaceCheckpoint { id: CheckpointId(2), tree: Hash::of(b"t2"), parent: None, cause: CheckpointCause::TurnEnd },
        ),
        (Principal::system("loop"), EntryKind::TurnEnd { turn: TurnId(1) }),
    ]);
    let mut ui = UiState::default();
    assert_eq!(typed(&mut ui, "/rewind"), None);
    assert!(notice(&ui).contains("no checkpoint"), "nothing to rewind to yet");
    ui.refold(&entries, &Default::default()).unwrap();
    let text = screen(&ui, 100, 20);
    assert!(text.contains("◆ checkpoint c2 · turn 1 · turn end") && text.contains("/rewind"), "{text}");

    assert_eq!(typed(&mut ui, "/rewind"), None);
    let list = screen(&ui, 100, 20);
    assert!(list.contains("rewind to…") && list.contains("▸ ◆ c2 · turn 1 · turn end"), "{list}");
    let cp = CheckpointId(2);
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), Some(Action::PlanRewind { checkpoint: cp, mode: RewindMode::Both }));
    let picker = screen(&ui, 100, 20);
    println!("--- rewind picker (TestBackend 100x20) ---\n{}", region(&picker, "rewind to c2", 12));
    for needle in ["rewind to c2", "▸ [1] both ", "[2] chat ", "[3] code ", "computing the plan"] {
        assert!(picker.contains(needle), "missing `{needle}` in\n{picker}");
    }
    // Enter before the plan is shown applies nothing.
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), None);
    assert_eq!(on_key(&mut ui, key(KeyCode::Char('3'))), Some(Action::PlanRewind { checkpoint: cp, mode: RewindMode::CodeOnly }));
    let Some(Overlay::Rewind(p)) = ui.overlays.last_mut() else { panic!("{:?}", ui.overlays) };
    p.plan = Some(Ok(RewindPlan {
        checkpoint: cp,
        mode: RewindMode::CodeOnly,
        fork_at: None,
        restore: Some((vec!["src/a.rs".into()], vec!["b.rs".into()])),
        kill: vec!["op3 bash".into()],
    }));
    let planned = screen(&ui, 100, 20);
    assert!(planned.contains("restore files: 1 changed, 1 deleted  src/a.rs, b.rs"), "{planned}");
    assert!(planned.contains("kill 1 running job(s): op3 bash"), "{planned}");
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), Some(Action::Rewind { checkpoint: cp, mode: RewindMode::CodeOnly }));
    assert!(ui.overlays.is_empty(), "applying closes the picker and the list under it");
}

#[test]
fn enter_prompts_when_idle_and_steers_when_busy() {
    let mut ui = UiState::default();
    for c in "hello".chars() {
        on_key(&mut ui, key(KeyCode::Char(c)));
    }
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), Some(Action::Prompt("hello".into())));
    ui.busy = true;
    on_key(&mut ui, key(KeyCode::Char('x')));
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), Some(Action::Steer { text: "x".into(), urgent: false }));
    on_key(&mut ui, key(KeyCode::Char('y')));
    assert_eq!(on_key(&mut ui, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)), Some(Action::Steer { text: "y".into(), urgent: true }));
    assert_eq!(on_key(&mut ui, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(Action::Abort));
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), None, "an empty draft sends nothing");
}

#[test]
fn hud_annotates_blocks_with_their_origin() {
    let call = CallId("c9".into());
    let entries = log_with(vec![(
        model(),
        EntryKind::ToolCall { call, tool: "read".into(), args: json!({"path": "a"}), class: Consequence::ReadOnly },
    )]);
    let mut ui = UiState { tool_caps: BTreeMap::from([("read".to_string(), "tool-fs".to_string())]), ..UiState::default() };
    ui.refold(&entries, &Default::default()).unwrap();
    assert!(!screen(&ui, 100, 16).contains("↳ e"));
    on_key(&mut ui, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT));
    let text = screen(&ui, 100, 16);
    assert!(text.contains(&format!("↳ e{} · by model · cap tool-fs · class ReadOnly", entries[0].id.0)), "{text}");
}

#[test]
fn frame_capture_drops_identical_frames_unless_anomalous_and_flags_oscillation() {
    let ui = UiState::default();
    let mut ring = FrameCapture::new(8);
    assert!(ring.record(capture(&ui, 100, 20).1).is_some());
    assert!(ring.record(capture(&ui, 100, 20).1).is_none(), "an identical frame is dropped");
    assert_eq!(ring.dropped(), 1);

    let (_, tiny) = capture(&ui, 30, 6);
    assert!(tiny.anomalies.iter().any(|a| a.contains("smaller than 40x10")), "{:?}", tiny.anomalies);
    assert!(ring.record(tiny.clone()).is_some());
    assert!(ring.record(tiny).is_some(), "an anomalous frame is kept even when identical");

    let mut ring = FrameCapture::new(8);
    let wide = capture(&ui, 100, 20).1;
    let narrow = capture(&ui, 60, 20).1;
    ring.record(wide.clone());
    ring.record(narrow.clone());
    ring.record(wide);
    let flagged = ring.record(narrow).cloned().unwrap();
    assert!(flagged.anomalies.iter().any(|a| a.starts_with(LAYOUT_OSCILLATION)), "{:?}", flagged.anomalies);
    let names: Vec<&str> = flagged.rects.iter().map(|r| r.0.as_str()).collect();

    // A layout that changes with the content (picker opens, its plan arrives, the mode changes)
    // never repeats a screen, so it is not flicker.
    let mut ring = FrameCapture::new(8);
    let picker_at = |selected, plan: Option<RewindMode>| {
        let mut ui = UiState::default();
        let plan =
            plan.map(|mode| Ok(RewindPlan { checkpoint: CheckpointId(1), mode, fork_at: Some(EntryId(5)), restore: None, kill: vec![] }));
        ui.overlays.push(Overlay::Rewind(RewindPicker { checkpoint: CheckpointId(1), entry: EntryId(5), selected, plan }));
        ui
    };
    let picker = picker_at(RewindMode::Both, None);
    let planned_both = picker_at(RewindMode::Both, Some(RewindMode::Both));
    let code = picker_at(RewindMode::CodeOnly, None);
    let planned_code = picker_at(RewindMode::CodeOnly, Some(RewindMode::CodeOnly));
    for s in [&ui, &picker, &planned_both, &code, &planned_code] {
        let kept = ring.record(capture(s, 100, 24).1).cloned().unwrap();
        assert!(kept.anomalies.is_empty(), "{:?}", kept.anomalies);
    }
    assert_eq!(names, ["transcript", "input", "footer"], "one column: no header bar, no sidebar at any width");
}

#[test]
fn headless_json_is_the_trimmed_screen_with_named_areas() {
    let mut ui = UiState::default();
    ui.local.draft = "draft text".into();
    ui.local.cursor = 10;
    let v = render_frame_json(&ui, 100, 20);
    let lines = v["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 20);
    assert!(lines.iter().all(|l| !l.as_str().unwrap().ends_with(' ')));
    let input = v["areas"].as_array().unwrap().iter().find(|a| a["area"] == "input").unwrap();
    assert!(input["text"].as_str().unwrap().contains("draft text"));
    assert_eq!((v["v"].as_u64(), v["overlay"].as_str()), (Some(2), Some("none")));
    assert!(v["menu"].is_null());
    ui.local.draft = "/tre".into();
    ui.local.cursor = 4;
    let v = render_frame_json(&ui, 100, 20);
    assert_eq!(v["menu"]["items"], json!(["/tree"]), "the open menu is part of the screen record");
    assert!(v["areas"].as_array().unwrap().iter().any(|a| a["area"] == "menu"));
}

// ───────────────────────────── against a real session ─────────────────────────────

struct Rig {
    planned: aho_compose::Planned,
    comp: Composition,
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
    let comp = Composition::new("tui-test").provide::<world_host::HostWorld>().consume::<tool_fs::ToolFs>().surface::<SurfaceTui>();
    let text = format!(
        "[model]\nprovider = \"scripted\"\n[tools]\ngrace_ms = 150\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n",
        root.display(),
        home.join("shadow").display()
    );
    let mut inputs = aho_compose::Inputs::new(&root, &home);
    inputs.files.push(FileSnapshot { path: root.join("aho.toml").display().to_string(), layer: Layer::FileProject, text });
    if let Ok(p) = std::env::var("PATH") {
        inputs.env.insert("PATH".into(), p);
    }
    let planned = aho_compose::load(&comp, &inputs).unwrap();
    Rig { planned, comp, root, home, _dir: dir }
}

async fn boot(r: &Rig, steps: Vec<Step>) -> Arc<Session> {
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(Arc::new(ScriptedLlm::from_steps(steps)))));
    Session::boot(Boot {
        id: SessionId(0x5eed),
        composition: &r.comp,
        plan: &r.planned.plan,
        loadout: &r.planned.loadout,
        bootstrap: aho_compose::bootstrap_entries(&r.planned),
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: r.root.display().to_string(),
        home: r.home.display().to_string(),
        spill_dir: r.home.join("spill"),
        seed,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn live_events_fold_to_the_same_screen_as_a_fresh_fold() {
    let r = rig();
    let s = boot(
        &r,
        vec![
            Step::tool_call("read", json!({"path": "a.txt"})).with_stop(ScriptedStop::ToolUse),
            Step::text("the file says alpha").expect_tool_result("alpha"),
        ],
    )
    .await;
    let mut live = ui_from_session(&s, &r.planned).unwrap();
    let folds = s.registry().entry_kinds.clone();
    let mut events = s.subscribe();
    assert_eq!(s.prompt("read a.txt", None).await.unwrap(), Transition::Completed);
    while let Ok(ev) = events.try_recv() {
        match ev {
            aho_session::SessionEvent::Entry(e) if !live.transcript.already_seen(&e) => {
                assert!(live.transcript.continues(&e));
                live.apply(&e, &folds).unwrap();
            }
            aho_session::SessionEvent::Stream(ev) => live.transcript.stream(&ev),
            _ => {}
        }
    }
    let fresh = ui_from_session(&s, &r.planned).unwrap();
    assert_eq!(live.transcript.blocks, fresh.transcript.blocks);
    let text = screen(&fresh, 120, 30);
    assert!(text.contains("❯ read a.txt"), "{text}");
    assert!(text.contains("▍ tool-fs.read · ReadOnly ✓"), "{text}");
    assert!(text.contains("the file says alpha"), "{text}");
    assert!(text.contains("◆ checkpoint c"), "turn end leaves a marker:\n{text}");
    let mut treed = fresh.clone();
    typed(&mut treed, "/tree");
    let tree = screen(&treed, 120, 30);
    assert!(tree.contains("● world-host") && tree.contains("● tool-fs"), "{tree}");

    // A chat rewind moves the head: the next entry does not continue the folded branch.
    let cp = s.fold().unwrap().checkpoints[0].0;
    let plan = rewind_plan(&s, cp, RewindMode::ChatOnly).await.unwrap();
    assert!(plan.fork_at.is_some() && plan.restore.is_none());
    let before = fresh.transcript.clone();
    let op = aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty());
    let restored = s.rewind(&op, cp, RewindMode::ChatOnly).await.unwrap();
    let e = s.log().get(restored).unwrap();
    assert!(!before.continues(&e), "a fork is detected so the surface refolds");
    let after = ui_from_session(&s, &r.planned).unwrap();
    assert!(after.transcript.blocks.iter().any(|b| matches!(b.body, Body::Restored { mode: RewindMode::ChatOnly, .. })));

    let json = headless(&s, &r.planned, 100, 24).unwrap();
    assert!(json["lines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("↺ rewound to c")));
}

// ───────────────────────────── slash commands ─────────────────────────────

fn typed(ui: &mut UiState, text: &str) -> Option<Action> {
    for c in text.chars() {
        on_key(ui, key(KeyCode::Char(c)));
    }
    on_key(ui, key(KeyCode::Enter))
}

#[test]
fn slash_lines_become_commands_even_mid_turn_and_double_slash_is_a_prompt() {
    let mut ui = UiState::default();
    assert_eq!(typed(&mut ui, "/plan  checking first "), Some(Action::Command { name: "plan".into(), args: "checking first".into() }));
    ui.busy = true;
    assert_eq!(typed(&mut ui, "/plan"), Some(Action::Command { name: "plan".into(), args: String::new() }), "a command is never a steer");
    assert_eq!(typed(&mut ui, "/help"), None, "/help is a view here");
    assert!(matches!(ui.overlays.pop(), Some(Overlay::Panel(p)) if p.body == PanelBody::View(View::Help)));
    assert_eq!(typed(&mut ui, "//etc/hosts is odd"), Some(Action::Steer { text: "/etc/hosts is odd".into(), urgent: false }));
    ui.busy = false;
    assert_eq!(typed(&mut ui, "//x"), Some(Action::Prompt("/x".into())));
    assert_eq!(parse_line("/"), Parsed::Command { name: "", args: "" });
    assert_eq!(parse_line("  hello /plan"), Parsed::Prompt("hello /plan"));
}

fn echo_commands() -> (Commands, std::sync::mpsc::Receiver<String>) {
    let (s, seen) = std::sync::mpsc::channel();
    let mut c = Commands::new();
    c.register(
        "echo",
        "<text>",
        "repeat the text",
        Arc::new(move |_session: &Arc<Session>, args: &str, _run| {
            s.send(args.to_string()).unwrap();
            Ok(format!("echoed {args}"))
        }),
    )
    .unwrap();
    c.register("fail", "", "always refuses", Arc::new(|_: &Arc<Session>, _: &str, _| Err("nothing to do here".to_string()))).unwrap();
    (c, seen)
}

fn kinds_after(s: &Session, before: usize) -> Vec<(Principal, EntryKind)> {
    let all = s.log().read_branch(s.log().head().unwrap()).unwrap();
    all.into_iter().skip(before).map(|e| (e.by, e.kind)).collect()
}

fn branch_len(s: &Session) -> usize {
    s.log().read_branch(s.log().head().unwrap()).unwrap().len()
}

#[tokio::test]
async fn registered_command_runs_with_its_args_is_logged_and_shows_in_the_status_line() {
    let r = rig();
    // No scripted steps: a turn reaching the model would fail the test.
    let s = boot(&r, vec![]).await;
    let (commands, seen) = echo_commands();
    let mut ui = ui_from_session(&s, &r.planned).unwrap();
    let before = branch_len(&s);
    let Some(Action::Command { name, args }) = typed(&mut ui, "/echo hi there") else { panic!("not a command") };
    crate::runtime::run_command(&mut ui, &commands, &s, &name, &args);
    assert_eq!(seen.try_iter().collect::<Vec<_>>(), ["hi there"]);
    assert_eq!(notice(&ui), "echoed hi there");
    assert!(screen(&ui, 100, 16).lines().last().unwrap().contains("echoed hi there"));
    let logged = kinds_after(&s, before);
    assert!(
        matches!(&logged[..], [
            (Principal::Operator, EntryKind::CommandRun { name, args, .. }),
            (Principal::Operator, EntryKind::CommandDone { kind, .. }),
        ] if name == "echo" && args == &["hi there"] && kind == "ok"),
        "{logged:?}"
    );

    crate::runtime::run_command(&mut ui, &commands, &s, "fail", "");
    assert!(notice(&ui).contains("`/fail`: nothing to do here"), "{}", notice(&ui));
    assert_eq!(ui.notice.as_ref().map(|n| n.tone), Some(Tone::Error));
    assert!(matches!(kinds_after(&s, before + 2).last(), Some((_, EntryKind::CommandDone { kind, .. })) if kind == "error"));
    assert_eq!(s.fold().unwrap().turns, 0, "no command started a turn");
}

#[tokio::test]
async fn unknown_command_is_refused_with_the_list_and_help_lists_every_command() {
    let r = rig();
    let s = boot(&r, vec![]).await;
    let (commands, seen) = echo_commands();
    let mut ui = ui_from_session(&s, &r.planned).unwrap();
    let before = branch_len(&s);

    crate::runtime::run_command(&mut ui, &commands, &s, "nope", "x");
    let refused = notice(&ui);
    assert!(refused.contains("`/nope` is not a command here, so nothing was sent to the model"), "{refused}");
    assert!(refused.contains("Commands: /help, /echo, /fail"), "{refused}");
    let text = screen(&ui, 100, 16);
    assert!(text.contains("Commands: /help, /echo, /fail") && text.contains("`//`"), "the refusal wraps into the footer:\n{text}");
    assert_eq!(branch_len(&s), before, "a refusal writes nothing");
    assert!(seen.try_recv().is_err(), "the handler was not called");

    // A multi-line answer opens a panel instead of crowding the hint row.
    ui.notice = None;
    crate::runtime::run_command(&mut ui, &commands, &s, "help", "");
    assert!(ui.notice.is_none());
    let Some(Overlay::Panel(p)) = ui.overlays.last() else { panic!("{:?}", ui.overlays) };
    assert_eq!(
        p.body,
        PanelBody::Text(vec!["/help: list these commands".into(), "/echo <text>: repeat the text".into(), "/fail: always refuses".into()])
    );
    let text = screen(&ui, 100, 16);
    assert!(text.contains("╭ /help ") && text.contains("/echo <text>: repeat the text"), "{text}");
    assert_eq!(branch_len(&s), before, "/help writes nothing");
    assert_eq!(s.fold().unwrap().turns, 0);
}

#[test]
fn registration_refuses_reserved_duplicate_and_malformed_names() {
    let (mut c, _) = echo_commands();
    let noop: Handler = Arc::new(|_: &Arc<Session>, _: &str, _| Ok(String::new()));
    assert!(c.register("help", "", "", noop.clone()).unwrap_err().contains("already a command"));
    assert!(c.register("echo", "", "", noop.clone()).unwrap_err().contains("already a command"));
    assert!(c.register("Bad Name", "", "", noop).unwrap_err().contains("not a command name"));
    assert_eq!(c.names().collect::<Vec<_>>(), ["echo", "fail"]);
}

// ───────────────────────────── the `/` menu ─────────────────────────────

fn with_preset(ui: &mut UiState) {
    ui.commands = vec![CommandInfo {
        name: "preset".into(),
        usage: "<name>".into(),
        help: "switch the session policy".into(),
        choices: vec![("read-only".into(), "look only".into()), ("dev".into(), "+ network".into()), ("lab".into(), "+ VMs".into())],
    }];
}

fn type_str(ui: &mut UiState, text: &str) {
    for c in text.chars() {
        on_key(ui, key(KeyCode::Char(c)));
    }
}

#[test]
fn slash_opens_the_menu_under_the_prompt_and_arrows_tab_enter_and_esc_drive_it() {
    let mut ui = UiState::default();
    with_preset(&mut ui);
    type_str(&mut ui, "/");
    let text = screen(&ui, 100, 24);
    println!("--- menu after `/` (TestBackend 100x24) ---\n{text}");
    assert!(text.contains("▸ /help") && text.contains("/tree") && text.contains("commands, views and keys"), "{text}");
    assert!(text.contains("↑↓ select · tab complete · enter run · esc close"), "{text}");

    // Narrow, move, complete: Tab puts the command in the prompt, then its values are offered.
    type_str(&mut ui, "pr");
    assert_eq!(render_frame_json(&ui, 100, 24)["menu"]["items"], json!(["/preset"]));
    assert_eq!(on_key(&mut ui, key(KeyCode::Tab)), None);
    assert_eq!(ui.local.draft, "/preset ");
    let values = screen(&ui, 100, 24);
    assert!(values.contains("▸ dev") && values.contains("lab") && values.contains("read-only"), "{values}");
    on_key(&mut ui, key(KeyCode::Down));
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), Some(Action::Command { name: "preset".into(), args: "lab".into() }));
    assert!(ui.local.draft.is_empty());

    // Enter on a name that needs its argument completes instead of running without it.
    type_str(&mut ui, "/pre");
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), None);
    assert_eq!(ui.local.draft, "/preset ");
    // Up wraps to the last value.
    on_key(&mut ui, key(KeyCode::Up));
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), Some(Action::Command { name: "preset".into(), args: "read-only".into() }));

    // Esc closes the menu for this draft; typing opens it again; Tab no longer moves focus.
    type_str(&mut ui, "/j");
    on_key(&mut ui, key(KeyCode::Esc));
    assert!(render_frame_json(&ui, 100, 24)["menu"].is_null());
    type_str(&mut ui, "o");
    assert_eq!(render_frame_json(&ui, 100, 24)["menu"]["items"], json!(["/jobs"]));
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), None, "/jobs runs from the menu: a view");
    assert_eq!(front(&ui), "panel");
}

#[test]
fn a_prompt_or_a_double_slash_line_never_opens_the_menu() {
    let mut ui = UiState::default();
    type_str(&mut ui, "fix the /tree bug");
    assert!(render_frame_json(&ui, 100, 20)["menu"].is_null());
    assert_eq!(on_key(&mut ui, key(KeyCode::Tab)), None);
    assert_eq!(ui.local.draft, "fix the /tree bug", "Tab edits nothing outside the menu");
    ui.local.draft.clear();
    ui.local.cursor = 0;
    type_str(&mut ui, "//tree");
    assert!(render_frame_json(&ui, 100, 20)["menu"].is_null());
    assert_eq!(on_key(&mut ui, key(KeyCode::Enter)), Some(Action::Prompt("/tree".into())));
}

// ───────────────────────────── inline: the scrollback ─────────────────────────────

fn user(text: &str) -> (Principal, EntryKind) {
    (Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: text.into(), client_key: None })
}

fn say(text: &str) -> (Principal, EntryKind) {
    (model(), EntryKind::AssistantMessage { content: vec![ContentBlock::text(text)], usage: Usage::default(), stop: StopReason::EndTurn })
}

fn visible(screen: &InlineScreen<TestBackend>) -> String {
    let buf = screen.backend().buffer().clone();
    buffer_lines(&buf, buf.area).join("\n")
}

#[test]
fn settled_blocks_go_to_the_scrollback_once_and_a_running_card_stays_live_until_its_result() {
    let call = CallId("c1".into());
    let entries = log_with(vec![
        user("first question"),
        say("first answer"),
        (
            model(),
            EntryKind::ToolCall {
                call: call.clone(),
                tool: "bash".into(),
                args: json!({"command": "sleep 1"}),
                class: Consequence::SpawnsProcess,
            },
        ),
        say("while it runs"),
        (
            Principal::system("tools"),
            EntryKind::ToolResult {
                call,
                outcome: ToolOutcome::Ok,
                value: json!({}),
                content: vec![ContentBlock::text("slept")],
                spill_ref: None,
                meta: None,
                late: false,
            },
        ),
    ]);
    let mut ui = UiState::default();
    ui.refold(&entries[..4], &Default::default()).unwrap();
    assert_eq!(ui.transcript.settled(), 2, "the running card holds back itself and what follows");

    let mut inline = InlineScreen::new(TestBackend::new(70, 24), 4).unwrap();
    let mut probe = Probe::default();
    inline.draw(&ui, &mut probe).unwrap();
    assert_eq!(inline.printed(), 2);
    let text = visible(&inline);
    println!("--- inline, card running (TestBackend 70x24) ---\n{text}");
    let at = |needle: &str| text.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("no `{needle}` in\n{text}"));
    assert!(at("❯ first question") < at("first answer") && at("first answer") < at("⋯ running"), "{text}");
    assert!(at("while it runs") < at("type a prompt"), "the live part sits above the prompt:\n{text}");

    ui.apply(&entries[4], &Default::default()).unwrap();
    inline.draw(&ui, &mut probe).unwrap();
    assert_eq!(inline.printed(), 4, "the result settles the card and what followed it");
    let text = visible(&inline);
    assert_eq!(text.matches("first question").count(), 1, "nothing is written twice:\n{text}");
    assert!(text.contains("✓ slept") && !text.contains("⋯ running"), "{text}");

    // A refold (fork, rewind) keeps what the scrollback already shows.
    ui.refold(&entries, &Default::default()).unwrap();
    inline.reanchor(&ui);
    assert_eq!(inline.printed(), 4);
    inline.draw(&ui, &mut probe).unwrap();
    assert_eq!(visible(&inline).matches("while it runs").count(), 1);
    assert!(probe.anomalies.is_empty(), "{:?}", probe.anomalies);
}

#[test]
fn the_inline_viewport_grows_with_the_menu_and_shrinks_back_when_it_closes() {
    let mut ui = UiState::default();
    let mut inline = InlineScreen::new(TestBackend::new(80, 30), 4).unwrap();
    let mut probe = Probe::default();
    inline.draw(&ui, &mut probe).unwrap();
    let idle = inline.height();
    type_str(&mut ui, "/");
    inline.draw(&ui, &mut probe).unwrap();
    assert!(inline.height() > idle + 4, "the menu gets rows: {} -> {}", idle, inline.height());
    assert!(visible(&inline).contains("▸ /help"));
    on_key(&mut ui, key(KeyCode::Esc));
    inline.draw(&ui, &mut probe).unwrap();
    assert_eq!(inline.height(), idle);
    assert!(!visible(&inline).contains("/help"), "the menu rows are cleared, not left behind");
}
