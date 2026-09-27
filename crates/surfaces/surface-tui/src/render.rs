//! Drawing is a pure function of [`UiState`]: [`draw`] fills a `Buffer`, records each area it
//! laid out on a [`Probe`], and asserts layout invariants with [`Probe::check`]. The runtime and
//! the tests (ratatui's `TestBackend`) call the same functions.
//!
//! One column, no sidebar: the transcript, then the prompt box (its top border is the status
//! line), then the `/` menu while it is open, then one row of key hints or a notice. Commands
//! open overlays in front of the transcript. Inline (the terminal's own scrollback holds the
//! finished transcript) only the live bottom is drawn: [`draw_inline`], [`block_lines`].

use aho_inspect::{TreeNode, glyph, render_classes, state_name};
use aho_log::Verdict;
use aho_log::{CapState, OpStatus};
use aho_types::{Consequence, Timestamp};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use serde_json::{Value, json};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::frames::{Frame as Captured, Probe, buffer_lines};
use crate::markdown;
use crate::menu::{self, MENU_ROWS, VIEWS};
use crate::model::{
    ApprovalView, Body, CheckpointList, MODES, Overlay, Panel, PanelBody, ResultStatus, RewindPicker, Tone, ToolCard, UiState, View,
    compact, mode_doc, mode_name, principal_label,
};
use crate::theme::Theme;

pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;
/// Rows a notice may wrap to in the hint row.
const NOTICE_MAX: usize = 3;

/// Renders `ui` into a fresh `w`×`h` buffer (the full screen). Pure: same state, same cells.
pub fn render_to_buffer(ui: &UiState, w: u16, h: u16) -> Buffer {
    capture(ui, w, h).0
}

/// Renders the full screen and captures the frame record (rects, text per area, anomalies).
pub fn capture(ui: &UiState, w: u16, h: u16) -> (Buffer, Captured) {
    let started = std::time::Instant::now();
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    let mut probe = Probe::default();
    draw(ui, area, &mut buf, &mut probe);
    let ms = started.elapsed().as_millis() as u64;
    let frame = Captured::from_buffer(&buf, probe, ms);
    (buf, frame)
}

/// What is in front of the transcript: `approval`, the top overlay, or `none`.
pub fn front(ui: &UiState) -> &'static str {
    if ui.approval.is_some() { "approval" } else { ui.overlays.last().map_or("none", Overlay::name) }
}

/// `aho tui --headless`: normalized screen JSON (rows trimmed, no styles) plus the frame record.
pub fn render_frame_json(ui: &UiState, w: u16, h: u16) -> Value {
    let (buf, frame) = capture(ui, w, h);
    json!({
        "v": 2,
        "width": w,
        "height": h,
        "lines": buffer_lines(&buf, buf.area),
        "areas": frame.rects.iter().zip(frame.text.iter()).map(|((n, x, y, rw, rh), (_, t))| json!({"area": n, "x": x, "y": y, "w": rw, "h": rh, "text": t})).collect::<Vec<_>>(),
        "anomalies": frame.anomalies,
        "overlay": front(ui),
        "menu": menu::open(ui).map(|(items, sel)| json!({"selected": sel, "items": items.iter().map(|s| s.label.clone()).collect::<Vec<_>>()})),
    })
}

// ───────────────────────────── layout ─────────────────────────────

/// Rows below the transcript: the prompt box, the menu, the hint row.
fn chrome_heights(ui: &UiState, width: u16) -> (u16, u16, u16) {
    let notice = ui.notice.as_ref().map_or(1, |n| wrap(&n.text, (width as usize).saturating_sub(1)).len().clamp(1, NOTICE_MAX));
    (3, menu::height(ui), notice as u16)
}

fn split_bottom(ui: &UiState, area: Rect) -> (Rect, Rect, Rect, Rect) {
    let (input_h, menu_h, footer_h) = chrome_heights(ui, area.width);
    let chrome = (input_h + menu_h + footer_h).min(area.height);
    let top = Rect { height: area.height - chrome, ..area };
    let mut y = area.y + top.height;
    let mut take = |h: u16| {
        let r = Rect { y, height: h.min(area.bottom().saturating_sub(y)), ..area };
        y += r.height;
        r
    };
    let input = take(input_h);
    let menu = take(menu_h);
    let footer = take(footer_h);
    (top, input, menu, footer)
}

/// Draws the whole screen; returns the terminal cursor position (in the prompt) if it should show.
pub fn draw(ui: &UiState, area: Rect, buf: &mut Buffer, probe: &mut Probe) -> Option<(u16, u16)> {
    probe.check(
        area.width >= MIN_WIDTH && area.height >= MIN_HEIGHT,
        format!("terminal is {}x{}, smaller than {MIN_WIDTH}x{MIN_HEIGHT}; panes are clipped", area.width, area.height),
    );
    let (top, input, menu_area, footer) = split_bottom(ui, area);
    probe.area("transcript", top);
    draw_transcript(ui, top, buf);
    let cursor = draw_chrome(ui, input, menu_area, footer, buf, probe);
    // Only the top overlay shows; the ones under it come back as it closes.
    match ui.overlays.last() {
        Some(Overlay::Panel(p)) => draw_panel(ui, p, top, buf, probe),
        Some(Overlay::Checkpoints(list)) => draw_checkpoints(&ui.theme, list, top, buf, probe),
        Some(Overlay::Rewind(p)) => draw_rewind(&ui.theme, p, area, buf, probe),
        None => {}
    }
    if let Some(card) = &ui.approval {
        draw_approval(&ui.theme, card, ui.approvals_queued, area, buf, probe);
        return None;
    }
    if !ui.overlays.is_empty() {
        return None;
    }
    cursor
}

/// Inline: the live part of the transcript (blocks from `from` on, plus the stream) above the
/// prompt, sized to what it holds. Everything before `from` is already in the scrollback.
pub fn draw_inline(ui: &UiState, from: usize, area: Rect, buf: &mut Buffer, probe: &mut Probe) -> Option<(u16, u16)> {
    let (top, input, menu_area, footer) = split_bottom(ui, area);
    let rows = live_rows(ui, from, top.width as usize);
    probe.area("live", top);
    let skip = rows.len().saturating_sub(top.height as usize);
    for (y, line) in rows.iter().skip(skip).enumerate() {
        set_row(buf, top, y as u16, line);
    }
    draw_chrome(ui, input, menu_area, footer, buf, probe)
}

/// The inline viewport height for the current state: the prompt chrome plus the live rows,
/// within `max` (a live part taller than that shows its tail).
pub fn inline_height(ui: &UiState, from: usize, width: u16, max: u16) -> u16 {
    let (a, b, c) = chrome_heights(ui, width);
    let live = u16::try_from(live_rows(ui, from, width as usize).len()).unwrap_or(u16::MAX);
    (a + b + c).saturating_add(live).min(max)
}

fn set_row(buf: &mut Buffer, area: Rect, y: u16, line: &Line<'_>) {
    if y >= area.height {
        return;
    }
    let row = Rect { y: area.y + y, height: 1, ..area };
    if let Some(bg) = line.style.bg {
        buf.set_style(row, Style::default().bg(bg));
    }
    buf.set_line(row.x, row.y, line, row.width);
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

fn push_wrapped(rows: &mut Vec<Line<'static>>, prefix: &str, text: &str, width: usize, style: Style, prefix_style: Style) {
    let pw = UnicodeWidthStr::width(prefix);
    let pad = " ".repeat(pw);
    for (i, l) in wrap(text, width.saturating_sub(pw)).into_iter().enumerate() {
        let p = if i == 0 { prefix.to_string() } else { pad.clone() };
        rows.push(Line::from(vec![Span::styled(p, prefix_style), Span::styled(l, style)]));
    }
}

fn class_name(c: Option<Consequence>) -> &'static str {
    c.map_or("unclassified", |c| c.name())
}

/// A tool card: tinted by its state (running, ok, failed), title, arguments, the result's tail.
fn tool_rows(t: &Theme, rows: &mut Vec<Line<'static>>, c: &ToolCard, width: usize) {
    let title = match &c.cap {
        Some(cap) => format!("{cap}.{}", c.tool),
        None => c.tool.clone(),
    };
    let (status, style, tint) = match &c.result {
        None => (
            match c.op {
                Some(op) => format!("⋯ running (op {})", op.0),
                None => "⋯ running".to_string(),
            },
            t.warning,
            t.tool_pending,
        ),
        Some(r) => {
            let late = if r.late { " (late)" } else { "" };
            match r.status {
                ResultStatus::Ok => (format!("✓ {}{late}", r.summary), t.success, t.tool_ok),
                ResultStatus::Err => (format!("✕ {}{late}", r.summary), t.error, t.tool_err),
                ResultStatus::Cancelled => (format!("⊘ {}{late}", r.summary), t.muted, t.tool_pending),
            }
        }
    };
    let head = format!("▍ {title} ");
    let class = format!("· {} ", class_name(c.class));
    let rest = width.saturating_sub(UnicodeWidthStr::width(head.as_str()) + class.len());
    let row = |spans: Vec<Span<'static>>| Line::from(spans).style(tint);
    rows.push(row(vec![Span::styled(head, t.title), Span::styled(class, t.muted), Span::styled(trunc(&status, rest), style)]));
    let bar = || Span::styled("▍ ", t.accent);
    if !c.args.is_null() {
        rows.push(row(vec![bar(), Span::styled(compact(&c.args, width.saturating_sub(2)), t.muted)]));
    }
    if let Some(r) = &c.result {
        for l in &r.body {
            rows.push(row(vec![bar(), Span::styled(trunc(l, width.saturating_sub(2)), t.dim)]));
        }
    }
    if let Some(d) = &c.decision {
        rows.push(row(vec![bar(), Span::styled(trunc(&format!("⚑ {d}"), width.saturating_sub(2)), t.info)]));
    }
}

/// Whether a blank row separates block `prev` from `cur` (runs of tool cards and of notices stay
/// together).
fn spaced(prev: Option<&Body>, cur: &Body) -> bool {
    let quiet = |b: &Body| matches!(b, Body::Notice(_) | Body::Checkpoint { .. } | Body::Restored { .. });
    match prev {
        None => false,
        Some(p) => !(matches!((p, cur), (Body::Tool(_), Body::Tool(_))) || (quiet(p) && quiet(cur)) || matches!(p, Body::Thinking(_))),
    }
}
fn visible(ui: &UiState, body: &Body) -> bool {
    ui.local.show_thinking || !matches!(body, Body::Thinking(_))
}

fn block_rows(ui: &UiState, i: usize, width: usize, rows: &mut Vec<Line<'static>>) {
    let t = &ui.theme;
    let b = &ui.transcript.blocks[i];
    if !visible(ui, &b.body) {
        return;
    }
    let prev = ui.transcript.blocks[..i].iter().rev().find(|b| visible(ui, &b.body)).map(|b| &b.body);
    if spaced(prev, &b.body) {
        rows.push(Line::default());
    }
    match &b.body {
        Body::User(text) => push_wrapped(rows, "❯ ", text, width, t.user, t.accent.add_modifier(Modifier::BOLD)),
        Body::Assistant(text) => rows.extend(markdown::render(text, width, t)),
        Body::Thinking(text) => push_wrapped(rows, "∴ ", text, width, t.thinking, t.dim),
        Body::Notice(text) => push_wrapped(rows, "· ", text, width, t.muted, t.dim),
        Body::Error(text) => push_wrapped(rows, "✕ ", text, width, t.error, t.error),
        Body::Tool(c) => tool_rows(t, rows, c, width),
        Body::Checkpoint { id, cause, turn } => {
            rows.push(Line::from(vec![
                Span::styled(trunc(&format!("◆ checkpoint c{} · turn {turn} · {cause}", id.0), width.saturating_sub(10)), t.info),
                Span::styled("  /rewind", t.dim),
            ]));
        }
        Body::Restored { from, to, mode, changed, deleted } => {
            let text =
                format!("↺ rewound to c{} ({}) · {changed} changed, {deleted} deleted · undo with c{}", to.0, mode_name(*mode), from.0);
            push_wrapped(rows, "", &text, width, t.info, t.info);
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
        rows.push(Line::from(Span::styled(trunc(&hud, width), t.info.add_modifier(Modifier::DIM))));
    }
}

/// Blocks `range` as the rows the scrollback keeps (spacing included).
pub fn block_lines(ui: &UiState, range: std::ops::Range<usize>, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for i in range.start..range.end.min(ui.transcript.blocks.len()) {
        block_rows(ui, i, width, &mut rows);
    }
    rows
}

/// What has not been committed yet: blocks from `from` on, and the stream of the running call.
fn live_rows(ui: &UiState, from: usize, width: usize) -> Vec<Line<'static>> {
    let t = &ui.theme;
    let mut rows = block_lines(ui, from..ui.transcript.blocks.len(), width);
    let tr = &ui.transcript;
    if ui.local.show_thinking && !tr.live_thinking.is_empty() {
        push_wrapped(&mut rows, "∴ ", &tr.live_thinking, width, t.thinking, t.dim);
    }
    if !tr.live_text.is_empty() {
        rows.extend(markdown::render(&tr.live_text, width, t));
    }
    for name in &tr.live_tools {
        rows.push(Line::from(Span::styled(trunc(&format!("⋯ calling {name}"), width), t.warning)));
    }
    rows
}

fn draw_transcript(ui: &UiState, area: Rect, buf: &mut Buffer) {
    let rows = live_rows(ui, 0, area.width as usize);
    let h = area.height as usize;
    if rows.is_empty() {
        let t = &ui.theme;
        let hello = Line::from(vec![
            Span::styled(format!(" {} ", ui.plan.label), t.title),
            Span::styled("· type a prompt, or / for commands", t.muted),
        ]);
        set_row(buf, area, 0, &hello);
        return;
    }
    let max_top = rows.len().saturating_sub(h);
    let top = max_top.saturating_sub(ui.local.scroll.min(max_top));
    for (y, line) in rows.iter().skip(top).take(h).enumerate() {
        set_row(buf, area, y as u16, line);
    }
}

// ───────────────────────────── prompt, menu, hints ─────────────────────────────

fn draw_chrome(ui: &UiState, input: Rect, menu_area: Rect, footer: Rect, buf: &mut Buffer, probe: &mut Probe) -> Option<(u16, u16)> {
    probe.area("input", input);
    let cursor = draw_input(ui, input, buf, probe);
    if menu_area.height > 0 {
        probe.area("menu", menu_area);
        draw_menu(ui, menu_area, buf);
    }
    probe.area("footer", footer);
    draw_footer(ui, footer, buf);
    cursor
}

/// Token counts the way the status line prints them: `950`, `4.2k`, `128k`, `1M`, `2.5M`.
pub(crate) fn tokens(n: u64) -> String {
    let short = |v: f64, unit: &str| {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    // Each bound is where the rounded value would print as the next unit (999.5k is `1M`).
    match n {
        0..1_000 => n.to_string(),
        1_000..9_950 => short(n as f64 / 1e3, "k"),
        9_950..999_500 => format!("{}k", (n as f64 / 1e3).round() as u64),
        999_500..9_950_000 => short(n as f64 / 1e6, "M"),
        _ => format!("{}M", (n as f64 / 1e6).round() as u64),
    }
}

/// US dollars: cents, or a tenth of a cent below one cent so a first cheap call is not `$0.00`.
pub(crate) fn dollars(v: f64) -> String {
    if v > 0.0 && v < 0.01 { format!("${v:.3}") } else { format!("${v:.2}") }
}

/// A piece of the status line, dropped by `rank` (lowest first) when the line is too narrow.
struct Seg {
    rank: u8,
    sep: &'static str,
    spans: Vec<Span<'static>>,
}

fn width_of(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// The prompt box's top border: model › workspace › spend · preset · world on the left, the
/// context gauge and the busy state on the right. Narrow terminals drop the HUD facts, the `aho`
/// mark and the world, then shorten the workspace to its last component, then drop it, the
/// preset and the spend; the model and the right side stay (the border clips what still overflows).
fn status_line(ui: &UiState, width: u16) -> (Line<'static>, Line<'static>) {
    let t = &ui.theme;
    let p = &ui.plan;
    let window = tokens(p.context_window);
    let mut right = match ui.context_used {
        Some(used) if p.context_window > 0 => {
            let ratio = used as f64 / p.context_window as f64;
            let tone = if ratio >= p.compact_at {
                t.error
            } else if ratio >= p.compact_at * 0.75 {
                t.warning
            } else {
                t.success
            };
            // A first short exchange is not an empty context.
            let pct = if used > 0 && ratio < 0.005 { "<1%".to_string() } else { format!("{:.0}%", ratio * 100.0) };
            vec![Span::styled(format!(" {pct}"), tone), Span::styled(format!(" of {window} "), t.muted)]
        }
        _ => vec![Span::styled(format!(" –% of {window} "), t.dim)],
    };
    right.push(if ui.is_busy() { Span::styled("· ● running ", t.warning) } else { Span::styled("· ○ idle ", t.dim) });

    let u = &ui.spent;
    let spend = match p.price {
        Some(price) => Span::styled(dollars(price.cost(u)), t.cost),
        None => Span::styled(
            format!("↑{} ↓{}", tokens(u.input_tokens + u.cache_read_tokens + u.cache_write_tokens), tokens(u.output_tokens)),
            t.muted,
        ),
    };
    let mut segs = vec![
        Seg { rank: 2, sep: "", spans: vec![Span::styled("aho", t.dim)] },
        Seg { rank: 9, sep: " › ", spans: vec![Span::styled(p.model.clone(), t.title)] },
    ];
    if !p.workspace.is_empty() {
        segs.push(Seg { rank: 5, sep: " › ", spans: vec![Span::styled(p.workspace.clone(), t.text)] });
    }
    segs.push(Seg { rank: 8, sep: " › ", spans: vec![spend] });
    segs.push(Seg { rank: 6, sep: " · ", spans: vec![Span::styled(format!("preset {}", ui.state.policy.preset), t.muted)] });
    segs.push(Seg { rank: 3, sep: " · ", spans: vec![Span::styled(format!("world {}", p.world), t.muted)] });
    if ui.local.hud {
        let hud = format!(
            "{} · epoch {} · plan {} · preset ← {} · {} · ↑{} ↓{} cached {}",
            p.label,
            ui.state.policy.epoch.0,
            p.hash,
            p.preset_origin,
            p.enforced_by,
            tokens(u.input_tokens),
            tokens(u.output_tokens),
            tokens(u.cache_read_tokens)
        );
        segs.push(Seg { rank: 1, sep: " · ", spans: vec![Span::styled(hud, t.info)] });
    }
    // Two corners, the padding around each side.
    let room = (width as usize).saturating_sub(2 + width_of(&right) + 2);
    let total =
        |segs: &[Seg]| segs.iter().enumerate().map(|(i, s)| if i == 0 { 0 } else { s.sep.width() } + width_of(&s.spans)).sum::<usize>();
    let mut shortened = false;
    while total(&segs) > room {
        let Some(at) = segs.iter().enumerate().filter(|(_, s)| s.rank < 9).min_by_key(|(_, s)| s.rank).map(|(i, _)| i) else {
            break;
        };
        if segs[at].rank == 5 && !shortened {
            shortened = true;
            let last = p.workspace.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            segs[at].spans = vec![Span::styled(format!("…/{last}"), t.text)];
            continue;
        }
        segs.remove(at);
    }
    if total(&segs) > room {
        // Only the model is left and it is still too wide: the gauge wins, the name is cut.
        for s in segs.iter_mut().filter(|s| s.rank == 9) {
            s.spans = vec![Span::styled(trunc(&p.model, room), t.title)];
        }
    }
    let mut left = vec![Span::raw(" ")];
    for (i, s) in segs.into_iter().enumerate() {
        if i > 0 {
            left.push(Span::styled(s.sep, t.dim));
        }
        left.extend(s.spans);
    }
    left.push(Span::raw(" "));
    (Line::from(left), Line::from(right).alignment(Alignment::Right))
}

fn draw_input(ui: &UiState, area: Rect, buf: &mut Buffer, probe: &mut Probe) -> Option<(u16, u16)> {
    let t = &ui.theme;
    let (status, gauge) = status_line(ui, area.width);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if ui.covered() { t.border } else { t.border_focus })
        .title(status)
        .title(gauge);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    let prompt = Span::styled("❯ ", t.accent.add_modifier(Modifier::BOLD));
    let pw = 2u16;
    if ui.local.draft.is_empty() {
        let hint = if ui.is_busy() { "type to steer the running turn" } else { "type a prompt · / for commands" };
        buf.set_line(inner.x, inner.y, &Line::from(vec![prompt, Span::styled(hint, t.dim)]), inner.width);
        return (!ui.covered()).then_some((inner.x + pw, inner.y));
    }
    let chars: Vec<char> = ui.local.draft.chars().collect();
    let cursor = ui.local.cursor.min(chars.len());
    let avail = inner.width.saturating_sub(pw + 1) as usize;
    let before_w: usize = chars[..cursor].iter().map(|c| c.width().unwrap_or(0)).sum();
    let mut start = 0usize;
    let mut w = before_w;
    while w > avail && start < cursor {
        w -= chars[start].width().unwrap_or(0);
        start += 1;
    }
    let visible: String = chars[start..].iter().collect();
    let text_style = if ui.local.draft.starts_with('/') && !ui.local.draft.starts_with("//") { t.accent } else { t.text };
    buf.set_line(
        inner.x,
        inner.y,
        &Line::from(vec![prompt, Span::styled(trunc(&visible, inner.width.saturating_sub(pw) as usize), text_style)]),
        inner.width,
    );
    let cx = inner.x + pw + w as u16;
    probe.check(cx < inner.x + inner.width, format!("input cursor column {cx} is outside the input box"));
    (!ui.covered()).then_some((cx.min(inner.right().saturating_sub(1)), inner.y))
}

fn draw_menu(ui: &UiState, area: Rect, buf: &mut Buffer) {
    let Some((items, selected)) = menu::open(ui) else { return };
    let t = &ui.theme;
    let shown = items.len().min(MENU_ROWS);
    let first = selected.saturating_sub(shown - 1).min(items.len() - shown);
    let label_w = items.iter().map(|s| UnicodeWidthStr::width(s.label.as_str()) + 1 + s.usage.len()).max().unwrap_or(0).min(28);
    for (row, (i, s)) in items.iter().enumerate().skip(first).take(shown).enumerate() {
        let sel = i == selected;
        let base = if sel { t.selected } else { Style::default() };
        let mark = if sel { "▸ " } else { "  " };
        let label = format!("{}{}", s.label, if s.usage.is_empty() { String::new() } else { format!(" {}", s.usage) });
        let pad = label_w.saturating_sub(UnicodeWidthStr::width(label.as_str()));
        let line = Line::from(vec![
            Span::styled(format!(" {mark}"), base.patch(t.accent)),
            Span::styled(s.label.clone(), base.patch(if sel { t.selected } else { t.accent })),
            Span::styled(if s.usage.is_empty() { String::new() } else { format!(" {}", s.usage) }, base.patch(t.muted)),
            Span::styled(" ".repeat(pad + 2), base),
            Span::styled(trunc(&s.desc, (area.width as usize).saturating_sub(label_w + 6)), base.patch(t.dim)),
        ])
        .style(base);
        set_row(buf, area, row as u16, &line);
    }
    if items.len() > shown {
        let more = Line::from(Span::styled(format!("   {} of {} · ↑↓ to see more", selected + 1, items.len()), t.dim));
        set_row(buf, area, shown as u16, &more);
    }
}

fn draw_footer(ui: &UiState, area: Rect, buf: &mut Buffer) {
    let t = &ui.theme;
    if let Some(n) = &ui.notice {
        let style = match n.tone {
            Tone::Info => t.accent,
            Tone::Warn => t.warning,
            Tone::Error => t.error,
        };
        for (i, text) in wrap(&n.text, (area.width as usize).saturating_sub(1)).iter().take(area.height as usize).enumerate() {
            set_row(buf, area, i as u16, &Line::from(Span::styled(format!(" {text}"), style)));
        }
        return;
    }
    let keys: &[(&str, &str)] = if ui.approval.is_some() {
        &[("a", "allow"), ("s", "session"), ("r", "reject"), ("esc", "cancel")]
    } else if !ui.overlays.is_empty() {
        &[("esc", "close"), ("↑↓", "move"), ("enter", "choose"), ("ctrl-q", "quit")]
    } else if menu::open(ui).is_some() {
        &[("↑↓", "select"), ("tab", "complete"), ("enter", "run"), ("esc", "close")]
    } else if ui.is_busy() {
        &[("enter", "steer"), ("alt-enter", "urgent steer"), ("ctrl-c", "abort"), ("/", "commands")]
    } else {
        &[("enter", "send"), ("/", "commands"), ("alt-h", "HUD"), ("ctrl-c", "abort"), ("ctrl-q", "quit")]
    };
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, what)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", t.dim));
        }
        spans.push(Span::styled(*k, t.muted.add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {what}"), t.dim));
    }
    set_row(buf, area, 0, &Line::from(spans));
}

// ───────────────────────────── panels ─────────────────────────────

fn state_style(t: &Theme, s: &CapState) -> Style {
    match s {
        CapState::Active => t.success,
        CapState::Unlocked => t.accent,
        CapState::Locked { .. } => t.warning,
        CapState::Broken { .. } => t.error,
        CapState::Disabled { .. } => t.dim,
    }
}

fn tree_rows(t: &Theme, nodes: &[TreeNode], depth: usize, width: usize, out: &mut Vec<Line<'static>>) {
    for n in nodes {
        let indent = "  ".repeat(depth);
        let name_w = width.saturating_sub(indent.len() + 2 + 10).clamp(8, 40);
        out.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(format!("{} ", glyph(&n.state)), state_style(t, &n.state)),
            Span::styled(format!("{:<w$} ", trunc(&n.id, name_w), w = name_w), t.text.add_modifier(Modifier::BOLD)),
            Span::styled(state_name(&n.state), state_style(t, &n.state)),
        ]));
        let sub = format!("{indent}    ");
        let sub_w = width.saturating_sub(sub.len());
        for m in &n.missing {
            out.push(Line::from(Span::styled(format!("{sub}{}", trunc(&m.requirement, sub_w)), t.muted)));
            if let Some(f) = &m.fix {
                out.push(Line::from(vec![
                    Span::raw(sub.clone()),
                    Span::styled("fix: ", t.dim),
                    Span::styled(trunc(f, sub_w.saturating_sub(5)), t.accent),
                ]));
            }
        }
        if let CapState::Broken { err } = &n.state {
            out.push(Line::from(Span::styled(format!("{sub}{}", trunc(err, sub_w)), t.error)));
        }
        if matches!(n.state, CapState::Unlocked | CapState::Disabled { .. })
            && let Some(a) = &n.action
        {
            out.push(Line::from(Span::styled(format!("{sub}{}", trunc(a, sub_w)), t.accent)));
        }
        if let Some(o) = &n.over_preset {
            out.push(Line::from(Span::styled(format!("{sub}! {}", trunc(&o.requirement, sub_w.saturating_sub(2))), t.warning)));
        }
        tree_rows(t, &n.children, depth + 1, width, out);
    }
}

fn help_rows(ui: &UiState, width: usize) -> Vec<Line<'static>> {
    let t = &ui.theme;
    let mut out = Vec::new();
    let section = |out: &mut Vec<Line<'static>>, name: &str| {
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.push(Line::from(Span::styled(name.to_string(), t.title)));
    };
    let row = |out: &mut Vec<Line<'static>>, name: String, what: &str| {
        let pad = 24usize.saturating_sub(UnicodeWidthStr::width(name.as_str()));
        out.push(Line::from(vec![
            Span::styled(format!("  {name}"), t.accent),
            Span::raw(" ".repeat(pad)),
            Span::styled(trunc(what, width.saturating_sub(28)), t.muted),
        ]));
    };
    section(&mut out, "views");
    for (n, h) in VIEWS {
        row(&mut out, format!("/{n}"), h);
    }
    if !ui.commands.is_empty() {
        section(&mut out, "commands");
        for c in &ui.commands {
            let usage = if c.usage.is_empty() { String::new() } else { format!(" {}", c.usage) };
            row(&mut out, format!("/{}{usage}", c.name), &c.help);
        }
    }
    section(&mut out, "keys");
    for (k, what) in [
        ("enter", "send the prompt (steer while a turn runs)"),
        ("alt-enter", "urgent steer"),
        ("/  tab  ↑↓", "open, complete and move in the command menu"),
        ("esc", "close the menu or the panel in front"),
        ("ctrl-c", "abort the running turn"),
        ("ctrl-a ctrl-e ctrl-u ctrl-w", "line editing"),
        ("alt-h", "HUD: entry ids and origins"),
        ("ctrl-q / ctrl-d", "quit"),
        ("//text", "send a prompt that starts with /"),
    ] {
        row(&mut out, k.to_string(), what);
    }
    out
}

fn panel_rows(ui: &UiState, p: &Panel, width: usize) -> Vec<Line<'static>> {
    let t = &ui.theme;
    let mut out = Vec::new();
    match &p.body {
        PanelBody::Text(lines) => {
            let style = if p.tone == Tone::Error { t.error } else { t.text };
            for l in lines {
                for w in wrap(l, width) {
                    out.push(Line::from(Span::styled(w, style)));
                }
            }
        }
        PanelBody::View(View::Help) => out = help_rows(ui, width),
        PanelBody::View(View::Tree) => tree_rows(t, &ui.tree, 0, width, &mut out),
        PanelBody::View(View::Inventory) => {
            for c in &ui.inventory {
                out.push(Line::from(vec![
                    Span::styled(format!("{} ", glyph(&c.state)), state_style(t, &c.state)),
                    Span::styled(trunc(&c.id, width.saturating_sub(2)), t.text.add_modifier(Modifier::BOLD)),
                ]));
                let origin = match &c.source {
                    Some(s) => format!("{} ({s})", c.origin),
                    None => c.origin.clone(),
                };
                out.push(Line::from(Span::styled(trunc(&format!("  {} · {} · {}", c.kind, origin, state_name(&c.state)), width), t.muted)));
                if c.classes.0 != 0 {
                    out.push(Line::from(Span::styled(trunc(&format!("  consequences: {}", render_classes(c.classes)), width), t.text)));
                }
                if let Some(e) = &c.equip {
                    out.push(Line::from(Span::styled(trunc(&format!("  equip: {e}"), width), t.dim)));
                }
                if let Some(f) = &c.fix {
                    out.push(Line::from(Span::styled(trunc(&format!("  fix: {f}"), width), t.warning)));
                }
            }
        }
        PanelBody::View(View::Jobs) => {
            if ui.jobs.is_empty() {
                out.push(Line::from(Span::styled("no operations yet", t.dim)));
            }
            let now = Timestamp::now();
            for j in &ui.jobs {
                let (status, style) = match &j.status {
                    None => (format!("running {}s", now.0.saturating_sub(j.started.0) / 1000), t.warning),
                    Some(OpStatus::Completed { code }) => (format!("✓ exit {code}"), t.success),
                    Some(OpStatus::Failed { err }) => (format!("✕ {err}"), t.error),
                    Some(OpStatus::Cancelled { reason }) => (format!("⊘ {reason:?}").to_lowercase(), t.dim),
                    Some(OpStatus::Uncertain { .. }) => ("? uncertain".to_string(), t.info),
                };
                let head = format!("op{} {} ", j.op.0, j.tool);
                let rest = width.saturating_sub(UnicodeWidthStr::width(head.as_str()));
                out.push(Line::from(vec![Span::styled(head, t.text), Span::styled(trunc(&status, rest), style)]));
            }
        }
        PanelBody::View(View::Invariants) => {
            let rows = ui.invariant_rows();
            if rows.is_empty() {
                out.push(Line::from(Span::styled("no invariants checked in this session", t.dim)));
            }
            for r in rows {
                let (v, style) = match &r.verdict {
                    Verdict::Pass => ("Pass".to_string(), t.success),
                    Verdict::Fail { counterexample } => (format!("Fail ✕ (cx {})", counterexample.short()), t.error),
                    Verdict::Unknown { reason } => (format!("Unknown ({reason})"), t.warning),
                    Verdict::Stale => ("Stale".to_string(), t.warning),
                    Verdict::Vacuous => ("Vacuous ⚠".to_string(), t.warning),
                };
                out.push(Line::from(vec![
                    Span::styled(format!("{} ", trunc(&r.id, width.saturating_sub(12))), t.text),
                    Span::styled(v, style),
                ]));
                if r.stale {
                    out.push(Line::from(Span::styled(trunc("  code changed since the check", width), t.dim)));
                }
            }
        }
    }
    out
}

/// The frame of an overlay box.
struct Frame<'a> {
    title: &'a str,
    border: Style,
    hint: &'a str,
}

/// A box in front of `area`, full width and bottom-anchored (over the transcript, above the
/// prompt), cleared, with a rounded border; returns its inside.
fn boxed(t: &Theme, area: Rect, height: u16, f: Frame<'_>, buf: &mut Buffer) -> Rect {
    let Frame { title, border, hint } = f;
    let w = area.width;
    let h = height.min(area.height.saturating_sub(1)).max(area.height.min(3));
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + area.height.saturating_sub(h), width: w, height: h };
    Clear.render(rect, buf);
    buf.set_style(rect, t.panel);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Line::from(Span::styled(format!(" {title} "), t.title)))
        .title_bottom(Line::from(Span::styled(format!(" {hint} "), t.dim)).alignment(Alignment::Right));
    let inner = block.inner(rect);
    block.render(rect, buf);
    inner
}

fn draw_panel(ui: &UiState, p: &Panel, area: Rect, buf: &mut Buffer, probe: &mut Probe) {
    let t = &ui.theme;
    let width = area.width;
    let rows = panel_rows(ui, p, width.saturating_sub(4) as usize);
    let height = (rows.len() as u16 + 2).max(5).min(area.height);
    let visible = height.saturating_sub(2) as usize;
    let max_top = rows.len().saturating_sub(visible);
    let top = p.scroll.min(max_top);
    let pos =
        if rows.len() > visible { format!("{}-{} of {} · ", top + 1, (top + visible).min(rows.len()), rows.len()) } else { String::new() };
    let border = if p.tone == Tone::Error { t.error } else { t.border_focus };
    let hint = format!("{pos}esc close · ↑↓ scroll");
    let inner = boxed(t, area, height, Frame { title: &p.title, border, hint: &hint }, buf);
    probe.area("panel", inner);
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
    for (y, line) in rows.iter().skip(top).take(inner.height as usize).enumerate() {
        buf.set_line(inner.x, inner.y + y as u16, line, inner.width);
    }
}

fn draw_checkpoints(t: &Theme, list: &CheckpointList, area: Rect, buf: &mut Buffer, probe: &mut Probe) {
    let shown = list.items.len().min(area.height.saturating_sub(3) as usize).max(1);
    let inner = boxed(t, area, shown as u16 + 2, Frame { title: "rewind to…", border: t.info, hint: "enter choose · esc close" }, buf);
    probe.area("checkpoints", inner);
    let first = list.selected.saturating_sub(shown - 1);
    for (row, (i, item)) in list.items.iter().enumerate().skip(first).take(shown).enumerate() {
        let sel = i == list.selected;
        let style = if sel { t.selected } else { t.text };
        let line =
            Line::from(Span::styled(trunc(&format!("{} ◆ {}", if sel { "▸" } else { " " }, item.label), inner.width as usize), style))
                .style(style);
        let r = Rect { y: inner.y + row as u16, height: 1, ..inner };
        if sel {
            buf.set_style(r, t.selected);
        }
        buf.set_line(r.x, r.y, &line, r.width);
    }
}

// ───────────────────────────── modal cards ─────────────────────────────

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn card_frame(t: &Theme, rect: Rect, title: String, border: Style, buf: &mut Buffer) -> Rect {
    Clear.render(rect, buf);
    buf.set_style(rect, t.panel);
    let block = Block::bordered().border_type(BorderType::Rounded).title(Line::from(Span::styled(title, t.title))).border_style(border);
    let inner = block.inner(rect);
    block.render(rect, buf);
    inner
}

/// The approval card.
fn draw_approval(t: &Theme, card: &ApprovalView, queued: usize, area: Rect, buf: &mut Buffer, probe: &mut Probe) {
    let width = area.width.saturating_sub(2).min(78);
    let inner_w = width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let title = card.title();
    let cls = format!("class: {}", card.class.name());
    let gap = inner_w.saturating_sub(1 + UnicodeWidthStr::width(title.as_str()) + cls.len()).max(2);
    lines.push(Line::from(vec![
        Span::styled(format!(" {title}"), t.text.add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(gap)),
        Span::styled(cls, t.warning),
    ]));
    for (i, l) in wrap(&card.args, inner_w.saturating_sub(7)).into_iter().take(3).enumerate() {
        lines.push(Line::from(vec![Span::styled(format!(" {}", if i == 0 { "args: " } else { "      " }), t.muted), Span::raw(l)]));
    }
    let why = format!("{}      (reason: {})", card.why, card.reason);
    for (i, l) in wrap(&why, inner_w.saturating_sub(7)).into_iter().take(4).enumerate() {
        lines.push(Line::from(vec![Span::styled(format!(" {}", if i == 0 { "why:  " } else { "      " }), t.muted), Span::raw(l)]));
    }
    let world = if card.enforced_by.is_empty() { card.world.clone() } else { format!("{} ({})", card.world, card.enforced_by) };
    lines.push(Line::from(Span::styled(format!(" world: {world}  ·  lease: {}", card.lease), t.muted)));
    lines.push(Line::from(""));
    let key = |k: &str| Span::styled(k.to_string(), t.key);
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
        lines.push(Line::from(Span::styled(format!(" (+{queued} more waiting)"), t.dim)));
    }
    let height = lines.len() as u16 + 2;
    let rect = centered(area, width, height);
    probe.area("approval", rect);
    probe.check(
        height <= area.height && width >= 40,
        format!("approval card needs {width}x{height}; the terminal is {}x{}", area.width, area.height),
    );
    let inner = card_frame(t, rect, " approval ".into(), t.warning, buf);
    for (y, l) in lines.iter().take(inner.height as usize).enumerate() {
        buf.set_line(inner.x, inner.y + y as u16, l, inner.width);
    }
}

fn draw_rewind(t: &Theme, p: &RewindPicker, area: Rect, buf: &mut Buffer, probe: &mut Probe) {
    let width = area.width.saturating_sub(2).min(78);
    let inner_w = width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, m) in MODES.iter().enumerate() {
        let sel = *m == p.selected;
        let text = format!("{} [{}] {:<5} {}", if sel { "▸" } else { " " }, i + 1, mode_name(*m), mode_doc(*m));
        lines.push(Line::from(Span::styled(trunc(&text, inner_w), if sel { t.selected } else { t.text })));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(format!(" plan ({}):", mode_name(p.selected)), t.text.add_modifier(Modifier::BOLD))));
    match &p.plan {
        None => lines.push(Line::from(Span::styled("   computing the plan…", t.dim))),
        Some(Err(e)) => {
            for l in wrap(&format!("cannot rewind: {e}"), inner_w.saturating_sub(3)) {
                lines.push(Line::from(Span::styled(format!("   {l}"), t.error)));
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
            lines.push(Line::from(Span::styled("   a PreRewind checkpoint is taken first, so this rewind can be undone", t.dim)));
        }
    }
    lines.push(Line::from(Span::styled(" [enter] apply   [↑↓ 1-3] mode   [esc] cancel", t.accent)));
    let height = lines.len() as u16 + 2;
    let rect = centered(area, width, height);
    probe.area("rewind", rect);
    probe.check(height <= area.height, format!("rewind picker needs {height} rows; the terminal has {}", area.height));
    let inner = card_frame(t, rect, format!(" rewind to c{} ", p.checkpoint.0), t.info, buf);
    for (y, l) in lines.iter().take(inner.height as usize).enumerate() {
        buf.set_line(inner.x, inner.y + y as u16, l, inner.width);
    }
}
