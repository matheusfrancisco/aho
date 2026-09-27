//! The one verb set. `aho` (clap), the TUI panel, the web panel, and SDK clients all call
//! these; there is no CLI-only or UI-only operation. Every state change is plan → diff → apply:
//! [`propose`] computes the diff against the file the change would land in, [`Proposal::commit`]
//! writes it (toml_edit, comments kept).

use std::collections::BTreeSet;
use std::path::PathBuf;

use aho_cap::Composition;
use aho_compose::{ComposeError, Inputs, PlanDiff, Planned};
use aho_config::{FileSnapshot, Layer, Schema, Ty, Where};
use aho_inspect::{Explanation, TreeNode};
use aho_log::{CapDeltaOp, CapState, Entry, EntryKind, State};
use aho_session::Session;
use aho_types::*;
use serde::Serialize;
use serde_json::json;

#[derive(Debug)]
pub enum CapsError {
    Compose(ComposeError),
    Unknown(String),
    BadParams(String),
}

impl CapsError {
    /// Plan/verify failures exit 2 in the CLI; everything else is a plain error.
    pub fn is_plan_failure(&self) -> bool {
        matches!(
            self,
            CapsError::Compose(
                ComposeError::Plan(_) | ComposeError::Resolve(aho_config::ResolveError::Strict(_)) | ComposeError::Drift { .. }
            )
        )
    }
}

impl std::fmt::Display for CapsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapsError::Compose(e) => write!(f, "{e}"),
            CapsError::Unknown(s) | CapsError::BadParams(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for CapsError {}

impl From<ComposeError> for CapsError {
    fn from(e: ComposeError) -> Self {
        CapsError::Compose(e)
    }
}

/// The verbs' planning input: the linked composition and the operator's captured inputs.
#[derive(Clone)]
pub struct CapsContext {
    pub composition: Composition,
    pub inputs: Inputs,
}

impl CapsContext {
    pub fn new(composition: Composition, inputs: Inputs) -> CapsContext {
        CapsContext { composition, inputs }
    }

    /// The inputs with the config files re-read, so a write since startup is seen.
    pub fn fresh_inputs(&self) -> Result<Inputs, CapsError> {
        let mut i = self.inputs.clone();
        i.capture_files()?;
        Ok(i)
    }

    pub fn load(&self) -> Result<Planned, CapsError> {
        Ok(aho_compose::load(&self.composition, &self.fresh_inputs()?)?)
    }
}

/// The tree with the resolved values of the settings that govern each node.
pub fn nodes(p: &Planned) -> Vec<TreeNode> {
    let mut n = aho_inspect::tree(&p.plan);
    aho_inspect::govern(&mut n, &p.loadout.config);
    n
}

pub fn find_node<'a>(nodes: &'a [TreeNode], id: &str) -> Option<&'a TreeNode> {
    nodes.iter().find_map(|n| if n.id == id { Some(n) } else { find_node(&n.children, id) })
}

/// Every capability id the plan knows (linked or discovered), for "did you mean" errors.
pub fn capability_ids(nodes: &[TreeNode]) -> Vec<String> {
    let mut out = Vec::new();
    for n in nodes {
        out.push(n.id.clone());
        out.extend(capability_ids(&n.children));
    }
    out
}

pub fn require_capability(p: &Planned, id: &str) -> Result<(), CapsError> {
    if find_node(&nodes(p), id).is_some() {
        Ok(())
    } else {
        Err(CapsError::Unknown(format!(
            "no capability `{id}` is linked or discovered. Nothing was changed; `aho inventory` lists every capability."
        )))
    }
}

/// Whether `key` is a setting this build declares (table keys accept any sub-key).
pub fn known_key(schema: &Schema, key: &str) -> bool {
    if schema.keys.contains_key(key) {
        return true;
    }
    let mut prefix = key;
    while let Some((head, _)) = prefix.rsplit_once('.') {
        if schema.keys.get(head).is_some_and(|k| k.ty == Ty::Table) {
            return true;
        }
        prefix = head;
    }
    false
}

pub fn require_key(p: &Planned, key: &str) -> Result<(), CapsError> {
    if known_key(&p.schema, key) {
        Ok(())
    } else {
        Err(CapsError::Unknown(format!(
            "`{key}` is not a setting this build knows. Nothing was written; `aho catalog config` lists every setting."
        )))
    }
}

/// `enable` / `disable` / `allow` as the key they write.
pub fn switch(verb: &str, id: &str) -> Result<(String, Value), CapsError> {
    match verb {
        "enable" | "disable" => {
            let key = match id.strip_prefix("skill.") {
                Some(name) => format!("skills.{name}.enabled"),
                None => format!("caps.{id}.enabled"),
            };
            Ok((key, Value::Bool(verb == "enable")))
        }
        "allow" => Ok((format!("caps.{id}.allow"), Value::Bool(true))),
        other => Err(CapsError::BadParams(format!("`{other}` is not enable, disable, or allow"))),
    }
}

pub fn require_preset(name: &str) -> Result<(), CapsError> {
    aho_policy::preset(name).map(|_| ()).ok_or_else(|| {
        CapsError::Unknown(format!("`{name}` names no preset. Nothing was written; use one of read-only, workspace, dev, lab, full, spec."))
    })
}

/// A proposed layer (`aho plan --patch`, `caps.plan{patch}`): ranks like a flag, origins point at it.
pub fn with_patch(inputs: &Inputs, path: &str, text: String) -> Inputs {
    inputs.clone().with_layer(FileSnapshot { path: path.to_string(), layer: Layer::Arg, text })
}

/// A planned write: the file it lands in, the plans before and after, and their diff.
pub struct Proposal {
    pub changes: Vec<(String, Value)>,
    pub file: PathBuf,
    pub before: Planned,
    pub after: Planned,
    pub diff: PlanDiff,
    /// Keys whose value is a credential or a declared secret: never echoed.
    pub secret: BTreeSet<String>,
}

/// Plans `changes` as if written to the most specific existing layer file (`./aho.toml` if
/// present, else `~/.aho/config.toml`). Nothing is written.
pub fn propose(comp: &Composition, inputs: &Inputs, changes: Vec<(String, Value)>) -> Result<Proposal, CapsError> {
    propose_in(comp, inputs, aho_compose::apply_target(&inputs.project, &inputs.home), changes)
}

/// [`propose`] into an explicit layer file (credentials always go to `~/.aho/config.toml`).
pub fn propose_in(comp: &Composition, inputs: &Inputs, file: PathBuf, changes: Vec<(String, Value)>) -> Result<Proposal, CapsError> {
    let before = aho_compose::load(comp, inputs)?;
    let path = file.display().to_string();
    let layer = if file == inputs.project_config() { Layer::FileProject } else { Layer::FileHome };
    let mut text = inputs.files.iter().find(|f| f.path == path).map(|f| f.text.clone()).unwrap_or_default();
    for (k, v) in &changes {
        text = aho_config::apply_to_file(&text, k, v)
            .map_err(|e| CapsError::Compose(ComposeError::Apply { path: file.clone(), err: format!("{e:?}") }))?;
    }
    let mut after_inputs = inputs.clone();
    after_inputs.files.retain(|f| f.path != path);
    after_inputs.files.push(FileSnapshot { path, layer, text });
    let after = aho_compose::load(comp, &after_inputs)?;
    let diff = aho_compose::plan_diff(&before, &after);
    let secret = changes
        .iter()
        .map(|(k, _)| k)
        .filter(|k| aho_compose::credential_name(k).is_some() || before.schema.keys.get(k.as_str()).is_some_and(|c| c.secret))
        .cloned()
        .collect();
    Ok(Proposal { changes, file, before, after, diff, secret })
}

fn show(v: &Value) -> String {
    match v {
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

impl Proposal {
    fn shown(&self, k: &str, v: &Value) -> String {
        if self.secret.contains(k) { "(secret)".into() } else { show(v) }
    }

    /// The block printed before `apply? [y/N]`: state diff, preset notes, and the write itself.
    pub fn render(&self) -> String {
        let mut out = String::from("plan:\n");
        out.push_str(&aho_compose::render_plan_diff(&self.diff));
        let after_nodes = nodes(&self.after);
        for flip in self.diff.caps.iter().filter(|f| matches!(f.after, Some(CapState::Active))) {
            if let Some(note) = find_node(&after_nodes, &flip.id).and_then(|n| n.over_preset.as_ref()) {
                let fix = note.fix.as_deref().map(|f| format!("  → calls needing them ask or are denied until `{f}`")).unwrap_or_default();
                out.push_str(&format!("  !        {}{fix}\n", note.requirement));
            }
        }
        for (k, v) in &self.changes {
            out.push_str(&format!("writes {}  {k} = {}\n", self.file.display(), self.shown(k, v)));
        }
        out
    }

    /// Writes every change; returns one operator-facing line per key (with its line number, and
    /// a warning when a higher layer still wins).
    pub fn commit(&self) -> Result<Vec<String>, CapsError> {
        for (k, v) in &self.changes {
            aho_compose::apply(&self.file, k, v.clone())?;
        }
        let path = self.file.display().to_string();
        Ok(self
            .changes
            .iter()
            .map(|(k, v)| {
                let sourced = self.after.loadout.config.get(k);
                let line = |o: &aho_config::Origin| match &o.at {
                    Where::File { path: p, line } if *p == path => Some(*line),
                    _ => None,
                };
                let at = sourced.and_then(|s| line(&s.winner).or_else(|| s.shadowed.iter().find_map(|(o, _)| line(o))));
                let mut s = match at {
                    Some(n) => format!("wrote {path}:{n}  {k} = {}", self.shown(k, v)),
                    None => format!("wrote {path}  {k} = {}", self.shown(k, v)),
                };
                if let Some(src) = sourced.filter(|s| line(&s.winner).is_none()) {
                    s.push_str(&format!("   ⚠ still shadowed by {} = {}", src.winner.render(), self.shown(k, &src.value)));
                }
                s
            })
            .collect())
    }

    /// Persists an operator mutation and its audit trail. The caller must hold the session's
    /// mutation reservation until it has replaced the booted plan (daemon: reopen).
    pub fn commit_in_session(&self, session: &Session, method: &str) -> Result<Vec<String>, CapsError> {
        self.commit_recorded(session, method, false)
    }

    /// The owner accepted the preset named by a currently pending approval card. Unlike a
    /// general apply, it may run while that turn waits; only the preset is recorded, not a
    /// replacement tool plan. The approval broker must validate the card before calling.
    pub fn commit_approval_preset(&self, session: &Session) -> Result<Vec<String>, CapsError> {
        if self.changes.len() != 1 || self.changes[0].0 != "policy.preset" {
            return Err(CapsError::BadParams("an approval preset changes only policy.preset".into()));
        }
        self.commit_recorded(session, "approval.preset", true)
    }

    fn commit_recorded(&self, session: &Session, method: &str, approval: bool) -> Result<Vec<String>, CapsError> {
        let state = session.fold().map_err(|e| CapsError::BadParams(format!("cannot audit this mutation: {e}")))?;
        if state.turn.is_some() && !approval {
            return Err(CapsError::BadParams("a turn is running; apply the change once the session is idle".into()));
        }
        let append = |kind| {
            session
                .append_entry(Principal::Operator, kind)
                .map_err(|e| CapsError::BadParams(format!("could not record the operator mutation: {e}")))
        };
        // Secret values never enter the command arguments (nor the resolved config plaintext).
        let args = self.changes.iter().map(|(k, v)| format!("{k}={}", self.shown(k, v))).collect();
        let run = append(EntryKind::CommandRun { name: method.to_string(), args, key: None })?.id;
        let wrote = match self.commit() {
            Ok(wrote) => wrote,
            Err(error) => {
                append(EntryKind::CommandDone { run, kind: "error".into() })?;
                return Err(error);
            }
        };
        for kind in plan_entries(&state, &self.after, run, method) {
            if !approval
                || matches!(&kind, EntryKind::PolicyEpoch { .. })
                || matches!(&kind, EntryKind::ConfigResolved { key, .. } if key == "policy.preset")
            {
                append(kind)?;
            }
        }
        append(EntryKind::CommandDone { run, kind: "ok".into() })?;
        Ok(wrote)
    }

    pub fn to_json(&self, applied: Option<&[String]>) -> Value {
        json!({
            "file": self.file,
            "changes": self.changes.iter().map(|(k, v)| json!([k, if self.secret.contains(k) { json!("(secret)") } else { v.clone() }])).collect::<Vec<_>>(),
            "diff": self.diff,
            "rendered": self.render(),
            "plan_hash": self.after.plan.hash,
            "applied": applied.is_some(),
            "wrote": applied,
        })
    }
}

/// Entries that make a session's fold agree with a resolved plan, attributed to the operator
/// command `run`. Shared by CLI mutation, daemon boot and every RPC capability apply.
pub fn plan_entries(st: &State, p: &Planned, run: EntryId, why: &str) -> Vec<EntryKind> {
    let mut out = Vec::new();
    for (key, sourced) in &p.loadout.config {
        if st.config.get(key).map(|s| &s.value) != Some(&sourced.value) {
            out.push(EntryKind::ConfigResolved { key: key.clone(), sourced: sourced.clone() });
        }
    }
    let tree = aho_inspect::tree(&p.plan);
    let mut pending: Vec<_> = tree.iter().rev().collect();
    while let Some(row) = pending.pop() {
        pending.extend(row.children.iter().rev());
        let id = row.id.clone();
        let was_active = matches!(st.caps.get(&id), Some(CapState::Active));
        let op = match &row.state {
            CapState::Active if !was_active => Some(CapDeltaOp::Activated),
            CapState::Broken { err } if !matches!(st.caps.get(&id), Some(CapState::Broken { .. })) => {
                Some(CapDeltaOp::Broken { err: err.clone() })
            }
            CapState::Active | CapState::Broken { .. } => None,
            _ if was_active => Some(CapDeltaOp::Deactivated),
            _ => None,
        };
        if let Some(op) = op {
            out.push(EntryKind::CapDelta { id, op, reason: format!("{why} ({})", aho_inspect::state_name(&row.state)) });
        }
    }
    if p.plan.allowed != st.policy.allowed || p.plan.preset != st.policy.preset {
        out.push(EntryKind::PolicyEpoch {
            epoch: Epoch(st.policy.epoch.0 + 1),
            allowed: p.plan.allowed,
            preset: p.plan.preset.clone(),
            approval_ref: Some(run),
        });
    }
    if st.plan_hash != Some(p.plan.hash) {
        out.push(EntryKind::PlanSealed { plan_hash: p.plan.hash, fingerprint: p.plan.fingerprint, schema_hash: p.schema.hash() });
    }
    out
}

// ───────────────────────────── explain --at ─────────────────────────────

/// `aho explain <key|cap> --at N`: what the agent saw at entry `N` of a session branch.
#[derive(Clone, Debug, Serialize)]
pub struct AtEntry {
    pub at: EntryId,
    pub subject: String,
    /// The capability's state in the fold up to `at` (`None` for a config key or an unknown id).
    pub cap_state: Option<CapState>,
    /// Tools the last model request at or before `at` could see.
    pub tools_visible: Option<Vec<String>>,
    pub policy: Option<aho_log::PolicyState>,
    /// The key explained over the entries up to `at` (`None` for a capability).
    pub explanation: Option<Explanation>,
}

pub fn explain_at(entries: &[Entry], subject: &str, at: EntryId) -> Result<AtEntry, CapsError> {
    let upto: Vec<Entry> = entries.iter().filter(|e| e.id <= at).cloned().collect();
    if upto.is_empty() {
        return Err(CapsError::BadParams(format!("the session has no entries up to {}", at.0)));
    }
    let state = aho_log::fold(&upto, &Default::default())
        .map_err(|e| CapsError::BadParams(format!("the log does not fold up to {}: {e}", at.0)))?;
    let tools_visible = upto.iter().rev().find_map(|e| match &e.kind {
        EntryKind::RequestHeader { tools_visible, .. } => Some(tools_visible.clone()),
        _ => None,
    });
    let cap_state = state.caps.get(subject).cloned();
    let explanation = if cap_state.is_none() { Some(aho_inspect::explain(&upto[..], subject)) } else { None };
    Ok(AtEntry { at, subject: subject.to_string(), cap_state, tools_visible, policy: Some(state.policy), explanation })
}

pub fn render_at(a: &AtEntry) -> String {
    let mut out = format!("at entry {}: {}\n", a.at.0, a.subject);
    if let Some(s) = &a.cap_state {
        out.push_str(&format!("  state    {} {}\n", aho_inspect::glyph(s), aho_inspect::state_name(s)));
    }
    if let Some(p) = &a.policy {
        out.push_str(&format!(
            "  policy   epoch {} · preset {} · allowed {}\n",
            p.epoch.0,
            p.preset,
            aho_inspect::render_classes(p.allowed)
        ));
    }
    if let Some(t) = &a.tools_visible {
        out.push_str(&format!("  tools    {}\n", if t.is_empty() { "(none)".to_string() } else { t.join(" ") }));
    }
    if let Some(e) = &a.explanation {
        out.push_str(&aho_inspect::render_explain(e));
    }
    out
}

// ───────────────────────────── rpc dispatch ─────────────────────────────

fn param<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}

fn need<'a>(p: &'a Value, k: &str) -> Result<&'a str, CapsError> {
    param(p, k).ok_or_else(|| CapsError::BadParams(format!("missing string param `{k}`")))
}

/// Whether this RPC requires exclusive turn admission and a replacement boot.
pub fn is_apply(method: &str, params: &Value) -> bool {
    matches!(method, "caps.set" | "caps.enable" | "caps.disable" | "caps.allow" | "caps.preset")
        && params.get("apply").and_then(Value::as_bool) == Some(true)
}

/// Read-only `caps.*` calls. Applying without a session would omit its audit log and is refused.
pub fn call(ctx: &CapsContext, method: &str, params: &Value, entries: &[Entry]) -> Result<Value, CapsError> {
    dispatch(ctx, method, params, entries, None)
}

/// Serves `caps.*` against a live session, including operator audit records on apply.
/// The host must hold `Session::try_mutation()` for applies through replacement boot.
pub fn call_in_session(ctx: &CapsContext, session: &Session, method: &str, params: &Value) -> Result<Value, CapsError> {
    let entries = match session.log().head() {
        Some(head) if params.get("at").is_some() => {
            session.log().read_branch(head).map_err(|e| CapsError::BadParams(format!("could not read the session branch: {e}")))?
        }
        _ => Vec::new(),
    };
    dispatch(ctx, method, params, &entries, Some(session))
}

fn dispatch(ctx: &CapsContext, method: &str, params: &Value, entries: &[Entry], session: Option<&Session>) -> Result<Value, CapsError> {
    let apply = is_apply(method, params);
    match method {
        "caps.list" => {
            let p = ctx.load()?;
            js(&aho_inspect::inventory_in(&nodes(&p), &p.plan, param(params, "kind")))
        }
        "caps.tree" => {
            let p = ctx.load()?;
            let all = nodes(&p);
            let picked: Vec<&TreeNode> = match param(params, "id") {
                Some(id) => vec![find_node(&all, id).ok_or_else(|| CapsError::Unknown(format!("no capability `{id}`")))?],
                None => all.iter().collect(),
            };
            Ok(json!({
                "composition": ctx.composition.name,
                "plan": p.plan.hash,
                "preset": p.plan.preset,
                "world": p.plan.world,
                "fingerprint": p.plan.fingerprint,
                "nodes": picked,
            }))
        }
        "caps.why" => {
            let p = ctx.load()?;
            let id = need(params, "id")?;
            js(&aho_inspect::why_in(&nodes(&p), &p.plan, id).ok_or_else(|| CapsError::Unknown(format!("no capability `{id}`")))?)
        }
        "caps.explain" => {
            let subject = need(params, "key")?;
            if let Some(at) = params.get("at").and_then(Value::as_u64) {
                return js(&explain_at(entries, subject, EntryId(at))?);
            }
            let p = ctx.load()?;
            match aho_inspect::why_in(&nodes(&p), &p.plan, subject) {
                Some(w) => Ok(json!({ "capability": w })),
                None => Ok(json!({ "key": aho_inspect::explain(&p.loadout, subject) })),
            }
        }
        "caps.plan" => {
            let inputs = ctx.fresh_inputs()?;
            let before = aho_compose::load(&ctx.composition, &inputs)?;
            let after = match param(params, "patch") {
                Some(text) => Some(aho_compose::load(
                    &ctx.composition,
                    &with_patch(&inputs, param(params, "patch_path").unwrap_or("rpc:patch"), text.to_string()),
                )?),
                None => None,
            };
            let shown = after.as_ref().unwrap_or(&before);
            let diff = after.as_ref().map(|a| aho_compose::plan_diff(&before, a));
            Ok(json!({
                "plan": shown.plan,
                "config": shown.loadout.config,
                "diagnostics": shown.loadout.diagnostics,
                "loadout_hash": shown.loadout.hash,
                "schema_hash": shown.schema.hash(),
                "rendered_diff": diff.as_ref().map(aho_compose::render_plan_diff),
                "diff": diff,
            }))
        }
        "caps.set" | "caps.enable" | "caps.disable" | "caps.allow" | "caps.preset" => {
            let inputs = ctx.fresh_inputs()?;
            let before = aho_compose::load(&ctx.composition, &inputs)?;
            let change = match method {
                "caps.set" => {
                    let key = need(params, "key")?;
                    require_key(&before, key)?;
                    let value = params.get("value").cloned().ok_or_else(|| CapsError::BadParams("missing param `value`".into()))?;
                    (key.to_string(), value)
                }
                "caps.preset" => {
                    let name = need(params, "name")?;
                    require_preset(name)?;
                    ("policy.preset".to_string(), Value::String(name.to_string()))
                }
                verb => {
                    let id = need(params, "id")?;
                    require_capability(&before, id)?;
                    switch(verb.trim_start_matches("caps."), id)?
                }
            };
            let prop = propose(&ctx.composition, &inputs, vec![change])?;
            if apply {
                let session = session.ok_or_else(|| {
                    CapsError::BadParams(
                        "applying capabilities requires a live session so the change can be audited; no file was written".into(),
                    )
                })?;
                let wrote = prop.commit_in_session(session, method)?;
                Ok(prop.to_json(Some(&wrote)))
            } else {
                Ok(prop.to_json(None))
            }
        }
        other => Err(CapsError::Unknown(format!("unknown method `{other}`"))),
    }
}

fn js<T: Serialize>(v: &T) -> Result<Value, CapsError> {
    serde_json::to_value(v).map_err(|e| CapsError::BadParams(format!("could not encode the reply: {e}")))
}
