//! The terminal surface. One interactive owner per session; the only state it keeps is
//! surface-local (draft, cursor, the `/` menu, open overlays, HUD); the transcript, tree,
//! inventory, jobs and invariants are folds of the session log and plan.
//!
//! One column: the transcript, the prompt box (its border is the status line), the `/` menu,
//! one row of hints. `/tree`, `/inventory`, `/jobs`, `/invariants`, `/rewind`, `/help` and long
//! command answers open closable overlays in front of the transcript. Colours come from a
//! [`Theme`] detected from the terminal. Inline by default: finished blocks are written to the
//! terminal's scrollback ([`InlineScreen`]) and only the bottom is redrawn.
//!
//! Visual debug: [`render_to_buffer`] is a pure function of [`UiState`], every drawn frame is
//! captured into a [`FrameCapture`] ring (rects, text per area, timing, `check` anomalies),
//! identical frames are dropped unless anomalous, and [`render_frame_json`] is the
//! `aho tui --headless` screen.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

mod frames;
mod inline;
mod keys;
mod markdown;
mod menu;
mod model;
mod render;
mod runtime;
mod slash;
mod theme;

use aho_cap::*;
use aho_config::{ConfigKey, Ty};

pub use frames::{Frame, FrameCapture, LAYOUT_OSCILLATION, Probe, buffer_lines};
pub use inline::{InlineScreen, Reopen};
pub use keys::{Action, on_key, open_rewind};
pub use menu::{MENU_ROWS, Suggestion, VIEWS, suggestions};
pub use model::{
    ApprovalView, Block, Body, CheckpointItem, CheckpointList, Local, MODES, Notice, Overlay, Panel, PanelBody, PlanInfo, ResultStatus,
    RewindPicker, RewindPlan, Tone, ToolCard, ToolResultView, Transcript, UiState, View, compact, invariant_rows, mode_doc, mode_name,
    principal_label, reason_label, tool_caps,
};
pub use render::{
    MIN_HEIGHT, MIN_WIDTH, block_lines, capture, draw, draw_inline, front, inline_height, render_frame_json, render_to_buffer, trunc, wrap,
};
pub use runtime::{FRAME_RING, RunOptions, rewind_plan, run, ui_from_session};
pub use slash::{CommandInfo, Commands, HELP, Handler, Parsed, Reply, parse_line};
pub use theme::{ColorMode, Theme};

pub struct SurfaceTui;
#[derive(Clone, Debug)]
pub struct TuiConfig;

impl Config for TuiConfig {
    const PREFIX: &'static str = "tui";

    fn keys() -> Vec<ConfigKey> {
        vec![ConfigKey::new(
            "tui.thinking",
            Ty::Bool,
            Some(serde_json::json!(true)),
            "Show model reasoning in the TUI transcript; `/thinking` toggles it for one run.",
        )]
    }

    fn from_loadout(_: &aho_config::Loadout) -> Result<Self, ConfigError> {
        Ok(TuiConfig)
    }
}

impl Capability for SurfaceTui {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = TuiConfig;
    fn build(_: (), _: TuiConfig) -> Result<Self, BuildError> {
        Ok(SurfaceTui)
    }
    fn register(&self, _r: &mut Registrar<'_>) {}
}

declare_capability!(
    SurfaceTui,
    "surface-tui",
    kind = Kind::Surface,
    provides = [],
    requires = [],
    tools = &[],
    doc = "Terminal UI: one column (transcript, prompt whose border is the status line: model, workspace, spend, context gauge; hints), inline in the terminal's scrollback; themed tool cards and checkpoint markers; `/` command menu with Tab completion; /tree /inventory /jobs /invariants /rewind /help as closable overlays; approval card; HUD."
);

/// `aho tui --headless`: the screen the TUI would draw for `session` right now, as normalized JSON.
/// `workspace` is the path the status line shows (see [`RunOptions::workspace`]).
pub fn headless(
    session: &aho_session::Session,
    planned: &aho_compose::Planned,
    workspace: &str,
    width: u16,
    height: u16,
) -> Result<serde_json::Value, String> {
    let mut ui = ui_from_session(session, planned)?;
    ui.plan.workspace = workspace.to_string();
    Ok(render_frame_json(&ui, width, height))
}

#[cfg(test)]
mod tests;
