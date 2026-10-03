//! What a boot finds that the plan alone cannot: a capability Active in the plan whose build
//! failed (a world that could not open, and everything that needed its handles). The plan is
//! pure, so these errors exist only in a session log (`CapDelta::Broken`) and in the booted
//! session; [`with_boot`] lays the logged ones over the plan for `aho plan`/`tree`/`why`/`status`,
//! and [`world_refusal`] is the rule `aho run` uses to refuse a session without its world.

use aho_cap::{CapabilityId, Kind, Manifest, PlanRow, Requirement};
use aho_log::{CapState, State};

use crate::Planned;

/// `p` with every row it has Active that the session `state` last recorded Broken shown as
/// Broken with that error. A log sealed under a different plan says nothing about this one.
pub fn with_boot(p: &Planned, state: &State) -> Planned {
    let mut out = p.clone();
    if state.plan_hash != Some(p.plan.hash) {
        return out;
    }
    for row in &mut out.plan.rows {
        if let (CapState::Active, Some(CapState::Broken { err })) = (&row.state, state.caps.get(row.id.name)) {
            row.state = CapState::Broken { err: err.clone() };
        }
    }
    out
}

/// Why a session must not start: the selected world is not usable and either it failed to
/// build, or coding capabilities the operator left on lost their file/process handles with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldRefusal {
    /// The world provider (`world-vm`).
    pub world: String,
    /// `Broken: <its own error>`, `Locked: <missing requirements>`, `Disabled: <origin>`; `None`
    /// when the world is Active and only a coding capability lost a handle.
    pub world_error: Option<String>,
    /// Coding capabilities left without their world, each with the reason it is down.
    pub affected: Vec<(String, String)>,
    /// The commands that fix it, in order: the world's own fixes, then `aho why <world>`.
    pub fixes: Vec<String>,
}

impl std::fmt::Display for WorldRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the session was not started: ")?;
        match &self.world_error {
            Some(e) => write!(f, "the world `{}` is {e}", self.world)?,
            None => write!(f, "the world `{}` is up, but coding tools lost its handles", self.world)?,
        }
        for (id, why) in &self.affected {
            write!(f, "\n  {id} has no world: {why}")?;
        }
        for (i, fix) in self.fixes.iter().enumerate() {
            write!(f, "\n{} `{fix}`", if i == 0 { "fix:" } else { "    " })?;
        }
        Ok(())
    }
}

/// The world row: the exclusive provider `seams.world` bound.
fn world_row(p: &Planned) -> Option<(&PlanRow, &'static Manifest)> {
    p.plan.rows.iter().filter(|r| r.kind == Kind::Provider && !r.bound.is_empty()).find_map(|r| {
        let m = p.manifests.iter().copied().find(|m| m.id == r.id && m.exclusive)?;
        Some((r, m))
    })
}

fn needs_seam_of(r: &Requirement, p: &Planned, world: &str) -> bool {
    match r {
        Requirement::Seam(s) => p.plan.seams.get(s.0) == Some(&world),
        Requirement::AnyOf(rs) => rs.iter().all(|r| needs_seam_of(r, p, world)),
        _ => false,
    }
}

/// Refuse when the selected world failed to build (`broken` is what the boot reported, `[]`
/// before booting), or when a capability in `coding` that the plan did not switch off is
/// Broken for a missing handle or Locked on a seam the world provides while the world is down.
/// Disabled and opt-in (Unlocked) coding capabilities never cause a refusal.
pub fn world_refusal(p: &Planned, broken: &[(CapabilityId, String)], coding: &[&str]) -> Option<WorldRefusal> {
    let broken_of = |name: &str| broken.iter().find(|(id, _)| id.name == name).map(|(_, e)| e.as_str());
    let world = world_row(p);
    let world_name = world.map(|(r, _)| r.id.name).unwrap_or_default();
    let world_built = world.and_then(|(r, _)| broken_of(r.id.name));
    let world_error = match (world, world_built) {
        (Some(_), Some(e)) => Some(format!("Broken: {e}")),
        (Some((r, _)), None) => match &r.state {
            CapState::Active => None,
            CapState::Broken { err } => Some(format!("Broken: {err}")),
            CapState::Locked { missing } => Some(format!("Locked: {}", missing.join(", "))),
            CapState::Disabled { why } => Some(format!("Disabled: {why}")),
            CapState::Unlocked => Some("not switched on".into()),
        },
        (None, _) => None,
    };
    let mut affected = Vec::new();
    for id in coding {
        let (Some(row), Some(m)) = (p.plan.row(id), p.manifests.iter().find(|m| m.id.name == *id)) else { continue };
        let why = match (&row.state, broken_of(id)) {
            (CapState::Active, Some(e)) if e.starts_with("missing handle") => {
                Some(e.split_once(&format!(": {world_name} is Broken")).map_or(e, |(head, _)| head).to_string())
            }
            (CapState::Locked { missing }, _) if world_error.is_some() && m.requires.iter().any(|r| needs_seam_of(r, p, world_name)) => {
                Some(missing.join(", "))
            }
            _ => None,
        };
        if let Some(why) = why {
            affected.push((id.to_string(), why));
        }
    }
    if world_built.is_none() && affected.is_empty() {
        return None;
    }
    let mut fixes: Vec<String> = Vec::new();
    if let Some((r, m)) = world {
        let own = r.missing.iter().filter_map(|x| x.fix.clone()).chain(m.unlock.map(|f| f.cli.to_string()));
        for fix in own.chain([format!("aho why {}", r.id.name)]) {
            if !fixes.contains(&fix) {
                fixes.push(fix);
            }
        }
    }
    Some(WorldRefusal { world: world_name.to_string(), world_error, affected, fixes })
}

#[cfg(test)]
mod tests;
