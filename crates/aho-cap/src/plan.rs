//! Planning (02): seam bindings, exclusive bundles, requirement checks, states, fixes, ceilings. Pure.

use super::*;

// ───────────────────────────────── plan ─────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PlanRow {
    pub id: CapabilityId,
    pub kind: Kind,
    pub state: CapState,
    pub because: Because,
    pub bound: Vec<SeamId>,
    pub ceiling: ConsequenceSet,
    pub declared: ConsequenceSet,
    pub missing: Vec<Missing>,
    /// Classes this capability's tools declare that the active preset does not allow.
    pub over_preset: ConsequenceSet,
    pub enforced_by: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Missing {
    pub requirement: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub enum Because {
    CompositionDefault,
    OnlyProvider,
    Bound {
        key: String,
        origin: String,
    },
    NotBound {
        seam: &'static str,
        chosen: String,
    },
    Disabled {
        origin: String,
    },
    /// Off by an engineer default (`caps.<id>.enabled = false` at the Default/Bundle layer): the
    /// operator enables it (05: `○ Unlocked  enable: aho enable <id>`).
    OptIn {
        origin: String,
    },
    Operator {
        origin: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub rows: Vec<PlanRow>,
    pub seams: BTreeMap<&'static str, &'static str>,
    pub world: BoundWorld,
    pub preset: String,
    pub allowed: ConsequenceSet,
    pub discovered: Vec<Discovered>,
    /// Operator switches for discovered capabilities, resolved before requirement checks.
    pub discovered_states: BTreeMap<String, CapState>,
    pub hash: Hash,
    pub fingerprint: Hash,
}

impl Plan {
    pub fn row(&self, name: &str) -> Option<&PlanRow> {
        self.rows.iter().find(|r| r.id.name == name)
    }
    pub fn active(&self) -> impl Iterator<Item = &PlanRow> {
        self.rows.iter().filter(|r| matches!(r.state, CapState::Active))
    }
}

#[derive(Debug, Clone)]
pub enum PlanError {
    DuplicateId(&'static str),
    UnboundSeam { seam: &'static str, providers: Vec<&'static str> },
    SplitExclusiveBundle { bundle: &'static str, seam: &'static str, conflicting: String },
    UnknownBinding { key: String, value: String },
    HostHandleOnTool(&'static str),
    ClassAboveCeiling { cap: &'static str, tool: &'static str, class: Consequence },
    Cycle(Vec<&'static str>),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::DuplicateId(id) => write!(f, "capability id `{id}` linked twice without `replaces`"),
            PlanError::UnboundSeam { seam, providers } => {
                write!(f, "seam `{seam}` has {} providers ({}) and no binding; set `seams.{seam}`", providers.len(), providers.join(", "))
            }
            PlanError::SplitExclusiveBundle { bundle, seam, conflicting } => {
                write!(f, "`{bundle}` binds its seams together; `seams.{seam} = {conflicting}` would split it")
            }
            PlanError::UnknownBinding { key, value } => write!(f, "`{key} = {value}` names no linked provider"),
            PlanError::HostHandleOnTool(id) => write!(f, "tool `{id}` declares a host handle; only daemon providers may"),
            PlanError::ClassAboveCeiling { cap, tool, class } => {
                write!(f, "`{cap}::{tool}` claims {} but its handles do not admit it", class.name())
            }
            PlanError::Cycle(ids) => write!(f, "capability requirements form a cycle: {}", ids.join(" → ")),
        }
    }
}

impl std::error::Error for PlanError {}

/// Policy inputs to planning (computed by `aho-compose` from `aho-policy` presets).
#[derive(Clone, Debug)]
pub struct PolicyInput {
    pub preset: String,
    pub allowed: ConsequenceSet,
}

/// Pure. `aho plan` = this + rendering.
pub fn plan(
    manifests: &[&'static Manifest],
    loadout: &Loadout,
    env: &Env,
    policy: &PolicyInput,
    discovered: Vec<Discovered>,
) -> Result<Plan, PlanError> {
    // 1. identity
    let mut by_name: BTreeMap<&'static str, &'static Manifest> = BTreeMap::new();
    let replaced: BTreeSet<&'static str> = manifests.iter().flat_map(|m| m.replaces.iter().map(|r| r.name)).collect();
    for m in manifests {
        if let Some(prev) = by_name.insert(m.id.name, m)
            && !(m.replaces.iter().any(|r| r.name == prev.id.name))
        {
            return Err(PlanError::DuplicateId(m.id.name));
        }
    }
    let manifests: Vec<&'static Manifest> =
        by_name.values().copied().filter(|m| !replaced.contains(m.id.name) || m.replaces.is_empty()).collect();

    // 2. static checks
    for m in &manifests {
        if m.kind == Kind::Tool {
            if m.handles.iter().any(|h| is_host_handle(*h)) {
                return Err(PlanError::HostHandleOnTool(m.id.name));
            }
            let ceil = ceiling(m.handles).with(Consequence::TalksToHuman);
            for t in m.tools {
                if !ceil.contains(t.class) {
                    return Err(PlanError::ClassAboveCeiling { cap: m.id.name, tool: t.name, class: t.class });
                }
            }
        }
    }

    // 3. seam bindings
    let mut providers: BTreeMap<&'static str, Vec<&'static Manifest>> = BTreeMap::new();
    for m in &manifests {
        for s in m.provides {
            providers.entry(s.0).or_default().push(m);
        }
    }
    let world_choice = loadout.str("seams.world").map(str::to_string);
    let mut seams: BTreeMap<&'static str, &'static str> = BTreeMap::new();
    let mut because_bound: BTreeMap<&'static str, Because> = BTreeMap::new();
    // exclusive bundles are chosen through `seams.world`
    let bundles: Vec<&'static Manifest> = manifests.iter().copied().filter(|m| m.exclusive).collect();
    if !bundles.is_empty() {
        let chosen = match &world_choice {
            Some(w) => bundles
                .iter()
                .find(|m| m.id.name == w || m.id.name == format!("world-{w}"))
                .copied()
                .ok_or_else(|| PlanError::UnknownBinding { key: "seams.world".into(), value: w.clone() })?,
            None if bundles.len() == 1 => bundles[0],
            None => return Err(PlanError::UnboundSeam { seam: "world", providers: bundles.iter().map(|m| m.id.name).collect() }),
        };
        let origin = loadout.config.get("seams.world").map(|s| s.winner.render()).unwrap_or_else(|| "only world linked".into());
        for s in chosen.provides {
            seams.insert(s.0, chosen.id.name);
        }
        because_bound.insert(chosen.id.name, Because::Bound { key: "seams.world".into(), origin });
    }
    for (seam, ps) in &providers {
        let key = format!("seams.{seam}");
        if let Some(v) = loadout.str(&key) {
            if let Some(owner) = seams.get(seam)
                && *owner != v
                && by_name.get(owner).map(|m| m.exclusive).unwrap_or(false)
            {
                return Err(PlanError::SplitExclusiveBundle { bundle: owner, seam, conflicting: v.to_string() });
            }
            let chosen =
                ps.iter().find(|m| m.id.name == v).ok_or_else(|| PlanError::UnknownBinding { key: key.clone(), value: v.to_string() })?;
            seams.insert(seam, chosen.id.name);
            let origin = loadout.config.get(&key).map(|s| s.winner.render()).unwrap_or_default();
            because_bound.insert(chosen.id.name, Because::Bound { key, origin });
            continue;
        }
        if seams.contains_key(seam) {
            continue;
        }
        let candidates: Vec<&&'static Manifest> = ps.iter().filter(|m| !m.exclusive).collect();
        match candidates.len() {
            0 => {}
            1 => {
                seams.insert(seam, candidates[0].id.name);
                because_bound.entry(candidates[0].id.name).or_insert(Because::OnlyProvider);
            }
            _ => {
                // several non-exclusive providers of one seam are all active when the seam is additive (e.g. llm adapters)
                if ADDITIVE_SEAMS.contains(seam) {
                    seams.insert(seam, candidates[0].id.name);
                } else {
                    return Err(PlanError::UnboundSeam { seam, providers: candidates.iter().map(|m| m.id.name).collect() });
                }
            }
        }
    }

    // 4. world
    let world = seams
        .get("sandbox")
        .and_then(|w| by_name.get(w))
        .map(|m| BoundWorld {
            name: m.id.name.trim_start_matches("world-").to_string(),
            confines: m.enforced_by.map(|e| e != "policy only").unwrap_or(false),
            enforced_by: m.enforced_by.unwrap_or("policy only").to_string(),
        })
        .unwrap_or_else(BoundWorld::host);

    // 5. states in dependency order
    let order = topo(&manifests)?;
    let mut states: BTreeMap<&'static str, CapState> = BTreeMap::new();
    let mut rows = Vec::new();
    for m in order {
        let name = m.id.name;
        let mut missing = Vec::new();
        let (state, because) = if let Some((_, origin)) = loadout.disabled.iter().find(|(d, _)| d == name) {
            (CapState::Disabled { why: origin.render() }, Because::Disabled { origin: origin.render() })
        } else if loadout.get(&format!("caps.{name}.enabled")) == Some(&Value::Bool(false)) {
            let winner = &loadout.config[&format!("caps.{name}.enabled")].winner;
            let o = winner.render();
            if matches!(winner.layer, aho_config::Layer::Default | aho_config::Layer::Bundle(_)) {
                (CapState::Unlocked, Because::OptIn { origin: o })
            } else {
                (CapState::Disabled { why: o.clone() }, Because::Disabled { origin: o })
            }
        } else if !m.provides.is_empty()
            && !m.provides.iter().any(|s| seams.get(s.0) == Some(&name))
            && !m.provides.iter().any(|s| ADDITIVE_SEAMS.contains(&s.0))
        {
            // A world is one exclusive bundle chosen by `seams.world` (07), not by its member seams.
            let (seam, chosen) = if m.exclusive {
                ("world", seams.get(m.provides[0].0).map(|w| w.trim_start_matches("world-")).unwrap_or("none").to_string())
            } else {
                (m.provides[0].0, seams.get(m.provides[0].0).copied().unwrap_or("none").to_string())
            };
            (CapState::Unlocked, Because::NotBound { seam, chosen })
        } else {
            for r in m.requires {
                if let Some(miss) = check(r, &seams, &states, loadout, env) {
                    missing.push(miss);
                }
            }
            // The preset is a per-call ceiling enforced by the decision chain (04); plan only flags
            // capabilities whose tools would need more, so the tree can say "calls needing X will ask/deny".
            let because = because_bound.get(name).cloned().unwrap_or(Because::CompositionDefault);
            if missing.is_empty() {
                (CapState::Active, because)
            } else {
                if missing.iter().all(|x| x.fix.is_none())
                    && let Some(f) = m.unlock
                {
                    missing[0].fix = Some(f.cli.to_string());
                }
                (CapState::Locked { missing: missing.iter().map(|x| x.requirement.clone()).collect() }, because)
            }
        };
        states.insert(name, state.clone());
        rows.push(PlanRow {
            id: m.id,
            kind: m.kind,
            state,
            because,
            bound: m.provides.iter().filter(|s| seams.get(s.0) == Some(&name)).copied().collect(),
            ceiling: ceiling(m.handles),
            declared: m.declared(),
            missing,
            over_preset: ConsequenceSet(m.declared().0 & !policy.allowed.0),
            enforced_by: if m.kind == Kind::Tool { world.enforced_by.clone() } else { m.enforced_by.unwrap_or("").to_string() },
        });
    }
    rows.sort_by_key(|r| (r.kind, r.id.name));

    let fingerprint = fingerprint(&manifests);
    let hash =
        Hash::of_json(&(rows.iter().map(|r| (r.id.name, &r.state)).collect::<Vec<_>>(), &seams, &policy.preset, loadout.hash, fingerprint));
    let discovered_states = discovered
        .iter()
        .map(|d| {
            let skill = d.id.strip_prefix("skill.");
            let key = match skill {
                Some(name) => format!("skills.{name}.enabled"),
                None => format!("caps.{}.enabled", d.id),
            };
            let state = if let Some((_, origin)) = loadout.disabled.iter().find(|(id, _)| id == &d.id) {
                CapState::Disabled { why: origin.render() }
            } else if let Some(s) = loadout.config.get(&key).filter(|s| s.value == Value::Bool(false)) {
                if matches!(s.winner.layer, aho_config::Layer::Default | aho_config::Layer::Bundle(_)) {
                    CapState::Unlocked
                } else {
                    CapState::Disabled { why: s.winner.render() }
                }
            } else if let Some(err) = &d.broken {
                CapState::Broken { err: err.clone() }
            } else if skill.is_some() && loadout.get(&key) != Some(&Value::Bool(true)) {
                CapState::Unlocked
            } else {
                // Switched on: Active only while what it follows is Active (05: Locked names the fix).
                let active = |name: &str| matches!(states.get(name), Some(CapState::Active));
                let parents: Vec<&str> = rows.iter().map(|r| r.id.name).filter(|name| follows(d, name)).collect();
                let mut missing: Vec<String> = parents.iter().filter(|p| !active(p)).map(|p| format!("(follows {p})")).collect();
                missing.extend(d.requires.iter().filter(|r| !active(r) && !parents.contains(&r.as_str())).map(|r| format!("Cap({r})")));
                if missing.is_empty() { CapState::Active } else { CapState::Locked { missing } }
            };
            (d.id.clone(), state)
        })
        .collect();
    Ok(Plan {
        rows,
        seams,
        world,
        preset: policy.preset.clone(),
        allowed: policy.allowed,
        discovered,
        discovered_states,
        hash,
        fingerprint,
    })
}

/// Whether discovered `d` follows plan row `row`: it names the row in `requires`, or it is one of
/// the row's tools (`mcp__github__create_issue` follows `mcp.github`).
pub fn follows(d: &Discovered, row: &str) -> bool {
    d.requires.iter().any(|r| r == row) || d.id.starts_with(&format!("{}__", row.replace('.', "__")))
}

/// Seams where several providers are all live at once (model adapters, embedders); bindings pick a default.
pub const ADDITIVE_SEAMS: &[&str] = &["llm", "hooks", "skills", "mcp"];

pub fn fingerprint(manifests: &[&'static Manifest]) -> Hash {
    let mut ids: Vec<(&str, u32, Hash)> = manifests.iter().map(|m| (m.id.name, m.id.major, m.hash())).collect();
    ids.sort();
    Hash::of_json(&(ids, env!("CARGO_PKG_VERSION")))
}

fn check(r: &Requirement, seams: &BTreeMap<&str, &str>, states: &BTreeMap<&str, CapState>, l: &Loadout, env: &Env) -> Option<Missing> {
    let ok = |_: ()| None;
    match r {
        Requirement::Seam(s) => match seams.get(s.0) {
            Some(p) if matches!(states.get(p), Some(CapState::Active)) => ok(()),
            Some(p) => Some(Missing {
                requirement: format!("Seam({}) — provider `{p}` is not active", s.0),
                fix: Some(format!("aho explain {p}")),
            }),
            None => Some(Missing { requirement: format!("Seam({})", s.0), fix: Some(format!("aho add provider <{}-provider>", s.0)) }),
        },
        Requirement::Cap(c) => match states.get(c) {
            Some(CapState::Active) => ok(()),
            _ => Some(Missing { requirement: format!("Cap({c})"), fix: Some(format!("aho enable {c}")) }),
        },
        Requirement::AnyOf(rs) => {
            let misses: Vec<Missing> = rs.iter().filter_map(|x| check(x, seams, states, l, env)).collect();
            if misses.len() < rs.len() {
                None
            } else {
                Some(Missing {
                    requirement: format!("AnyOf({})", misses.iter().map(|m| m.requirement.as_str()).collect::<Vec<_>>().join(" | ")),
                    fix: misses.into_iter().find_map(|m| m.fix),
                })
            }
        }
        Requirement::Setting { key, pred } => {
            let v = l.get(key);
            let pass = match pred {
                Pred::Eq(x) => v.and_then(|v| v.as_str()) == Some(*x),
                Pred::OneOf(xs) => v.and_then(|v| v.as_str()).map(|s| xs.contains(&s)).unwrap_or(false),
                Pred::Set => v.is_some(),
                Pred::True => v == Some(&Value::Bool(true)),
            };
            if pass {
                None
            } else {
                let want = match pred {
                    Pred::Eq(x) => x.to_string(),
                    Pred::OneOf(xs) => xs[0].to_string(),
                    Pred::Set => "<value>".into(),
                    Pred::True => "true".into(),
                };
                Some(Missing { requirement: format!("Setting({key} {pred:?})"), fix: Some(format!("aho set {key} {want}")) })
            }
        }
        Requirement::Credential(c) => {
            // An empty stored value is a cleared credential (`aho logout`), not a present one.
            if env.credentials.contains(*c) || l.secrets.iter().any(|(k, s)| k.contains(c) && !s.expose().is_empty()) {
                None
            } else {
                Some(Missing { requirement: format!("Credential({c})"), fix: Some(format!("aho credential set {c}")) })
            }
        }
        Requirement::Binary(b) => {
            if env.binaries.contains(*b) {
                None
            } else {
                Some(Missing { requirement: format!("Binary({b})"), fix: Some("./install-system-deps.sh".into()) })
            }
        }
        Requirement::Platform(os) => {
            if env.os == *os {
                None
            } else {
                Some(Missing { requirement: format!("Platform({os:?})"), fix: Some("aho set seams.world vm".into()) })
            }
        }
        Requirement::Feature(f) => {
            if env.features.contains(*f) {
                None
            } else {
                Some(Missing { requirement: format!("Feature({f})"), fix: Some(format!("cargo build --features {f}")) })
            }
        }
    }
}

/// Every binary name any requirement mentions (the I/O shim probes PATH for exactly these).
pub fn binaries_needed(manifests: &[&'static Manifest]) -> BTreeSet<&'static str> {
    fn walk(r: &Requirement, out: &mut BTreeSet<&'static str>) {
        match r {
            Requirement::Binary(b) => {
                out.insert(b);
            }
            Requirement::AnyOf(rs) => rs.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    for m in manifests {
        m.requires.iter().for_each(|r| walk(r, &mut out));
    }
    out
}

pub(crate) fn topo(manifests: &[&'static Manifest]) -> Result<Vec<&'static Manifest>, PlanError> {
    let providers_of = |seam: &str| -> Vec<&'static str> {
        manifests.iter().filter(|m| m.provides.iter().any(|s| s.0 == seam)).map(|m| m.id.name).collect()
    };
    let hprov =
        |h: HandleKind| -> Vec<&'static str> { manifests.iter().filter(|m| m.provides_handles.contains(&h)).map(|m| m.id.name).collect() };
    let mut deps: BTreeMap<&'static str, BTreeSet<&'static str>> = BTreeMap::new();
    for m in manifests {
        let mut d = BTreeSet::new();
        fn reqs(r: &Requirement, out: &mut Vec<&'static str>, seams: &mut Vec<&'static str>) {
            match r {
                Requirement::Cap(c) => out.push(c),
                Requirement::Seam(s) => seams.push(s.0),
                Requirement::AnyOf(rs) => rs.iter().for_each(|x| reqs(x, out, seams)),
                _ => {}
            }
        }
        let (mut caps, mut seams) = (Vec::new(), Vec::new());
        m.requires.iter().for_each(|r| reqs(r, &mut caps, &mut seams));
        d.extend(caps);
        for s in seams {
            d.extend(providers_of(s));
        }
        for h in m.handles {
            d.extend(hprov(*h));
        }
        d.remove(m.id.name);
        deps.insert(m.id.name, d);
    }
    let by: BTreeMap<&str, &'static Manifest> = manifests.iter().map(|m| (m.id.name, *m)).collect();
    let mut out = Vec::new();
    let mut done: BTreeSet<&'static str> = BTreeSet::new();
    let mut remaining: Vec<&'static str> = by.keys().copied().collect();
    while !remaining.is_empty() {
        let ready: Vec<&'static str> =
            remaining.iter().copied().filter(|n| deps[n].iter().all(|d| done.contains(d) || !by.contains_key(d))).collect();
        if ready.is_empty() {
            return Err(PlanError::Cycle(remaining));
        }
        for n in ready {
            done.insert(n);
            out.push(by[n]);
            remaining.retain(|x| *x != n);
        }
    }
    Ok(out)
}
