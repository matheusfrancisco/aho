//! `aho plan --patch` / every UI toggle: the state diff shown before anything is written (05 rule 1).

use std::collections::BTreeSet;

use aho_cap::{Missing, PlanRow};
use aho_log::CapState;
use aho_types::*;
use serde::Serialize;

use crate::Planned;

#[derive(Clone, Debug, Default, Serialize)]
pub struct PlanDiff {
    pub caps: Vec<CapFlip>,
    pub config: Vec<ConfigChange>,
    pub world: Option<(String, String)>,
    pub preset: Option<(String, String)>,
    /// `(gained, lost)` in the preset's allowed set.
    pub allowed: Option<(ConsequenceSet, ConsequenceSet)>,
}

impl PlanDiff {
    pub fn is_empty(&self) -> bool {
        self.caps.is_empty() && self.config.is_empty() && self.world.is_none() && self.preset.is_none() && self.allowed.is_none()
    }
}

/// A capability whose state differs. `None` = not linked in that plan.
#[derive(Clone, Debug, Serialize)]
pub struct CapFlip {
    pub id: String,
    pub before: Option<CapState>,
    pub after: Option<CapState>,
    /// Classes the flip makes reachable (the capability becomes Active).
    pub unlocks: ConsequenceSet,
    /// Classes the flip takes away (the capability stops being Active).
    pub revokes: ConsequenceSet,
    /// What the capability still lacks after the change.
    pub missing: Vec<Missing>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConfigChange {
    pub key: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    /// Origin of the new winner (`None` when the key disappeared).
    pub winner: Option<String>,
    /// The previous winner, still present and now outranked by `winner`.
    pub beats: Option<String>,
    /// The previous winner, no longer present in any layer.
    pub replaced: Option<String>,
}

pub fn plan_diff(before: &Planned, after: &Planned) -> PlanDiff {
    let ids: BTreeSet<&str> = before.plan.rows.iter().chain(after.plan.rows.iter()).map(|r| r.id.name).collect();
    let mut caps = Vec::new();
    for id in ids {
        let (a, b) = (before.plan.row(id), after.plan.row(id));
        let (sa, sb) = (a.map(|r| &r.state), b.map(|r| &r.state));
        let over = |r: Option<&PlanRow>| r.map(|r| r.over_preset).unwrap_or_default();
        let active = |r: Option<&PlanRow>| r.is_some_and(|r| matches!(r.state, CapState::Active));
        // A preset change leaves capabilities Active and moves which of their classes the chain allows;
        // for a capability that is not Active on either side the ceiling reaches nothing.
        if sa == sb && (over(a) == over(b) || !(active(a) || active(b))) {
            continue;
        }
        let reachable = |r: Option<&PlanRow>| r.map(|r| ConsequenceSet(r.declared.0 & !r.over_preset.0)).unwrap_or_default();
        let (ra, rb) =
            (if active(a) { reachable(a) } else { ConsequenceSet::EMPTY }, if active(b) { reachable(b) } else { ConsequenceSet::EMPTY });
        let unlocks = ConsequenceSet(rb.0 & !ra.0);
        let revokes = ConsequenceSet(ra.0 & !rb.0);
        caps.push(CapFlip {
            id: id.to_string(),
            before: sa.cloned(),
            after: sb.cloned(),
            unlocks,
            revokes,
            missing: b.map(|r| r.missing.clone()).unwrap_or_default(),
        });
    }

    let config = aho_config::diff(&before.loadout, &after.loadout)
        .into_iter()
        .map(|(key, was, now)| {
            let after_s = after.loadout.config.get(&key);
            let prev = before.loadout.config.get(&key).map(|s| &s.winner);
            let winner = after_s.map(|s| &s.winner);
            let (mut beats, mut replaced) = (None, None);
            if let Some(p) = prev.filter(|p| Some(*p) != winner) {
                let still_there = after_s.is_some_and(|s| s.shadowed.iter().any(|(o, _)| o == p));
                if still_there { beats = Some(p.render()) } else { replaced = Some(p.render()) }
            }
            ConfigChange { key, before: was, after: now, winner: winner.map(|w| w.render()), beats, replaced }
        })
        .collect();

    let changed = |a: &str, b: &str| (a != b).then(|| (a.to_string(), b.to_string()));
    let (pa, pb) = (before.plan.allowed, after.plan.allowed);
    let gained = ConsequenceSet(pb.0 & !pa.0);
    let lost = ConsequenceSet(pa.0 & !pb.0);
    PlanDiff {
        caps,
        config,
        world: changed(&before.plan.world.name, &after.plan.world.name),
        preset: changed(&before.plan.preset, &after.plan.preset),
        allowed: (pa != pb).then_some((gained, lost)),
    }
}

fn state_label(s: Option<&CapState>) -> &'static str {
    match s {
        None => "absent",
        Some(CapState::Active) => "Active",
        Some(CapState::Disabled { .. }) => "Disabled",
        Some(CapState::Locked { .. }) => "Locked",
        Some(CapState::Broken { .. }) => "Broken",
        Some(CapState::Unlocked) => "Unlocked",
    }
}

fn classes(c: ConsequenceSet) -> String {
    c.iter().map(|x| x.name()).collect::<Vec<_>>().join(", ")
}

fn show(v: &Option<Value>) -> String {
    match v {
        None => "(unset)".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(m)) if m.len() == 1 && m.contains_key("digest") => "(secret)".into(),
        Some(other) => other.to_string(),
    }
}

/// The plan block printed before `apply? [y/N]`.
pub fn render_plan_diff(d: &PlanDiff) -> String {
    if d.is_empty() {
        return "  (no change)\n".into();
    }
    let mut out = String::new();
    for f in &d.caps {
        let line = match (f.before.as_ref(), f.after.as_ref()) {
            (Some(CapState::Active), Some(CapState::Active)) => {
                let mut parts = Vec::new();
                if f.unlocks.0 != 0 {
                    parts.push(format!("unlocks: {}", classes(f.unlocks)));
                }
                if f.revokes.0 != 0 {
                    parts.push(format!("revokes: {}", classes(f.revokes)));
                }
                format!("  ~Allows  {:<20}{}", f.id, parts.join("  "))
            }
            (_, Some(CapState::Active)) => {
                let unlocks = if f.unlocks.0 == 0 { String::new() } else { format!("unlocks: {}", classes(f.unlocks)) };
                format!("  +Active  {:<20}{unlocks}", f.id)
            }
            (Some(CapState::Active), after) => {
                let why = match after {
                    Some(CapState::Locked { missing }) => format!("Locked: {}", missing.join(", ")),
                    Some(CapState::Disabled { why }) => format!("Disabled: {why}"),
                    Some(CapState::Broken { err }) => format!("Broken: {err}"),
                    other => state_label(other).to_string(),
                };
                format!("  -Active  {:<20}{why}", f.id)
            }
            (before, after) => {
                let mut s = format!("  ~ {:<26}{} -> {}", f.id, state_label(before), state_label(after));
                if let Some(CapState::Locked { missing }) = after {
                    s.push_str(&format!("   ({})", missing.join(", ")));
                }
                s
            }
        };
        out.push_str(line.trim_end());
        out.push('\n');
        if matches!(f.after, Some(CapState::Locked { .. })) {
            for m in &f.missing {
                if let Some(fix) = &m.fix {
                    out.push_str(&format!("  !        {}   fix: `{fix}`\n", m.requirement));
                }
            }
        }
    }
    for c in &d.config {
        let mut line = format!("  ~ {}  {} -> {}", c.key, show(&c.before), show(&c.after));
        match (&c.winner, &c.beats, &c.replaced) {
            (Some(w), Some(b), _) => line.push_str(&format!("   ({w} beats {b})")),
            (Some(w), None, Some(r)) => line.push_str(&format!("   ({w}; was {r})")),
            (Some(w), None, None) => line.push_str(&format!("   ({w})")),
            (None, _, Some(r)) => line.push_str(&format!("   (was {r})")),
            _ => {}
        }
        out.push_str(&line);
        out.push('\n');
    }
    if let Some((a, b)) = &d.world {
        out.push_str(&format!("  ~ world   {a} -> {b}\n"));
    }
    if let Some((a, b)) = &d.preset {
        out.push_str(&format!("  ~ preset  {a} -> {b}\n"));
    }
    if let Some((gained, lost)) = d.allowed {
        let mut parts = Vec::new();
        if gained.0 != 0 {
            parts.push(format!("+{}", classes(gained)));
        }
        if lost.0 != 0 {
            parts.push(format!("-{}", classes(lost)));
        }
        out.push_str(&format!("  ~ allowed {}\n", parts.join("  ")));
    }
    out
}
