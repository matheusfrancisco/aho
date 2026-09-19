use super::*;
use aho_cap::BoundWorld;
use aho_policy::DecisionReason;
use serde_json::json;

fn entry(id: u64, kind: EntryKind) -> Entry {
    Entry { id: EntryId(id), parent: None, prev_hash: Hash::ZERO, hash: Hash::ZERO, at: Timestamp(0), by: Principal::Operator, kind }
}

fn terminal_call(v: &Value) -> Card {
    Card::Terminal { command: v["command"].as_str().unwrap_or("").into(), output: String::new(), exit: None }
}

fn text_printer(hud: bool) -> Printer<Vec<u8>, Vec<u8>> {
    let mut p = Printer::new(Mode::Text, Style { color: false, hud, echo_user: false }, Vec::new(), Vec::new());
    p.tools.insert("bash".into(), ToolInfo { cap: "tool-bash".into(), present: terminal_call });
    p
}

fn out(p: Printer<Vec<u8>, Vec<u8>>) -> (String, String) {
    let (o, e) = p.into_parts();
    (String::from_utf8(o).unwrap(), String::from_utf8(e).unwrap())
}

fn turn() -> Vec<SessionEvent> {
    vec![
        SessionEvent::Entry(Box::new(entry(1, EntryKind::UserMessage { form: Form::Prompt, text: "hi".into(), client_key: None }))),
        SessionEvent::Stream(StreamEvent::Thinking("hmm".into())),
        SessionEvent::Stream(StreamEvent::Text("hello ".into())),
        SessionEvent::Stream(StreamEvent::Text("there".into())),
        SessionEvent::Entry(Box::new(entry(
            2,
            EntryKind::AssistantMessage {
                content: vec![ContentBlock::Thinking { text: "hmm".into() }, ContentBlock::text("hello there")],
                usage: Usage::default(),
                stop: StopReason::EndTurn,
            },
        ))),
        SessionEvent::Idle { turn: TurnId(1), transition: Transition::Completed },
    ]
}

#[test]
fn json_mode_is_one_entry_per_line_and_nothing_else() {
    let mut p = Printer::new(Mode::Json, Style::default(), Vec::new(), Vec::new());
    for ev in turn() {
        p.event(&ev).unwrap();
    }
    p.finish().unwrap();
    let (o, e) = out(p);
    assert!(e.is_empty(), "stderr stays empty without an error: {e:?}");
    let lines: Vec<&str> = o.lines().collect();
    assert_eq!(lines.len(), 2, "stream chunks and idle are not durable: {o}");
    let parsed: Vec<Entry> = lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(parsed[0].id, EntryId(1));
    assert!(matches!(parsed[1].kind, EntryKind::AssistantMessage { .. }));
}

#[test]
fn json_mode_sends_errors_to_stderr_only() {
    let mut p = Printer::new(Mode::Json, Style::default(), Vec::new(), Vec::new());
    p.event(&SessionEvent::Error("model call failed: boom".into())).unwrap();
    let (o, e) = out(p);
    assert!(o.is_empty());
    assert!(e.contains("model call failed: boom"));
}

#[test]
fn streamed_text_is_not_printed_twice_and_replayed_entries_are_skipped() {
    let mut p = text_printer(false);
    for ev in turn() {
        p.event(&ev).unwrap();
    }
    p.event(&turn()[4]).unwrap(); // the same entry again (subscribe replay overlapping live)
    let (o, _) = out(p);
    assert_eq!(o.matches("hello there").count(), 1, "{o}");
    assert!(o.contains("hmm"));
    assert!(!o.contains("hi\n"), "the operator's own prompt is not echoed unless asked");
}

#[test]
fn unstreamed_messages_print_from_the_entry() {
    let mut p = text_printer(false);
    p.event(&turn()[4]).unwrap();
    let (o, _) = out(p);
    assert!(o.contains("hello there"));
}

#[test]
fn tool_cards_come_from_the_tool_present_fn_and_the_result_card() {
    let mut p = text_printer(false);
    let call = CallId("c1".into());
    p.entry(&entry(
        3,
        EntryKind::ToolCall {
            call: call.clone(),
            tool: "bash".into(),
            args: json!({"command": "echo hi"}),
            class: Consequence::SpawnsProcess,
        },
    ))
    .unwrap();
    let output: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    p.entry(&entry(
        4,
        EntryKind::ToolResult {
            call: call.clone(),
            outcome: ToolOutcome::Ok,
            value: Value::Null,
            content: vec![],
            spill_ref: None,
            meta: Some(Card::Terminal { command: "echo hi".into(), output, exit: Some(3) }),
            late: false,
        },
    ))
    .unwrap();
    p.entry(&entry(
        5,
        EntryKind::ToolCall { call: CallId("c2".into()), tool: "read".into(), args: json!({"path": "x"}), class: Consequence::ReadOnly },
    ))
    .unwrap();
    p.entry(&entry(
        6,
        EntryKind::ToolResult {
            call: CallId("c2".into()),
            outcome: ToolOutcome::Err { code: "NOT_FOUND".into(), reason: "x does not exist".into() },
            value: Value::Null,
            content: vec![],
            spill_ref: None,
            meta: None,
            late: true,
        },
    ))
    .unwrap();
    let (o, _) = out(p);
    assert!(o.contains("● bash $ echo hi  SpawnsProcess"), "{o}");
    assert!(o.contains("line 12") && !o.contains("line 13"), "results are capped: {o}");
    assert!(o.contains("… 8 more lines"));
    assert!(o.contains("[exit 3]"));
    assert!(o.contains("✕ read: x does not exist (NOT_FOUND)") && o.contains("(late result)"), "{o}");
}

#[test]
fn hud_names_the_capability_behind_each_tool_call() {
    let mut p = text_printer(true);
    p.entry(&entry(
        9,
        EntryKind::ToolCall {
            call: CallId("c".into()),
            tool: "bash".into(),
            args: json!({"command": "ls"}),
            class: Consequence::SpawnsProcess,
        },
    ))
    .unwrap();
    let (o, _) = out(p);
    assert!(o.contains("[#9 ToolCall by operator] cap tool-bash"), "{o}");
}

#[test]
fn approval_card_is_a_closed_box_with_the_answer_keys() {
    let card = ApprovalCard {
        tool: "write".into(),
        class: Consequence::WorkspaceWrite,
        args: json!({"path": "src/main.rs", "content": "x".repeat(300)}),
        reason: DecisionReason::Rule { origin: "./aho.toml:3".into(), rule: "ask write(*)".into() },
        world: BoundWorld::host(),
        suggested_cut: Some("preset dev".into()),
        rendered: "`write` needs approval by rule `ask write(*)` from ./aho.toml:3; the user is asked before it runs.".into(),
    };
    let s = render_approval(&card, Some("tool-fs"));
    let widths: Vec<usize> = s.lines().map(|l| l.chars().count()).collect();
    assert!(widths.iter().all(|w| *w == CARD_W), "every row has the same width: {widths:?}\n{s}");
    assert!(s.contains("tool-fs.write") && s.contains("class: WorkspaceWrite"));
    assert!(s.contains("reason: Rule"));
    assert!(s.contains("[p] aho preset dev"), "a preset cut is offered as an answer key: {s}");
    assert!(s.contains("[y] allow once") && s.contains("[Esc] cancel"));
}
