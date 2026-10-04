//! Abort keys (`tui.keys.*`), the pinned jobs panel and the scope column of `/jobs`.

use aho_approval::Decided;
use aho_cap::{BoundWorld, Config};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_log::{CancelReason, OpKind, OpStatus};
use aho_policy::{ApprovalCard, DecisionReason};
use aho_proc::OpInfo;
use aho_session::AbortScope;
use aho_types::*;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

use crate::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn screen(ui: &UiState, w: u16, h: u16) -> String {
    let buf = render_to_buffer(ui, w, h);
    buffer_lines(&buf, buf.area).join("\n")
}

fn card() -> ApprovalCard {
    ApprovalCard {
        tool: "bash".into(),
        class: Consequence::SpawnsProcess,
        args: json!({"command": "make"}),
        reason: DecisionReason::Preset("dev".into()),
        world: BoundWorld { name: "host".into(), confines: false, enforced_by: "nothing".into() },
        suggested_cut: None,
        rendered: "preset `dev` asks for SpawnsProcess".into(),
    }
}

fn loadout(toml: &str) -> aho_config::Loadout {
    let mut schema = Schema::default();
    for k in aho_cap::core_keys().into_iter().chain(TuiConfig::keys()) {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: toml.into() });
    resolve(&inputs, &schema, false).unwrap()
}

#[test]
fn abort_all_acts_anywhere_and_esc_aborts_the_turn_only_when_nothing_in_front_used_it() {
    let mut ui = UiState::default();
    assert_eq!(on_key(&mut ui, ctrl('c')), Some(Action::Abort(AbortScope::All)), "idle: jobs may still run");
    assert_eq!(on_key(&mut ui, key(KeyCode::Esc)), None, "idle with nothing open, Esc aborts nothing");

    ui.busy = true;
    assert_eq!(on_key(&mut ui, key(KeyCode::Esc)), Some(Action::Abort(AbortScope::Turn)));

    // The `/` menu takes Esc first: it closes, nothing is aborted.
    on_key(&mut ui, key(KeyCode::Char('/')));
    assert!(render_frame_json(&ui, 100, 24)["menu"].is_object());
    assert_eq!(on_key(&mut ui, key(KeyCode::Esc)), None);
    assert!(render_frame_json(&ui, 100, 24)["menu"].is_null());
    assert_eq!(on_key(&mut ui, key(KeyCode::Esc)), Some(Action::Abort(AbortScope::Turn)), "the next Esc reaches the turn");
    ui.local.draft.clear();
    ui.local.cursor = 0;

    // An overlay takes Esc (it closes); Ctrl+C still aborts everything with it open.
    ui.overlays.push(Overlay::Panel(Panel::view(View::Jobs)));
    assert_eq!(on_key(&mut ui, ctrl('c')), Some(Action::Abort(AbortScope::All)));
    assert_eq!(front(&ui), "panel", "abort-all leaves the overlay open");
    assert_eq!(on_key(&mut ui, key(KeyCode::Esc)), None);
    assert_eq!(front(&ui), "none");

    // The approval card takes Esc as its cancel answer.
    ui.approval = Some(ApprovalView::new(&card(), None, &ui.state));
    assert_eq!(on_key(&mut ui, key(KeyCode::Esc)), Some(Action::Answer(Decided::cancelled())));
    assert_eq!(on_key(&mut ui, ctrl('c')), Some(Action::Abort(AbortScope::All)));
}

#[test]
fn abort_keys_come_from_config_and_replace_the_defaults() {
    assert_eq!(Keymap::from_loadout(&loadout("")).unwrap(), Keymap::default());

    let keys = Keymap::from_loadout(&loadout("[tui.keys]\nabort_turn = \"ctrl+x\"\nabort_all = \"F2\"\n")).unwrap();
    let mut ui = UiState::default();
    ui.local.keys = keys;
    ui.busy = true;
    assert_eq!(on_key(&mut ui, key(KeyCode::Esc)), None, "Esc no longer aborts");
    assert_eq!(on_key(&mut ui, ctrl('x')), Some(Action::Abort(AbortScope::Turn)));
    assert_eq!(on_key(&mut ui, ctrl('c')), None, "Ctrl+C no longer aborts");
    assert_eq!(on_key(&mut ui, key(KeyCode::F(2))), Some(Action::Abort(AbortScope::All)));
    let hints = screen(&ui, 120, 12);
    assert!(hints.contains("ctrl-x abort turn · f2 abort all"), "{hints}");
    ui.busy = false;
    assert_eq!(on_key(&mut ui, ctrl('x')), None, "the abort-turn key does nothing while idle");
}

#[test]
fn a_value_that_is_not_a_key_is_refused_naming_its_key() {
    let bad = loadout("[tui.keys]\nabort_all = \"hyper+q\"\n");
    let err = Keymap::from_loadout(&bad).unwrap_err().to_string();
    assert!(err.contains("tui.keys.abort_all"), "{err}");
    assert!(TuiConfig::from_loadout(&bad).is_err(), "the surface refuses to build with it");

    for ok in ["esc", "Ctrl+C", "ctrl+x", "alt+k", "f2", "shift+tab", "space"] {
        assert!(KeySpec::parse(ok).is_some(), "{ok}");
    }
    for bad in ["", "ctrl+", "ctrl+ctrl+c", "f0", "f25", "shift+a", "escc", "super+k"] {
        assert_eq!(KeySpec::parse(bad), None, "{bad:?}");
    }
}

fn op(n: u64, call: &str, tool: &str, detached: bool, status: Option<OpStatus>, age_ms: u64) -> OpInfo {
    OpInfo {
        op: OpId(n),
        call: CallId(call.into()),
        tool: tool.into(),
        kind: OpKind::Process,
        status,
        started: Timestamp(Timestamp::now().0.saturating_sub(age_ms)),
        identity: None,
        detached,
        delivered: false,
    }
}

fn with_calls() -> UiState {
    let mut ui = UiState::default();
    let calls = [
        ("c7", "subagent", json!({"prompt": "find the parser", "label": "explore\nparser"})),
        ("c8", "bash", json!({"command": "cargo test"})),
        ("c9", "bash", json!({"command": "ls"})),
    ];
    for (call, tool, args) in calls {
        ui.state.calls.insert(CallId(call.into()), (tool.into(), Consequence::SpawnsProcess, args));
    }
    ui
}

#[test]
fn the_jobs_panel_pins_running_jobs_above_the_prompt_and_hides_without_one() {
    let mut ui = with_calls();
    let finished = op(8, "c8", "bash", true, Some(OpStatus::Completed { code: 0 }), 5_000);
    let turn_op = op(9, "c9", "bash", false, None, 1_000);
    ui.set_jobs(vec![op(7, "c7", "subagent", true, None, 12_400), finished.clone(), turn_op.clone()]);

    let v = render_frame_json(&ui, 100, 20);
    assert_eq!(v["jobs"], json!({"running": 1, "rows": [{"op": 7, "tool": "subagent", "label": "explore parser", "elapsed": "12s"}]}));
    let lines: Vec<&str> = v["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
    let prompt = lines.iter().position(|l| l.contains('╭')).unwrap();
    assert_eq!(lines[prompt - 2], " jobs · 1 running · /jobs lists every op");
    assert_eq!(lines[prompt - 1], "  ⋯ op7 · subagent · 12s · explore parser");
    let inline_with = inline_height(&ui, 0, 80, 30);

    // Only a finished job and a turn op: no panel, no rows taken.
    ui.set_jobs(vec![finished, turn_op]);
    let v = render_frame_json(&ui, 100, 20);
    assert!(v["jobs"].is_null());
    assert!(!v["areas"].as_array().unwrap().iter().any(|a| a["area"] == "jobs"));
    assert!(!screen(&ui, 100, 20).contains("jobs ·"));
    assert_eq!(inline_height(&ui, 0, 80, 30), inline_with - 2, "the inline viewport gives the panel's rows back");
    assert!(ui.job_labels.is_empty(), "labels are kept for running jobs only");
}

#[test]
fn the_jobs_panel_caps_its_rows_so_the_transcript_keeps_the_screen() {
    let mut ui = UiState::default();
    ui.set_jobs((1..=6).map(|n| op(n, &format!("c{n}"), "bash", true, None, 185_000)).collect());
    let v = render_frame_json(&ui, 100, 24);
    let panel = v["areas"].as_array().unwrap().iter().find(|a| a["area"] == "jobs").cloned().unwrap();
    assert_eq!(panel["h"], 6, "header, four rows, one overflow row");
    let text = screen(&ui, 100, 24);
    assert!(text.contains(" jobs · 6 running"), "{text}");
    assert!(text.contains("  ⋯ op4 · bash · 3m05s") && !text.contains("op5 ·"), "{text}");
    assert!(text.contains("  …and 2 more"), "{text}");
}

#[test]
fn elapsed_reads_seconds_then_minutes_then_hours() {
    for (ms, want) in [(0, "0s"), (59_999, "59s"), (60_000, "1m00s"), (184_000, "3m04s"), (3_599_999, "59m59s"), (3_720_000, "1h02m")] {
        assert_eq!(crate::jobs::elapsed(ms), want, "{ms}");
    }
}

#[test]
fn slash_jobs_lists_every_op_with_its_scope_and_status() {
    let mut ui = with_calls();
    ui.set_jobs(vec![
        op(7, "c7", "subagent", true, None, 3_000),
        op(8, "c8", "bash", true, Some(OpStatus::Cancelled { reason: CancelReason::Aborted }), 0),
        op(9, "c9", "bash", false, Some(OpStatus::Completed { code: 0 }), 0),
    ]);
    ui.overlays.push(Overlay::Panel(Panel::view(View::Jobs)));
    let text = screen(&ui, 100, 24);
    assert!(text.contains("op7 job  subagent running 3s"), "{text}");
    assert!(text.contains("op8 job  bash ⊘ aborted"), "{text}");
    assert!(text.contains("op9 turn bash ✓ exit 0"), "{text}");
}
