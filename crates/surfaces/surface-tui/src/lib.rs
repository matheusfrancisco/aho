//! The terminal surface. One interactive owner per session; the only state it keeps
//! is surface-local (draft, cursor, scroll, focus, sidebar tab, HUD); the transcript, tree,
//! inventory, jobs and invariants are folds of the session log and plan.
//!
//! Visual debug: [`render_to_buffer`] is a pure function of [`UiState`], every drawn frame is
//! captured into a [`FrameCapture`] ring (rects, text per area, timing, `check` anomalies),
//! identical frames are dropped unless anomalous, and [`render_frame_json`] is the
//! `aho tui --headless` screen.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

mod frames;
mod keys;
mod model;
mod render;
mod runtime;
mod slash;

use aho_cap::*;

pub use frames::{Frame, FrameCapture, LAYOUT_OSCILLATION, Probe, buffer_lines};
pub use keys::{Action, on_key, open_rewind};
pub use model::{
    ApprovalView, Block, Body, Focus, Local, MODES, PlanInfo, ResultStatus, RewindPicker, RewindPlan, TABS, Tab, ToolCard, ToolResultView,
    Transcript, UiState, compact, invariant_rows, mode_doc, mode_name, principal_label, reason_label, tool_caps,
};
pub use render::{MIN_HEIGHT, MIN_WIDTH, capture, draw, render_frame_json, render_to_buffer, trunc, wrap};
pub use runtime::{FRAME_RING, rewind_plan, run, ui_from_session};
pub use slash::{Commands, HELP, Handler, Parsed, Reply, parse_line};

pub struct SurfaceTui;

impl Capability for SurfaceTui {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
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
    doc = "Terminal UI: transcript with tool cards and checkpoint markers, approval card, tree/inventory/jobs/invariants sidebar, HUD."
);

/// `aho tui --headless`: the screen the TUI would draw for `session` right now, as normalized JSON.
pub fn headless(
    session: &aho_session::Session,
    planned: &aho_compose::Planned,
    width: u16,
    height: u16,
) -> Result<serde_json::Value, String> {
    let ui = ui_from_session(session, planned)?;
    Ok(render_frame_json(&ui, width, height))
}

#[cfg(test)]
mod tests;
