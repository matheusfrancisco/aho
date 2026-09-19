//! Drawing is a pure function of [`UiState`]: [`draw`] fills a `Buffer`, records each area it
//! laid out on a [`Probe`], and asserts layout invariants with [`Probe::check`]. The runtime and
//! the tests (ratatui's `TestBackend`) call the same function.

use aho_inspect::{TreeNode, glyph, render_classes, state_name};
use aho_log::Verdict;
use aho_log::{CapState, OpStatus};
use aho_types::{Consequence, Timestamp};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};
use serde_json::{Value, json};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::frames::{Frame, Probe, buffer_lines};
use crate::model::{
    ApprovalView, Body, Focus, MODES, ResultStatus, RewindPicker, TABS, Tab, ToolCard, UiState, compact, mode_doc, mode_name,
    principal_label,
};

pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;
const SIDEBAR_MIN_TOTAL: u16 = 72;

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Renders `ui` into a fresh `w`×`h` buffer. Pure: same state, same cells.
pub fn render_to_buffer(ui: &UiState, w: u16, h: u16) -> Buffer {
    capture(ui, w, h).0
}

/// Renders and captures the frame record (rects, text per area, anomalies).
pub fn capture(ui: &UiState, w: u16, h: u16) -> (Buffer, Frame) {
    let started = std::time::Instant::now();
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    let mut probe = Probe::default();
    draw(ui, area, &mut buf, &mut probe);
    let ms = started.elapsed().as_millis() as u64;
    let frame = Frame::from_buffer(&buf, probe, ms);
    (buf, frame)
}

/// `aho tui --headless`: normalized screen JSON (rows trimmed, no styles) plus the frame record.
pub fn render_frame_json(ui: &UiState, w: u16, h: u16) -> Value {
    let (buf, frame) = capture(ui, w, h);
    json!({
        "v": 1,
        "width": w,
        "height": h,
        "lines": buffer_lines(&buf, buf.area),
        "areas": frame.rects.iter().zip(frame.text.iter()).map(|((n, x, y, rw, rh), (_, t))| json!({"area": n, "x": x, "y": y, "w": rw, "h": rh, "text": t})).collect::<Vec<_>>(),
        "anomalies": frame.anomalies,
        "focus": format!("{:?}", ui.local.focus).to_lowercase(),
        "tab": ui.local.tab.name(),
        "modal": if ui.approval.is_some() { "approval" } else if ui.rewind.is_some() { "rewind" } else { "none" },
    })
}

/// Draws the whole screen; returns the terminal cursor position (input line) if it should show.
pub fn draw(ui: &UiState, area: Rect, buf: &mut Buffer, probe: &mut Probe) -> Option<(u16, u16)> {
    probe.check(
        area.width >= MIN_WIDTH && area.height >= MIN_HEIGHT,
        format!("terminal is {}x{}, smaller than {MIN_WIDTH}x{MIN_HEIGHT}; panes are clipped", area.width, area.height),
    );
    let notice = ui.notice.as_deref().map(|n| notice_rows(n, area.width));
    let footer_h = notice.as_ref().map_or(1, |rows| rows.len().clamp(1, FOOTER_MAX)) as u16;
    let [header, body, input, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(3), Constraint::Length(footer_h)]).areas(area);
    probe.area("header", header);
    draw_header(ui, header, buf);

    let (main, side) = if body.width >= SIDEBAR_MIN_TOTAL {
        let side_w = (body.width / 3).clamp(30, 48);
        let [m, s] = Layout::horizontal([Constraint::Min(40), Constraint::Length(side_w)]).areas(body);
        (m, Some(s))
    } else {
        (body, None)
    };
    probe.area("transcript", main);
    draw_transcript(ui, main, buf, probe);
    if let Some(s) = side {
        probe.area("sidebar", s);
        draw_sidebar(ui, s, buf);
    }
    probe.area("input", input);
    let cursor = draw_input(ui, input, buf, probe);
    probe.area("footer", footer);
    draw_footer(ui, notice.as_deref(), footer, buf);

    if let Some(card) = &ui.approval {
        draw_approval(card, ui.approvals_queued, area, buf, probe);
        return None;
    }
    if let Some(p) = &ui.rewind {
        draw_rewind(p, area, buf, probe);
        return None;
    }
    cursor
}

fn draw_header(ui: &UiState, area: Rect, buf: &mut Buffer) {
    let busy =
        if ui.is_busy() { Span::styled(" ● running ", Style::default().fg(Color::Yellow)) } else { Span::styled(" ○ idle ", dim()) };
    let p = &ui.plan;
    let mut spans = vec![
        Span::styled(format!(" {} ", p.label), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            "· plan {} · preset {} (epoch {}) · world {} ",
            p.hash, ui.state.policy.preset, ui.state.policy.epoch.0, p.world
        )),
        busy,
    ];
    if ui.local.hud {
        spans.push(Span::styled(
            format!("· HUD · preset ← {} · enforced by {}", p.preset_origin, p.enforced_by),
            Style::default().fg(Color::Magenta),
        ));
    }
    buf.set_style(area, Style::default().bg(Color::Black));
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);
}

// ───────────────────────────── transcript ─────────────────────────────

/// Word wrap by display width; long words break hard.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.replace('\t', "    ").split('\n') {
        let mut line = String::new();
        let mut w = 0usize;
        for word in raw.split_inclusive(' ') {
            let ww = UnicodeWidthStr::width(word);
            if w + ww > width && w > 0 {
                out.push(line.trim_end().to_string());
                line.clear();
                w = 0;
            }
            if ww > width {
                for ch in word.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if w + cw > width && w > 0 {
                        out.push(std::mem::take(&mut line));
                        w = 0;
                    }
                    line.push(ch);
                    w += cw;
                }
            } else {
                line.push_str(word);
                w += ww;
            }
        }
        out.push(line.trim_end().to_string());
    }
    out
}

/// Cuts `s` to `width` display columns, ending in `…` when cut.
pub fn trunc(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

type Row = (Line<'static>, Option<usize>);

fn push_wrapped(rows: &mut Vec<Row>, block: Option<usize>, prefix: &str, text: &str, width: usize, style: Style, prefix_style: Style) {
    let pw = UnicodeWidthStr::width(prefix);
    let pad = " ".repeat(pw);
    for (i, l) in wrap(text, width.saturating_sub(pw)).into_iter().enumerate() {
        let p = if i == 0 { prefix.to_string() } else { pad.clone() };
        rows.push((Line::from(vec![Span::styled(p, prefix_style), Span::styled(l, style)]), block));
    }
}

fn class_name(c: Option<Consequence>) -> &'static str {
    c.map_or("unclassified", |c| c.name())
}

fn tool_rows(rows: &mut Vec<Row>, i: usize, c: &ToolCard, width: usize) {
    let title = match &c.cap {
        Some(cap) => format!("{cap}.{}", c.tool),
        None => c.tool.clone(),
    };
    let (status, style) = match &c.result {
        None => (
            match c.op {
                Some(op) => format!("⋯ running (op {})", op.0),
                None => "⋯ running".to_string(),
            },
            Style::default().fg(Color::Yellow),
        ),
        Some(r) => {
            let late = if r.late { " (late)" } else { "" };
            match r.status {
                ResultStatus::Ok => (format!("✓ {}{late}", r.summary), Style::default().fg(Color::Green)),
                ResultStatus::Err => (format!("✕ {}{late}", r.summary), Style::default().fg(Color::Red)),
                ResultStatus::Cancelled => (format!("⊘ {}{late}", r.summary), dim()),
            }
        }
    };
    let head = format!("╭ {title} · {} ─ ", class_name(c.class));
    let rest = width.saturating_sub(UnicodeWidthStr::width(head.as_str()));
    rows.push((Line::from(vec![Span::styled(head, Style::default().fg(Color::Cyan)), Span::styled(trunc(&status, rest), style)]), Some(i)));
    let bar = || Span::styled("│ ", Style::default().fg(Color::Cyan));
    if !c.args.is_null() {
        rows.push((Line::from(vec![bar(), Span::raw(compact(&c.args, width.saturating_sub(2)))]), Some(i)));
    }
    if let Some(r) = &c.result {
        for l in &r.body {
            rows.push((Line::from(vec![bar(), Span::styled(trunc(l, width.saturating_sub(2)), dim())]), Some(i)));
        }
    }
    if let Some(d) = &c.decision {
        rows.push((
            Line::from(vec![bar(), Span::styled(trunc(&format!("⚑ {d}"), width.saturating_sub(2)), Style::default().fg(Color::Magenta))]),
            Some(i),
        ));
    }
    rows.push((Line::from(Span::styled("╰", Style::default().fg(Color::Cyan))), Some(i)));
}

fn transcript_rows(ui: &UiState, width: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let selected = if ui.local.focus == Focus::Transcript { ui.local.selected } else { None };
    for (i, b) in ui.transcript.blocks.iter().enumerate() {
        let start = rows.len();
        match &b.body {
            Body::User(t) => push_wrapped(
                &mut rows,
                Some(i),
                "you ▸ ",
                t,
                width,
                Style::default(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Body::Assistant(t) => push_wrapped(&mut rows, Some(i), "", t, width, Style::default(), Style::default()),
            Body::Thinking(t) => push_wrapped(&mut rows, Some(i), "thinking ▸ ", t, width, dim().add_modifier(Modifier::ITALIC), dim()),
            Body::Notice(t) => push_wrapped(&mut rows, Some(i), "· ", t, width, dim(), dim()),
            Body::Error(t) => {
                push_wrapped(&mut rows, Some(i), "✕ ", t, width, Style::default().fg(Color::Red), Style::default().fg(Color::Red))
            }
            Body::Tool(c) => tool_rows(&mut rows, i, c, width),
            Body::Checkpoint { id, cause, turn } => {
                let style = Style::default().fg(Color::Magenta);
                rows.push((
                    Line::from(Span::styled(trunc(&format!("◆ checkpoint c{} · turn {turn} · {cause}", id.0), width), style)),
                    Some(i),
                ));
                if selected == Some(i) {
                    rows.push((Line::from(Span::styled(trunc("  [enter] rewind: both · chat · code", width), style)), Some(i)));
                }
            }
            Body::Restored { from, to, mode, changed, deleted } => {
                let t =
                    format!("↺ rewound to c{} ({}) · {changed} changed, {deleted} deleted · undo with c{}", to.0, mode_name(*mode), from.0);
                push_wrapped(&mut rows, Some(i), "", &t, width, Style::default().fg(Color::Magenta), Style::default());
            }
        }
        if ui.local.hud {
            let mut hud = format!("  ↳ e{} · by {}", b.entry.0, principal_label(&b.by));
            if let Body::Tool(c) = &b.body {
                hud.push_str(&format!(" · cap {} · class {}", c.cap.as_deref().unwrap_or("?"), class_name(c.class)));
                if let Some(r) = &c.result {
                    hud.push_str(&format!(" · result e{}", r.entry.0));
                }
            }
            rows.push((
                Line::from(Span::styled(trunc(&hud, width), Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM))),
                Some(i),
            ));
        }
        if selected == Some(i) {
            for r in rows.iter_mut().skip(start) {
                r.0 = r.0.clone().patch_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
    let t = &ui.transcript;
    if !t.live_thinking.is_empty() {
        push_wrapped(&mut rows, None, "thinking ▸ ", &t.live_thinking, width, dim().add_modifier(Modifier::ITALIC), dim());
    }
    if !t.live_text.is_empty() {
        push_wrapped(&mut rows, None, "", &t.live_text, width, Style::default(), Style::default());
    }
    for name in &t.live_tools {
        rows.push((Line::from(Span::styled(trunc(&format!("⋯ calling {name}"), width), Style::default().fg(Color::Yellow))), None));
    }
    rows
}

fn draw_transcript(ui: &UiState, area: Rect, buf: &mut Buffer, probe: &mut Probe) {
    let focused = ui.local.focus == Focus::Transcript;
    let block = Block::bordered().title(" transcript ").border_style(if focused { Style::default().fg(Color::Cyan) } else { dim() });
    let inner = block.inner(area);
    block.render(area, buf);
    probe.check(
        ui.local.selected.is_none_or(|s| s < ui.transcript.blocks.len()),
        format!("selected block {:?} is past the transcript ({} blocks)", ui.local.selected, ui.transcript.blocks.len()),
    );
    let rows = transcript_rows(ui, inner.width as usize);
    let h = inner.height as usize;
    let max_top = rows.len().saturating_sub(h);
    let mut top = max_top.saturating_sub(ui.local.scroll.min(max_top));
    if focused && let Some(sel) = ui.local.selected {
        let first = rows.iter().position(|r| r.1 == Some(sel));
        let last = rows.iter().rposition(|r| r.1 == Some(sel));
        if let (Some(f), Some(l)) = (first, last) {
            if f < top {
                top = f;
            } else if l >= top + h {
                top = (l + 1).saturating_sub(h).min(f);
            }
        }
    }
    if rows.is_empty() {
        buf.set_line(inner.x, inner.y, &Line::from(Span::styled("Type a prompt below and press Enter.", dim())), inner.width);
    }
    for (y, (line, _)) in rows.iter().skip(top).take(h).enumerate() {
        buf.set_line(inner.x, inner.y + y as u16, line, inner.width);
    }
}

// ───────────────────────────── sidebar ─────────────────────────────

fn state_color(s: &CapState) -> Color {
    match s {
        CapState::Active => Color::Green,
        CapState::Unlocked => Color::Blue,
        CapState::Locked { .. } => Color::Yellow,
        CapState::Broken { .. } => Color::Red,
        CapState::Disabled { .. } => Color::DarkGray,
    }
}

fn tree_rows(nodes: &[TreeNode], depth: usize, width: usize, out: &mut Vec<Line<'static>>) {
    for n in nodes {
        let indent = "  ".repeat(depth);
        let name_w = width.saturating_sub(indent.len() + 2 + 10).max(8);
        out.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(format!("{} ", glyph(&n.state)), Style::default().fg(state_color(&n.state))),
            Span::raw(format!("{:<w$} ", trunc(&n.id, name_w), w = name_w)),
            Span::styled(state_name(&n.state), Style::default().fg(state_color(&n.state))),
        ]));
        let sub = format!("{indent}    ");
        let sub_w = width.saturating_sub(sub.len());
        for m in &n.missing {
            out.push(Line::from(Span::styled(format!("{sub}{}", trunc(&m.requirement, sub_w)), dim())));
            if let Some(f) = &m.fix {
                out.push(Line::from(Span::styled(format!("{sub}fix: {}", trunc(f, sub_w.saturating_sub(5))), dim())));
            }
        }
        if let CapState::Broken { err } = &n.state {
            out.push(Line::from(Span::styled(format!("{sub}{}", trunc(err, sub_w)), Style::default().fg(Color::Red))));
        }
        if matches!(n.state, CapState::Unlocked | CapState::Disabled { .. })
            && let Some(a) = &n.action
        {
            out.push(Line::from(Span::styled(format!("{sub}{}", trunc(a, sub_w)), dim())));
        }
        if let Some(o) = &n.over_preset {
            out.push(Line::from(Span::styled(
                format!("{sub}! {}", trunc(&o.requirement, sub_w.saturating_sub(2))),
                Style::default().fg(Color::Yellow),
            )));
        }
        tree_rows(&n.children, depth + 1, width, out);
    }
}

fn sidebar_rows(ui: &UiState, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    match ui.local.tab {
        Tab::Tree => tree_rows(&ui.tree, 0, width, &mut out),
        Tab::Inventory => {
            for c in &ui.inventory {
                out.push(Line::from(vec![
                    Span::styled(format!("{} ", glyph(&c.state)), Style::default().fg(state_color(&c.state))),
                    Span::styled(trunc(&c.id, width.saturating_sub(2)), Style::default().add_modifier(Modifier::BOLD)),
                ]));
                let origin = match &c.source {
                    Some(s) => format!("{} ({s})", c.origin),
                    None => c.origin.clone(),
                };
                out.push(Line::from(Span::styled(trunc(&format!("  {} · {} · {}", c.kind, origin, state_name(&c.state)), width), dim())));
                if c.classes.0 != 0 {
                    out.push(Line::from(Span::raw(trunc(&format!("  consequences: {}", render_classes(c.classes)), width))));
                }
                if let Some(e) = &c.equip {
                    out.push(Line::from(Span::styled(trunc(&format!("  equip: {e}"), width), dim())));
                }
                if let Some(f) = &c.fix {
                    out.push(Line::from(Span::styled(trunc(&format!("  fix: {f}"), width), Style::default().fg(Color::Yellow))));
                }
            }
        }
        Tab::Jobs => {
            if ui.jobs.is_empty() {
                out.push(Line::from(Span::styled("no operations yet", dim())));
            }
            let now = Timestamp::now();
            for j in &ui.jobs {
                let (status, style) = match &j.status {
                    None => (format!("running {}s", now.0.saturating_sub(j.started.0) / 1000), Style::default().fg(Color::Yellow)),
                    Some(OpStatus::Completed { code }) => (format!("✓ exit {code}"), Style::default().fg(Color::Green)),
                    Some(OpStatus::Failed { err }) => (format!("✕ {err}"), Style::default().fg(Color::Red)),
                    Some(OpStatus::Cancelled { reason }) => (format!("⊘ {reason:?}").to_lowercase(), dim()),
                    Some(OpStatus::Uncertain { .. }) => ("? uncertain".to_string(), Style::default().fg(Color::Magenta)),
                };
                let head = format!("op{} {} ", j.op.0, j.tool);
                let rest = width.saturating_sub(UnicodeWidthStr::width(head.as_str()));
                out.push(Line::from(vec![Span::raw(head), Span::styled(trunc(&status, rest), style)]));
            }
        }
        Tab::Invariants => {
            let rows = ui.invariant_rows();
            if rows.is_empty() {
                out.push(Line::from(Span::styled("no invariants checked in this session", dim())));
            }
            for r in rows {
                let (v, style) = match &r.verdict {
                    Verdict::Pass => ("Pass".to_string(), Style::default().fg(Color::Green)),
                    Verdict::Fail { counterexample } => {
                        (format!("Fail ✕ (cx {})", counterexample.short()), Style::default().fg(Color::Red))
                    }
                    Verdict::Unknown { reason } => (format!("Unknown ({reason})"), Style::default().fg(Color::Yellow)),
                    Verdict::Stale => ("Stale".to_string(), Style::default().fg(Color::Yellow)),
                    Verdict::Vacuous => ("Vacuous ⚠".to_string(), Style::default().fg(Color::Yellow)),
                };
                let head = format!("{} ", trunc(&r.id, width.saturating_sub(12)));
                out.push(Line::from(vec![Span::raw(head), Span::styled(v, style)]));
                if r.stale {
                    out.push(Line::from(Span::styled(trunc("  code changed since the check", width), dim())));
                }
            }
        }
    }
    out
}

fn draw_sidebar(ui: &UiState, area: Rect, buf: &mut Buffer) {
    let focused = ui.local.focus == Focus::Sidebar;
    let block = Block::bordered().border_style(if focused { Style::default().fg(Color::Cyan) } else { dim() });
    let inner = block.inner(area);
    block.render(area, buf);
    let mut tabs: Vec<Span> = Vec::new();
    for (i, t) in TABS.iter().enumerate() {
        if i > 0 {
            tabs.push(Span::styled("│", dim()));
        }
        let style = if *t == ui.local.tab { Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { dim() };
        tabs.push(Span::styled(t.name().to_string(), style));
    }
    let tabs = Line::from(tabs);
    let tabs = if tabs.width() <= inner.width as usize {
        tabs
    } else {
        Line::from(Span::styled(format!("◂ {} ▸", ui.local.tab.name()), Style::default().add_modifier(Modifier::BOLD)))
    };
    buf.set_line(inner.x, inner.y, &tabs, inner.width);
    let body = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
    let rows = sidebar_rows(ui, body.width as usize);
    let top = ui.local.sidebar_scroll.min(rows.len().saturating_sub(1));
    for (y, line) in rows.iter().skip(top).take(body.height as usize).enumerate() {
        buf.set_line(body.x, body.y + y as u16, line, body.width);
    }
}

// ───────────────────────────── input & footer ─────────────────────────────

fn draw_input(ui: &UiState, area: Rect, buf: &mut Buffer, probe: &mut Probe) -> Option<(u16, u16)> {
    let focused = ui.local.focus == Focus::Input;
    let title = if ui.is_busy() { " steer (turn running · alt-enter = urgent) " } else { " prompt " };
    let block = Block::bordered().title(title).border_style(if focused { Style::default().fg(Color::Cyan) } else { dim() });
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    let chars: Vec<char> = ui.local.draft.chars().collect();
    let cursor = ui.local.cursor.min(chars.len());
    let avail = inner.width.saturating_sub(1) as usize;
    let before_w: usize = chars[..cursor].iter().map(|c| c.width().unwrap_or(0)).sum();
    let mut start = 0usize;
    let mut w = before_w;
    while w > avail && start < cursor {
        w -= chars[start].width().unwrap_or(0);
        start += 1;
    }
    let visible: String = chars[start..].iter().collect();
    buf.set_line(inner.x, inner.y, &Line::from(trunc(&visible, inner.width as usize)), inner.width);
    let cx = inner.x + w as u16;
    probe.check(cx < inner.x + inner.width, format!("input cursor column {cx} is outside the input box"));
    focused.then_some((cx.min(inner.right().saturating_sub(1)), inner.y))
}

/// Rows the status line may grow to for a long or multi-line notice (a `/help` listing, a refusal).
const FOOTER_MAX: usize = 6;

/// The notice wrapped to the footer width (one leading space per row).
fn notice_rows(notice: &str, width: u16) -> Vec<String> {
    wrap(notice, (width as usize).saturating_sub(1))
}

fn draw_footer(ui: &UiState, notice: Option<&[String]>, area: Rect, buf: &mut Buffer) {
    if let Some(rows) = notice {
        let style = Style::default().fg(Color::Yellow);
        for (i, text) in rows.iter().take(area.height as usize).enumerate() {
            buf.set_line(area.x, area.y + i as u16, &Line::from(Span::styled(format!(" {text}"), style)), area.width);
        }
        return;
    }
    let keys = match ui.local.focus {
        Focus::Input if ui.is_busy() => " enter steer · alt-enter urgent steer · ctrl-c abort · tab focus · alt-h HUD · ctrl-q quit",
        Focus::Input => " enter prompt · /help commands · tab focus · alt-1..4 sidebar · alt-h HUD · ctrl-q quit",
        Focus::Transcript => " ↑↓ select · enter on ◆ rewind · pgup/pgdn scroll · esc input · alt-h HUD",
        Focus::Sidebar => " ←→ tab · ↑↓ scroll · esc input · alt-h HUD",
    };
    buf.set_line(area.x, area.y, &Line::from(Span::styled(keys, dim())), area.width);
}

// ───────────────────────────── modals ─────────────────────────────

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// The doc-05 approval card.
fn draw_approval(card: &ApprovalView, queued: usize, area: Rect, buf: &mut Buffer, probe: &mut Probe) {
    let width = area.width.saturating_sub(2).min(78);
    let inner_w = width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let title = card.title();
    let cls = format!("class: {}", card.class.name());
    let gap = inner_w.saturating_sub(1 + UnicodeWidthStr::width(title.as_str()) + cls.len()).max(2);
    lines.push(Line::from(vec![
        Span::styled(format!(" {title}"), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(gap)),
        Span::styled(cls, Style::default().fg(Color::Yellow)),
    ]));
    for (i, l) in wrap(&card.args, inner_w.saturating_sub(7)).into_iter().take(3).enumerate() {
        lines.push(Line::from(format!(" {}{l}", if i == 0 { "args: " } else { "      " })));
    }
    let why = format!("{}      (reason: {})", card.why, card.reason);
    for (i, l) in wrap(&why, inner_w.saturating_sub(7)).into_iter().take(4).enumerate() {
        lines.push(Line::from(format!(" {}{l}", if i == 0 { "why:  " } else { "      " })));
    }
    let world = if card.enforced_by.is_empty() { card.world.clone() } else { format!("{} ({})", card.world, card.enforced_by) };
    lines.push(Line::from(format!(" world: {world}  ·  lease: {}", card.lease)));
    lines.push(Line::from(""));
    let key = |k: &str| Span::styled(k.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    lines.push(Line::from(vec![
        Span::raw(" "),
        key("[a]"),
        Span::raw(" allow once   "),
        key("[s]"),
        Span::raw(format!(" allow for session → opens epoch {}", card.epoch_next)),
    ]));
    // `[r]`/`[Esc]` must always be visible: share the `[p]` row only when everything fits,
    // otherwise the cut gets its own (truncated) row.
    let refuse = [key("[r]"), Span::raw(" reject        "), key("[Esc]"), Span::raw(" cancel")];
    match &card.cut {
        Some(cut) => {
            let p_text = format!(" aho {cut:<20}  ");
            let shared = Line::from([vec![Span::raw(" "), key("[p]"), Span::raw(p_text.clone())], refuse.to_vec()].concat());
            if shared.width() <= inner_w {
                lines.push(shared);
            } else {
                let p_room = inner_w.saturating_sub(4);
                lines.push(Line::from(vec![Span::raw(" "), key("[p]"), Span::raw(trunc(&format!(" aho {cut}"), p_room))]));
                lines.push(Line::from([vec![Span::raw(" ")], refuse.to_vec()].concat()));
            }
        }
        None => lines.push(Line::from([vec![Span::raw(" ")], refuse.to_vec()].concat())),
    }
    probe.check(inner_w < 40 || lines.iter().all(|l| l.width() <= inner_w), "an approval card row is wider than the card and is cut off");
    if queued > 0 {
        lines.push(Line::from(Span::styled(format!(" (+{queued} more waiting)"), dim())));
    }
    let height = lines.len() as u16 + 2;
    let rect = centered(area, width, height);
    probe.area("approval", rect);
    probe.check(
        height <= area.height && width >= 40,
        format!("approval card needs {width}x{height}; the terminal is {}x{}", area.width, area.height),
    );
    Clear.render(rect, buf);
    let block = Block::bordered().title(" approval ").border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(rect);
    block.render(rect, buf);
    for (y, l) in lines.iter().take(inner.height as usize).enumerate() {
        buf.set_line(inner.x, inner.y + y as u16, l, inner.width);
    }
}

fn draw_rewind(p: &RewindPicker, area: Rect, buf: &mut Buffer, probe: &mut Probe) {
    let width = area.width.saturating_sub(2).min(78);
    let inner_w = width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, m) in MODES.iter().enumerate() {
        let sel = *m == p.selected;
        let text = format!("{} [{}] {:<5} {}", if sel { "▸" } else { " " }, i + 1, mode_name(*m), mode_doc(*m));
        let style = if sel { Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { Style::default() };
        lines.push(Line::from(Span::styled(trunc(&text, inner_w), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(format!(" plan ({}):", mode_name(p.selected)), Style::default().add_modifier(Modifier::BOLD))));
    match &p.plan {
        None => lines.push(Line::from(Span::styled("   computing the plan…", dim()))),
        Some(Err(e)) => {
            for l in wrap(&format!("cannot rewind: {e}"), inner_w.saturating_sub(3)) {
                lines.push(Line::from(Span::styled(format!("   {l}"), Style::default().fg(Color::Red))));
            }
        }
        Some(Ok(plan)) => {
            match plan.fork_at {
                Some(at) => lines.push(Line::from(format!("   fork the log at entry {}", at.0))),
                None => lines.push(Line::from("   same branch: a restore notice is appended for the model")),
            }
            match &plan.restore {
                Some((changed, deleted)) => {
                    let names: Vec<&str> = changed.iter().chain(deleted.iter()).map(String::as_str).take(4).collect();
                    let more = if changed.len() + deleted.len() > names.len() { " …" } else { "" };
                    lines.push(Line::from(trunc(
                        &format!("   restore files: {} changed, {} deleted  {}{more}", changed.len(), deleted.len(), names.join(", ")),
                        inner_w,
                    )));
                }
                None => lines.push(Line::from("   files untouched")),
            }
            if !plan.kill.is_empty() {
                lines.push(Line::from(trunc(&format!("   kill {} running job(s): {}", plan.kill.len(), plan.kill.join(", ")), inner_w)));
            }
            lines.push(Line::from(Span::styled("   a PreRewind checkpoint is taken first, so this rewind can be undone", dim())));
        }
    }
    lines.push(Line::from(Span::styled(" [enter] apply   [↑↓ 1-3] mode   [esc] cancel", Style::default().fg(Color::Cyan))));
    let height = lines.len() as u16 + 2;
    let rect = centered(area, width, height);
    probe.area("rewind", rect);
    probe.check(height <= area.height, format!("rewind picker needs {height} rows; the terminal has {}", area.height));
    Clear.render(rect, buf);
    let block = Block::bordered().title(format!(" rewind to c{} ", p.checkpoint.0)).border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(rect);
    block.render(rect, buf);
    for (y, l) in lines.iter().take(inner.height as usize).enumerate() {
        buf.set_line(inner.x, inner.y + y as u16, l, inner.width);
    }
}
