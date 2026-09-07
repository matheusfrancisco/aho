//! Model-facing text: a worker's hydrated input and the graph status.

/// Appends one formatted line (String writes cannot fail).
macro_rules! ln {
    ($out:expr, $($t:tt)*) => {{
        $out.push_str(&format!($($t)*));
        $out.push('\n');
    }};
}

use crate::error::TaskError;
use crate::graph::{Derived, Graph};
use crate::model::{Confidence, HandoffArtifact, Node, TaskId, agent_label};

/// How upstream artifacts enter a dependent's input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Hydrate {
    /// Findings (excerpted), negative space, open questions and the contract; the rest by reference
    /// (`task_status {"id": ..}` returns the full artifact).
    #[default]
    Reference,
    /// Every field inline.
    Inline,
}

/// Findings longer than this are excerpted in `Hydrate::Reference`.
pub const REFERENCE_FINDINGS_CHARS: usize = 800;

fn excerpt(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}… (excerpt)")
}

fn bullets(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    ln!(out, "{title}:");
    for i in items {
        ln!(out, "- {i}");
    }
}

fn render_artifact(out: &mut String, n: &Node, a: &HandoffArtifact, mode: Hydrate) {
    let by = n.last_owner.map(agent_label).unwrap_or_else(|| "unknown".into());
    match mode {
        Hydrate::Reference => {
            ln!(
                out,
                "\n## {} ({}) — confidence {} — by {by}; full artifact: task_status {{\"id\": \"{}\"}}",
                n.id,
                n.kind,
                a.confidence,
                n.id
            );
            ln!(out, "Findings: {}", excerpt(&a.findings, REFERENCE_FINDINGS_CHARS));
        }
        Hydrate::Inline => {
            ln!(out, "\n## {} ({}) — confidence {} — by {by}", n.id, n.kind, a.confidence);
            ln!(out, "Findings: {}", a.findings);
            bullets(out, "Evidence", &a.evidence);
            bullets(out, "Edge cases", &a.edge_cases);
            if let Some(v) = &a.validation {
                ln!(out, "Validation: {v}");
            }
        }
    }
    if a.confidence == Confidence::Low {
        ln!(out, "(LOW confidence — treat as a lead, not a result.)");
    }
    bullets(out, "Not checked by the producer", &a.what_i_did_not_check);
    bullets(out, "Open questions", &a.open_questions);
    if !n.kind.is_gate() && !a.covers.is_empty() {
        let covers: Vec<String> = a.covers.iter().map(TaskId::to_string).collect();
        ln!(out, "Covers: {}", covers.join(", "));
    }
    if let Some(c) = &a.contract {
        ln!(out, "Contract:");
        for o in &c.outputs {
            match &o.hash {
                Some(h) => {
                    ln!(out, "- output {} ({}, blake3 {})", o.path, o.format, h.short());
                }
                None => {
                    ln!(out, "- output {} ({})", o.path, o.format);
                }
            }
        }
        if !c.units.is_empty() {
            ln!(out, "- units: {}", c.units.join("; "));
        }
        if let Some(v) = &c.version {
            ln!(out, "- version: {v}");
        }
        ln!(
            out,
            "Acceptance — run these before trusting this input; if one fails, re-open the producer (task_control op=reopen id={}):",
            n.id
        );
        for a in &c.acceptance {
            ln!(out, "- {a}");
        }
    }
}

impl Graph {
    /// The worker's input: its brief, gate scope or salvage snapshot, then every done upstream
    /// artifact under "# Inputs from completed dependencies".
    pub fn assemble_input(&self, id: &TaskId, mode: Hydrate) -> Result<String, TaskError> {
        let n = self.get(id)?;
        let mut out = String::new();
        let role = if n.expanded { " — synthesis join: its children and their gate are done" } else { "" };
        ln!(out, "# Task {} ({}){role}", n.id, n.kind);
        ln!(out, "{}", n.content.trim());

        if let Some(f) = &n.feedback {
            ln!(out, "\n# Your previous hand-off was re-opened\n{f}");
        }

        if n.kind.is_gate() {
            ln!(
                out,
                "\n# Gate scope\nName every task below in `covers`. A LOW-confidence task blocks the gate until work you inject for it \
                 (naming it in `addresses`) completes at medium or high."
            );
            for d in &n.depends_on {
                if let Some(dn) = self.node(d) {
                    let c = dn.confidence().map(|c| c.as_str()).unwrap_or("none");
                    let flag = if dn.confidence() == Some(Confidence::Low) { "  ← LOW" } else { "" };
                    ln!(out, "- {} ({}) {} · confidence {c}{flag}", dn.id, dn.kind, dn.status);
                }
            }
        }

        if let Some(s) = &n.salvage {
            let from = s.from.map(agent_label).unwrap_or_else(|| "a previous worker".into());
            ln!(out, "\n# Salvaged from {from}\nPick up where it stopped; do not redo finished steps blindly.");
            if let Some(c) = &s.checkpoint {
                ln!(out, "Checkpoint: {c}");
            }
            bullets(&mut out, "Its last tool calls", &s.summaries);
        }

        let inputs: Vec<&Node> = self.inputs(id).iter().filter_map(|d| self.node(d)).filter(|d| d.artifact.is_some()).collect();
        if !inputs.is_empty() {
            ln!(out, "\n# Inputs from completed dependencies");
            for d in inputs {
                if let Some(a) = &d.artifact {
                    render_artifact(&mut out, d, a, mode);
                }
            }
        }

        ln!(out, "\n# Hand-off");
        if n.kind.is_gate() {
            ln!(
                out,
                "Pass with task_complete {{id, artifact}} where artifact.covers lists every task in scope, or turn misses into work with \
                 task_complete {{id, inject: [..]}}."
            );
        } else {
            let contract = if self.settings().deep && n.kind.needs_contract() {
                " and contract {outputs [{path, format, hash?}], units, version, acceptance} (required in deep mode)"
            } else {
                ""
            };
            ln!(
                out,
                "Finish with task_complete {{id, artifact}}: findings, evidence, edge_cases, validation, open_questions, confidence \
                 (low|medium|high), what_i_did_not_check (never empty){contract}. Ending your turn without it requeues the task once, \
                 then fails it."
            );
        }
        Ok(out)
    }

    /// One line per node plus counts and growth accounting.
    pub fn render_status(&self) -> String {
        if self.is_empty() {
            return "The task graph is empty. Seed it with task_seed.".into();
        }
        let (mut done, mut running, mut ready, mut blocked, mut failed, mut stopped) = (0, 0, 0, 0, 0, 0);
        let mut lines = Vec::new();
        for n in self.nodes() {
            let d = self.derived(&n.id);
            let state = match &d {
                Some(Derived::Ready) => {
                    ready += 1;
                    match n.pinned {
                        Some(p) => format!("ready (reassigned to {})", agent_label(p)),
                        None => "ready".into(),
                    }
                }
                Some(Derived::Blocked(on)) => {
                    blocked += 1;
                    let on: Vec<String> = on.iter().map(TaskId::to_string).collect();
                    format!("blocked on {}", on.join(", "))
                }
                Some(Derived::Running(o)) => {
                    running += 1;
                    format!("running {}", o.map(agent_label).unwrap_or_default())
                }
                Some(Derived::Stale(o)) => {
                    running += 1;
                    format!("running (stale since resume) {}", o.map(agent_label).unwrap_or_default())
                }
                Some(Derived::Completed) => {
                    done += 1;
                    let c = n.confidence().map(|c| c.as_str()).unwrap_or("-");
                    format!("completed ({c}) by {}", n.last_owner.map(agent_label).unwrap_or_default())
                }
                Some(Derived::Failed(r)) => {
                    failed += 1;
                    format!("failed: {}", r.map(|r| r.as_str()).unwrap_or("unknown"))
                }
                Some(Derived::Stopped) => {
                    stopped += 1;
                    "stopped".into()
                }
                None => continue,
            };
            lines.push(format!("- {} [{}] {state}", n.id, n.kind));
        }
        let mut s = format!(
            "{} tasks — {done} completed, {running} running, {ready} ready, {blocked} blocked, {failed} failed, {stopped} stopped · {}\n",
            self.len(),
            self.growth()
        );
        s.push_str(&lines.join("\n"));
        s
    }
}
