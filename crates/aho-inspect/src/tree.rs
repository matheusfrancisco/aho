//! `aho tree`, `aho inventory`, and `aho_why`: one projection of the plan, three renderings.

use std::collections::BTreeMap;

use aho_cap::{Because, CapOrigin, Discovered, Kind, Plan, PlanRow};
use aho_config::Sourced;
use aho_log::CapState;
use aho_types::*;
use serde::Serialize;

use crate::{glyph, render_classes, show_value, state_name, suggest_preset};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Missing {
    pub requirement: String,
    pub fix: Option<String>,
}

/// A setting that governs a node's state, with the origin that won. `value` is known once the
/// node has been [`govern`]ed against resolved config; the plan alone records only origins.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Setting {
    pub key: String,
    pub value: Option<Value>,
    pub origin: String,
}

impl Setting {
    fn of(key: impl Into<String>, origin: &str) -> Setting {
        Setting { key: key.into(), value: None, origin: origin.to_string() }
    }
    fn render(&self) -> String {
        match &self.value {
            Some(v) => format!("{} = {} ← {}", self.key, show_value(v), self.origin),
            None => format!("{} ← {}", self.key, self.origin),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TreeNode {
    pub id: String,
    /// `provider tool skill mcp recipe media surface panel hook template …`
    pub kind: String,
    /// `bundled` for everything linked into the composition; the discoverer's origin otherwise.
    pub origin: String,
    pub source: Option<String>,
    pub state: CapState,
    pub because: String,
    pub bound: Vec<String>,
    pub classes: ConsequenceSet,
    pub ceiling: ConsequenceSet,
    pub enforced_by: String,
    pub settings: Vec<Setting>,
    pub missing: Vec<Missing>,
    /// The one command that changes this node's state (Locked: its first fix). `None` when Active.
    pub action: Option<String>,
    /// Classes this capability's tools declare that the preset does not allow: calls needing them
    /// ask or are denied by the decision chain. A note, never a lock.
    pub over_preset: Option<Missing>,
    pub doc: String,
    pub children: Vec<TreeNode>,
}

const LABELLED_PREFIXES: &[&str] = &["skill", "mcp", "recipe", "media", "theme"];

fn kind_label(kind: Kind, id: &str) -> String {
    if let Some((head, _)) = id.split_once('.').or_else(|| id.split_once("__"))
        && LABELLED_PREFIXES.contains(&head)
    {
        return head.to_string();
    }
    match kind {
        Kind::Provider => "provider",
        Kind::Tool => "tool",
        Kind::Surface => "surface",
        Kind::Panel => "panel",
        Kind::Hook => "hook",
        Kind::PromptSection => "section",
        Kind::Template => "template",
        Kind::Policy => "policy",
    }
    .to_string()
}

fn origin_label(o: CapOrigin) -> &'static str {
    match o {
        CapOrigin::Bundled => "bundled",
        CapOrigin::UserInstalled => "user",
        CapOrigin::Workspace => "workspace",
        CapOrigin::Experimental => "experimental",
    }
}

fn because_text(b: &Because) -> String {
    match b {
        Because::CompositionDefault => "composition default".into(),
        Because::OnlyProvider => "only provider".into(),
        Because::Bound { key, origin } => format!("{key} ← {origin}"),
        Because::NotBound { seam, chosen } => format!("seams.{seam} is bound to {chosen}"),
        Because::Disabled { origin } => format!("disabled ← {origin}"),
        Because::OptIn { origin } => format!("opt-in, off by default ← {origin}"),
        Because::Operator { origin } => format!("operator ← {origin}"),
    }
}

fn because_settings(id: &str, b: &Because) -> Vec<Setting> {
    match b {
        Because::Bound { key, origin } => vec![Setting::of(key.as_str(), origin)],
        Because::Disabled { origin } => vec![Setting::of("disabled", origin)],
        Because::OptIn { origin } => vec![Setting::of(format!("caps.{id}.enabled"), origin)],
        Because::Operator { origin } => vec![Setting::of(format!("caps.{id}"), origin)],
        _ => Vec::new(),
    }
}

fn first_fix(missing: &[Missing]) -> Option<String> {
    missing.iter().find_map(|m| m.fix.clone())
}

fn row_node(row: &PlanRow) -> TreeNode {
    let id = row.id.name;
    let missing: Vec<Missing> = row.missing.iter().map(|m| Missing { requirement: m.requirement.clone(), fix: m.fix.clone() }).collect();
    let action = match (&row.state, &row.because) {
        (CapState::Active, _) => None,
        (CapState::Unlocked, Because::NotBound { seam: "world", .. }) => {
            Some(format!("aho set seams.world {}", id.trim_start_matches("world-")))
        }
        (CapState::Unlocked, Because::NotBound { seam, .. }) => Some(format!("aho set seams.{seam} {id}")),
        (CapState::Unlocked | CapState::Disabled { .. }, _) => Some(format!("aho enable {id}")),
        (CapState::Locked { .. }, _) => first_fix(&missing).or_else(|| Some(format!("aho explain {id}"))),
        (CapState::Broken { .. }, _) => Some(format!("aho explain {id}")),
    };
    TreeNode {
        id: id.to_string(),
        kind: kind_label(row.kind, id),
        origin: "bundled".into(),
        source: None,
        state: row.state.clone(),
        because: because_text(&row.because),
        bound: row.bound.iter().map(|s| s.0.to_string()).collect(),
        classes: row.declared,
        ceiling: row.ceiling,
        enforced_by: row.enforced_by.clone(),
        settings: because_settings(id, &row.because),
        missing,
        action,
        over_preset: None,
        doc: String::new(),
        children: Vec::new(),
    }
}

fn discovered_node(d: &Discovered, plan: &Plan, parent: Option<&TreeNode>) -> TreeNode {
    let mut missing = Vec::new();
    if let Some(p) = parent
        && !matches!(p.state, CapState::Active)
    {
        missing.push(Missing { requirement: format!("(follows {})", p.id), fix: p.action.clone() });
    }
    for r in d.requires.iter().filter(|r| parent.is_none_or(|p| p.id != **r)) {
        match plan.row(r) {
            Some(row) if matches!(row.state, CapState::Active) => {}
            Some(row) if matches!(row.state, CapState::Locked { .. }) => missing.push(Missing {
                requirement: format!("Cap({r})"),
                fix: row.missing.iter().find_map(|m| m.fix.clone()).or_else(|| Some(format!("aho explain {r}"))),
            }),
            _ => missing.push(Missing { requirement: format!("Cap({r})"), fix: Some(format!("aho enable {r}")) }),
        }
    }
    let over = ConsequenceSet(d.classes.0 & !plan.allowed.0);
    let over_preset = (over.0 != 0).then(|| {
        let names: Vec<&str> = over.iter().map(|c| c.name()).collect();
        Missing {
            requirement: format!("Preset({} does not allow {})", plan.preset, names.join(", ")),
            fix: Some(format!("aho preset {}  |  aho allow {}", suggest_preset(over), d.id)),
        }
    });
    // The plan decides the state (switches and requirements); `missing` here only adds the fixes.
    let state = match plan.discovered_states.get(&d.id) {
        Some(CapState::Locked { .. }) => CapState::Locked { missing: missing.iter().map(|m| m.requirement.clone()).collect() },
        Some(state) => state.clone(),
        None if missing.is_empty() => CapState::Active,
        None => CapState::Locked { missing: missing.iter().map(|m| m.requirement.clone()).collect() },
    };
    let action = match &state {
        CapState::Active => None,
        CapState::Broken { .. } => Some(format!("aho explain {}", d.id)),
        CapState::Unlocked | CapState::Disabled { .. } => Some(format!("aho enable {}", d.id)),
        _ => first_fix(&missing),
    };
    let settings = match &state {
        CapState::Disabled { why } => vec![Setting::of("disabled", why)],
        _ => Vec::new(),
    };
    TreeNode {
        id: d.id.clone(),
        kind: kind_label(d.kind, &d.id),
        origin: origin_label(d.origin).into(),
        source: d.source_path.clone(),
        state,
        because: format!("discovered ({})", origin_label(d.origin)),
        bound: Vec::new(),
        classes: d.classes,
        ceiling: d.classes,
        enforced_by: plan.world.enforced_by.clone(),
        settings,
        missing,
        action,
        over_preset,
        doc: d.doc.clone(),
        children: Vec::new(),
    }
}

/// Derived from the plan (manifests + resolved config + discovered capabilities); never
/// hand-maintained. Discovered capabilities that follow a planned one appear as its children.
pub fn tree(plan: &Plan) -> Vec<TreeNode> {
    let mut nodes: Vec<TreeNode> = plan
        .rows
        .iter()
        .map(|row| {
            let mut n = row_node(row);
            if row.over_preset.0 != 0 {
                let names: Vec<&str> = row.over_preset.iter().map(|c| c.name()).collect();
                n.over_preset = Some(Missing {
                    requirement: format!("Preset({} does not allow {})", plan.preset, names.join(", ")),
                    fix: Some(format!("aho preset {}  |  aho allow {}", suggest_preset(row.over_preset), row.id.name)),
                });
            }
            n
        })
        .collect();
    let mut loose = Vec::new();
    for d in &plan.discovered {
        match nodes.iter().position(|n| aho_cap::follows(d, &n.id)) {
            Some(i) => {
                let child = discovered_node(d, plan, Some(&nodes[i]));
                nodes[i].children.push(child);
            }
            None => loose.push(discovered_node(d, plan, None)),
        }
    }
    nodes.extend(loose);
    nodes
}

/// Attaches the resolved values of the settings that govern each node: every `caps.<id>.*` key
/// and `seams.<seam>` for each seam it binds, with the winning origin. `config` is a loadout's or
/// a folded `State`'s table. Settings the plan already named (a seam binding) gain their value.
pub fn govern(nodes: &mut [TreeNode], config: &BTreeMap<String, Sourced<Value>>) {
    for n in nodes.iter_mut() {
        let caps_prefix = format!("caps.{}.", n.id);
        let skill_prefix = n.id.strip_prefix("skill.").map(|name| format!("skills.{name}."));
        let seam_keys: Vec<String> = n.bound.iter().map(|s| format!("seams.{s}")).collect();
        for (key, s) in config.iter().filter(|(k, _)| {
            k.starts_with(&caps_prefix) || skill_prefix.as_ref().is_some_and(|p| k.starts_with(p)) || seam_keys.contains(k)
        }) {
            match n.settings.iter_mut().find(|x| &x.key == key) {
                Some(existing) => existing.value = Some(s.value.clone()),
                None => n.settings.push(Setting { key: key.clone(), value: Some(s.value.clone()), origin: s.winner.render() }),
            }
        }
        if let Some(bound) = n.settings.iter_mut().find(|x| x.key == "seams.world" && x.value.is_none()) {
            bound.value = config.get("seams.world").map(|s| s.value.clone());
        }
        govern(&mut n.children, config);
    }
}

const ID_W: usize = 26;
const STATE_W: usize = 10;
const REQ_W: usize = 32;

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { format!("{s} ") } else { format!("{s}{}", " ".repeat(w - n)) }
}

fn detail_lines(n: &TreeNode) -> Vec<String> {
    let settings = || n.settings.iter().map(|s| format!("[{}]", s.render())).collect::<Vec<_>>();
    match &n.state {
        CapState::Active => {
            let mut parts = Vec::new();
            if !n.bound.is_empty() {
                parts.push(format!("bound: {}", n.bound.join(", ")));
            }
            if n.classes.0 != 0 {
                parts.push(render_classes(n.classes));
            }
            if !n.enforced_by.is_empty() {
                parts.push(format!("enforced by: {}", n.enforced_by));
            }
            parts.extend(settings());
            vec![parts.join("   ")]
        }
        CapState::Unlocked => {
            let fix = n.action.as_ref().map(|a| format!("   fix: `{a}`")).unwrap_or_default();
            vec![format!("{}{fix}", n.because)]
        }
        CapState::Locked { .. } => n
            .missing
            .iter()
            .map(|m| match &m.fix {
                Some(fix) => format!("{}fix: `{fix}`", pad(&m.requirement, REQ_W)),
                None => m.requirement.clone(),
            })
            .collect(),
        CapState::Broken { err } => vec![format!("{err}   (origin: {})", n.origin)],
        CapState::Disabled { .. } => vec![settings().join("   ")],
    }
}

fn render_node(out: &mut String, n: &TreeNode, child: bool) {
    let (lead, id_w) = if child { (format!("  {} ", glyph(&n.state)), ID_W - 2) } else { (format!("{} ", glyph(&n.state)), ID_W) };
    let head = format!("{lead}{}{}", pad(&n.id, id_w), pad(state_name(&n.state), STATE_W));
    let indent = " ".repeat(head.chars().count());
    let mut lines = detail_lines(n);
    if let (Some(m), CapState::Active | CapState::Locked { .. }) = (&n.over_preset, &n.state) {
        lines.push(match &m.fix {
            Some(fix) => format!("⚠ {}fix: `{fix}`", pad(&m.requirement, REQ_W.saturating_sub(2))),
            None => format!("⚠ {}", m.requirement),
        });
    }
    let mut lines = lines.iter();
    let first = lines.next().map(String::as_str).unwrap_or("");
    out.push_str(format!("{head}{first}").trim_end());
    out.push('\n');
    for l in lines {
        out.push_str(format!("{indent}{l}").trim_end());
        out.push('\n');
    }
    for c in &n.children {
        render_node(out, c, true);
    }
}

/// The `aho tree` screen: header, then one line per node, one line per missing prerequisite.
pub fn render_tree(composition: &str, nodes: &[TreeNode], plan: &Plan) -> String {
    let fp = plan.fingerprint.to_hex();
    let mut out = format!(
        "{composition} @ plan {}  (preset: {}, world: {}, fingerprint {}…)\n\n",
        plan.hash.short(),
        plan.preset,
        plan.world.name,
        &fp[..4]
    );
    for n in nodes {
        render_node(&mut out, n, false);
    }
    out
}

fn flatten(nodes: &[TreeNode]) -> Vec<&TreeNode> {
    let mut out = Vec::new();
    for n in nodes {
        out.push(n);
        out.extend(flatten(&n.children));
    }
    out
}

/// One uniform card per capability, whatever its kind.
#[derive(Clone, Debug, Serialize)]
pub struct Card {
    pub id: String,
    pub kind: String,
    pub origin: String,
    pub source: Option<String>,
    pub state: CapState,
    pub classes: ConsequenceSet,
    pub settings: Vec<Setting>,
    pub equip: Option<String>,
    pub fix: Option<String>,
    pub doc: String,
}

/// `kind` filters by label (`tool`, `skill`, `mcp`, `provider`, …); `None` lists everything.
pub fn inventory(plan: &Plan, kind: Option<&str>) -> Vec<Card> {
    inventory_in(&tree(plan), plan, kind)
}

/// [`inventory`] over nodes already built (e.g. [`govern`]ed with resolved config).
pub fn inventory_in(nodes: &[TreeNode], plan: &Plan, kind: Option<&str>) -> Vec<Card> {
    flatten(nodes)
        .into_iter()
        .filter(|n| kind.is_none_or(|k| n.kind == k))
        .map(|n| {
            let over = ConsequenceSet(n.classes.0 & !plan.allowed.0);
            let (equip, fix) = match &n.state {
                CapState::Active => (Some("(already active)".to_string()), n.over_preset.as_ref().and_then(|m| m.fix.clone())),
                CapState::Unlocked | CapState::Disabled { .. } => {
                    let mut e = n.action.clone().unwrap_or_default();
                    if over.0 != 0 {
                        e.push_str(&format!("       (will unlock: {}; exceeds preset '{}')", render_classes(over), plan.preset));
                    } else if n.classes.0 != 0 {
                        e.push_str(&format!("       (will unlock: {})", render_classes(n.classes)));
                    }
                    (Some(e), None)
                }
                CapState::Locked { .. } => (None, first_fix(&n.missing)),
                CapState::Broken { err } => (None, Some(err.clone())),
            };
            Card {
                id: n.id.clone(),
                kind: n.kind.clone(),
                origin: n.origin.clone(),
                source: n.source.clone(),
                state: n.state.clone(),
                classes: n.classes,
                settings: n.settings.clone(),
                equip,
                fix,
                doc: n.doc.clone(),
            }
        })
        .collect()
}

pub fn render_inventory(cards: &[Card]) -> String {
    let mut out = String::new();
    for c in cards {
        let origin = match &c.source {
            Some(s) => format!("{} ({s})", c.origin),
            None => c.origin.clone(),
        };
        out.push_str(&format!("{}origin: {}   state: {}\n", pad(&c.id, 23), origin, state_name(&c.state)));
        if c.classes.0 != 0 {
            out.push_str(&format!("  consequences: {}\n", render_classes(c.classes)));
        }
        for (i, s) in c.settings.iter().enumerate() {
            let label = if i == 0 { "settings:" } else { "" };
            out.push_str(&format!("  {}{}\n", pad(label, 14), s.render()));
        }
        if let Some(e) = &c.equip {
            out.push_str(&format!("  equip:        {e}\n"));
        }
        if let Some(f) = &c.fix {
            out.push_str(&format!("  fix:          {f}\n"));
        }
    }
    out
}

/// The result `aho_why <capability>` returns and `aho explain <capability>` prints.
#[derive(Clone, Debug, Serialize)]
pub struct Why {
    pub id: String,
    pub kind: String,
    pub state: CapState,
    pub because: String,
    pub classes: ConsequenceSet,
    pub ceiling: ConsequenceSet,
    pub bound: Vec<String>,
    pub enforced_by: String,
    pub missing: Vec<Missing>,
    pub governed_by: Vec<Setting>,
    /// The exact command that would change the state; `None` when Active.
    pub fix: Option<String>,
    pub preset: String,
    pub allowed: ConsequenceSet,
    pub world: String,
}

pub fn why(plan: &Plan, id: &str) -> Option<Why> {
    why_in(&tree(plan), plan, id)
}

/// [`why`] over nodes already built; `governed_by` then carries resolved values.
pub fn why_in(nodes: &[TreeNode], plan: &Plan, id: &str) -> Option<Why> {
    let n = flatten(nodes).into_iter().find(|n| n.id == id)?;
    Some(Why {
        id: n.id.clone(),
        kind: n.kind.clone(),
        state: n.state.clone(),
        because: n.because.clone(),
        classes: n.classes,
        ceiling: n.ceiling,
        bound: n.bound.clone(),
        enforced_by: n.enforced_by.clone(),
        missing: n.missing.clone(),
        governed_by: n.settings.clone(),
        fix: n.action.clone(),
        preset: plan.preset.clone(),
        allowed: plan.allowed,
        world: plan.world.name.clone(),
    })
}
