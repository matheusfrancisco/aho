//! The pinned jobs panel above the prompt: one header row and one row per session job (a detached
//! op) still running. Hidden while none runs; turn ops show in the transcript, not here.

use aho_proc::OpInfo;
use aho_types::Timestamp;
use ratatui::text::{Line, Span};
use serde_json::{Value, json};

use crate::model::UiState;
use crate::render::trunc;

#[cfg(test)]
mod tests;

/// Job rows the panel shows; the rest collapse into one `…and K more` row.
pub const PANEL_ROWS: usize = 4;

/// Argument names a job is labelled by, first present wins (subagent `label`, bash `command`).
const LABEL_ARGS: [&str; 5] = ["label", "description", "name", "agent", "command"];
const LABEL_MAX: usize = 120;

/// What the panel names a job by: the first string among [`LABEL_ARGS`], on one line.
pub fn label(args: &Value) -> Option<String> {
    let s = LABEL_ARGS.iter().find_map(|k| args.get(*k).and_then(Value::as_str))?;
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!one_line.is_empty()).then(|| trunc(&one_line, LABEL_MAX))
}

/// `12s`, `3m04s`, `1h02m`.
pub fn elapsed(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

pub fn running(ui: &UiState) -> impl Iterator<Item = &OpInfo> {
    ui.jobs.iter().filter(|j| j.detached && j.status.is_none())
}

/// Rows the panel takes: 0 while no job runs.
pub fn panel_height(ui: &UiState) -> u16 {
    let n = running(ui).count();
    match n {
        0 => 0,
        _ => 1 + n.min(PANEL_ROWS) as u16 + u16::from(n > PANEL_ROWS),
    }
}

fn row_text(ui: &UiState, j: &OpInfo, now: Timestamp) -> String {
    let age = elapsed(now.0.saturating_sub(j.started.0));
    match ui.job_labels.get(&j.op).filter(|l| !l.is_empty()) {
        Some(l) => format!("  ⋯ op{} · {} · {age} · {l}", j.op.0, j.tool),
        None => format!("  ⋯ op{} · {} · {age}", j.op.0, j.tool),
    }
}

/// The panel's rows, cut to `width`.
pub fn panel_lines(ui: &UiState, width: usize, now: Timestamp) -> Vec<Line<'static>> {
    let t = &ui.theme;
    let jobs: Vec<&OpInfo> = running(ui).collect();
    if jobs.is_empty() {
        return Vec::new();
    }
    let mut out = vec![Line::from(vec![
        Span::styled(" jobs", t.title),
        Span::styled(trunc(&format!(" · {} running · /jobs lists every op", jobs.len()), width.saturating_sub(5)), t.muted),
    ])];
    for j in jobs.iter().take(PANEL_ROWS) {
        out.push(Line::from(Span::styled(trunc(&row_text(ui, j, now), width), t.warning)));
    }
    if jobs.len() > PANEL_ROWS {
        out.push(Line::from(Span::styled(trunc(&format!("  …and {} more", jobs.len() - PANEL_ROWS), width), t.dim)));
    }
    out
}

/// The panel for the headless screen record: `null` while hidden.
pub fn panel_json(ui: &UiState, now: Timestamp) -> Value {
    let jobs: Vec<&OpInfo> = running(ui).collect();
    if jobs.is_empty() {
        return Value::Null;
    }
    json!({
        "running": jobs.len(),
        "rows": jobs.iter().map(|j| json!({
            "op": j.op.0,
            "tool": j.tool,
            "label": ui.job_labels.get(&j.op).cloned().unwrap_or_default(),
            "elapsed": elapsed(now.0.saturating_sub(j.started.0)),
        })).collect::<Vec<_>>(),
    })
}
