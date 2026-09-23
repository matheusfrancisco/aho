//! Verbs over the resolved plan (03, 05). Every write is plan → diff → `apply? [y/N]` →
//! toml_edit into the most specific layer file; `enable`/`disable`/`allow`/`set`/`preset` then
//! record the change in the current session (`record::record_change`), as the walkthroughs show.

use std::path::Path;

use aho_compose::{LockFile, Planned, loadouts};
use aho_config::{Loadout, Sourced};
use aho_inspect::{TreeNode, Why};
use aho_types::*;
use serde_json::json;
use surface_rpc::caps::{self, Proposal};

use crate::ctx::{Ctx, Fail, branch, confirm, err, find_session};

fn print_json(v: &Value) -> Result<(), Fail> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn js<T: serde::Serialize>(v: &T) -> Result<Value, Fail> {
    Ok(serde_json::to_value(v)?)
}

fn show(v: &Value) -> String {
    match v {
        Value::Object(m) if m.len() == 1 && m.contains_key("digest") => "(secret)".into(),
        other => other.to_string(),
    }
}

fn config_lines(config: &std::collections::BTreeMap<String, Sourced<Value>>) -> String {
    let w = config.keys().map(|k| k.len()).max().unwrap_or(0).min(34);
    config.iter().map(|(k, s)| format!("  {k:<w$} = {}   ← {}\n", show(&s.value), s.winner.render())).collect()
}

fn diagnostics(l: &Loadout) -> String {
    l.diagnostics.iter().map(|d| format!("  {:?} {} {}: {}\n", d.severity, js_name(&d.lint), d.key, d.detail).to_lowercase()).collect()
}

fn js_name<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        _ => String::new(),
    }
}

// ───────────────────────────── plan / lock ─────────────────────────────

pub fn plan(ctx: &Ctx, patch: Option<&Path>, lock: bool, strict: bool, json: bool) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], strict)?;
    let before = ctx.load(&inputs)?;
    let after = match patch {
        Some(f) => {
            let path = ctx.typed_path(f);
            let text = std::fs::read_to_string(&path).map_err(|e| err(format!("could not read the patch {} ({e})", path.display())))?;
            Some(ctx.load(&caps::with_patch(&inputs, &path.display().to_string(), text))?)
        }
        None => None,
    };
    let shown = after.as_ref().unwrap_or(&before);
    let diff = after.as_ref().map(|a| aho_compose::plan_diff(&before, a));
    if json {
        print_json(&json!({
            "composition": ctx.comp.name,
            "plan": shown.plan,
            "config": shown.loadout.config,
            "disabled": shown.loadout.disabled.iter().map(|(id, o)| json!({"id": id, "origin": o.render()})).collect::<Vec<_>>(),
            "diagnostics": shown.loadout.diagnostics,
            "loadout_hash": shown.loadout.hash,
            "schema_hash": shown.schema.hash(),
            "diff": diff,
        }))?;
    } else {
        print!("{}", aho_inspect::render_tree(ctx.comp.name, &caps::nodes(shown), &shown.plan));
        println!("\nconfig:");
        print!("{}", config_lines(&shown.loadout.config));
        if !shown.loadout.diagnostics.is_empty() {
            println!("\ndiagnostics:");
            print!("{}", diagnostics(&shown.loadout));
        }
        println!(
            "\nplan {} · loadout {} · schema {} · fingerprint {}",
            shown.plan.hash.short(),
            shown.loadout.hash.short(),
            shown.schema.hash().short(),
            shown.plan.fingerprint.short()
        );
        if let (Some(d), Some(f)) = (&diff, patch) {
            println!("\nstate diff with {}:", f.display());
            print!("{}", aho_compose::render_plan_diff(d));
        }
    }
    if lock {
        write_lock(ctx, shown)?;
    }
    Ok(())
}

fn write_lock(ctx: &Ctx, p: &Planned) -> Result<(), Fail> {
    let path = LockFile::of(p).write(&ctx.project)?;
    println!("wrote {}  (plan {})", path.display(), p.plan.hash.short());
    Ok(())
}

pub fn lock(ctx: &Ctx) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    write_lock(ctx, &p)
}

// ───────────────────────────── projections ─────────────────────────────

pub fn tree(ctx: &Ctx, id: Option<&str>, json: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let all = caps::nodes(&p);
    let picked: Vec<TreeNode> = match id {
        Some(id) => vec![caps::find_node(&all, id).cloned().ok_or_else(|| unknown_cap(id, &all))?],
        None => all,
    };
    if json {
        return print_json(&js(&picked)?);
    }
    print!("{}", aho_inspect::render_tree(ctx.comp.name, &picked, &p.plan));
    Ok(())
}

fn unknown_cap(id: &str, nodes: &[TreeNode]) -> Fail {
    let ids = caps::capability_ids(nodes);
    let near: Vec<&String> = ids.iter().filter(|x| x.contains(id) || id.contains(x.as_str())).take(3).collect();
    let hint = if near.is_empty() {
        String::new()
    } else {
        format!(" (did you mean {})", near.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "))
    };
    err(format!("no capability `{id}` is linked or discovered{hint}; `aho inventory` lists every capability"))
}

pub fn inventory(ctx: &Ctx, kind: Option<&str>, json: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let cards = aho_inspect::inventory_in(&caps::nodes(&p), &p.plan, kind);
    if json {
        return print_json(&js(&cards)?);
    }
    if cards.is_empty() {
        println!("no capability of kind `{}`", kind.unwrap_or("any"));
    }
    print!("{}", aho_inspect::render_inventory(&cards));
    Ok(())
}

pub fn render_why(w: &Why) -> String {
    let mut out = format!("{} {}  {}  ({})\n", aho_inspect::glyph(&w.state), w.id, aho_inspect::state_name(&w.state), w.kind);
    let row = |k: &str, v: String| format!("  {k:<12} {v}\n");
    out.push_str(&row("because", w.because.clone()));
    if w.classes.0 != 0 {
        out.push_str(&row("classes", aho_inspect::render_classes(w.classes)));
    }
    if w.ceiling.0 != 0 {
        out.push_str(&row("ceiling", aho_inspect::render_classes(w.ceiling)));
    }
    if !w.bound.is_empty() {
        out.push_str(&row("bound", w.bound.join(", ")));
    }
    if !w.enforced_by.is_empty() {
        out.push_str(&row("enforced by", w.enforced_by.clone()));
    }
    for s in &w.governed_by {
        let v = s.value.as_ref().map(|v| format!(" = {}", show(v))).unwrap_or_default();
        out.push_str(&row("governed by", format!("{}{v} ← {}", s.key, s.origin)));
    }
    for m in &w.missing {
        let fix = m.fix.as_deref().map(|f| format!("   fix: `{f}`")).unwrap_or_default();
        out.push_str(&row("missing", format!("{}{fix}", m.requirement)));
    }
    let over = ConsequenceSet(w.classes.0 & !w.allowed.0);
    if over.0 != 0 {
        out.push_str(&row(
            "preset",
            format!("{} does not allow {} → those calls ask or are denied", w.preset, aho_inspect::render_classes(over)),
        ));
    } else {
        out.push_str(&row("preset", format!("{} (allows {})", w.preset, aho_inspect::render_classes(w.allowed))));
    }
    out.push_str(&row("world", w.world.clone()));
    if let Some(f) = &w.fix {
        out.push_str(&row("change it", format!("`{f}`")));
    }
    out
}

pub fn why(ctx: &Ctx, id: &str, json: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let nodes = caps::nodes(&p);
    let w = aho_inspect::why_in(&nodes, &p.plan, id).ok_or_else(|| unknown_cap(id, &nodes))?;
    if json {
        return print_json(&js(&w)?);
    }
    print!("{}", render_why(&w));
    Ok(())
}

pub fn explain(ctx: &Ctx, subject: &str, at: Option<u64>, session: Option<&str>, json: bool) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    if let Some(n) = at {
        let s = find_session(&ctx.session_dir(&p), session)?;
        let (_, entries) = branch(&s.path)?;
        let a = caps::explain_at(&entries, subject, EntryId(n))?;
        if json {
            return print_json(&js(&a)?);
        }
        print!("{}", caps::render_at(&a));
        return Ok(());
    }
    let nodes = caps::nodes(&p);
    if let Some(w) = aho_inspect::why_in(&nodes, &p.plan, subject) {
        if json {
            return print_json(&js(&w)?);
        }
        print!("{}", render_why(&w));
        return Ok(());
    }
    caps::require_key(&p, subject)?;
    let mut e = aho_inspect::explain(&p.loadout, subject);
    if let Ok(s) = find_session(&ctx.session_dir(&p), session) {
        let (_, entries) = branch(&s.path)?;
        e.history = aho_inspect::explain(&entries[..], subject).history;
    }
    if json {
        return print_json(&js(&e)?);
    }
    if e.value.is_none() {
        println!("{subject} is not set (no default, no layer sets it)");
    }
    print!("{}", aho_inspect::render_explain(&e));
    Ok(())
}

pub fn catalog_config(ctx: &Ctx) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    print!("{}", aho_inspect::catalog_config(&p.schema));
    Ok(())
}

// ───────────────────────────── writes ─────────────────────────────

/// Prints the plan, asks, writes. Secret values (credentials) are never echoed. `false` when
/// the operator declined.
fn write(ctx: &Ctx, prop: &Proposal) -> Result<bool, Fail> {
    print!("{}", prop.render());
    if !confirm(ctx.global.yes)? {
        println!("nothing written.");
        return Ok(false);
    }
    for line in prop.commit()? {
        println!("{line}");
    }
    Ok(true)
}

fn apply(ctx: &Ctx, prop: Proposal) -> Result<(), Fail> {
    write(ctx, &prop).map(|_| ())
}

fn propose(ctx: &Ctx, changes: Vec<(String, Value)>) -> Result<Proposal, Fail> {
    Ok(caps::propose(&ctx.comp, &ctx.inputs(&[], false)?, changes)?)
}

/// [`apply`], then the change as entries in the current session (05: `appended Cap/Delta{…}`).
async fn apply_and_record(ctx: &Ctx, prop: Proposal, name: &str, args: Vec<String>) -> Result<(), Fail> {
    if !write(ctx, &prop)? {
        return Ok(());
    }
    crate::record::record_change(ctx, &prop.after, None, name, args).await
}

pub async fn switch(ctx: &Ctx, verb: &str, id: &str) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let nodes = caps::nodes(&p);
    if caps::find_node(&nodes, id).is_none() {
        return Err(unknown_cap(id, &nodes));
    }
    let change = caps::switch(verb, id)?;
    apply_and_record(ctx, propose(ctx, vec![change])?, verb, vec![id.to_string()]).await
}

pub async fn set(ctx: &Ctx, key: &str, raw: &str) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    caps::require_key(&p, key)?;
    if key == "policy.preset" {
        caps::require_preset(raw)?;
    }
    let prop = propose(ctx, vec![(key.to_string(), aho_config::parse_scalar(raw))])?;
    apply_and_record(ctx, prop, "set", vec![key.to_string(), raw.to_string()]).await
}

pub async fn preset(ctx: &Ctx, name: &str) -> Result<(), Fail> {
    caps::require_preset(name)?;
    let prop = propose(ctx, vec![("policy.preset".into(), Value::String(name.into()))])?;
    apply_and_record(ctx, prop, "preset", vec![name.to_string()]).await
}

pub fn credential_set(ctx: &Ctx, name: &str) -> Result<(), Fail> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(err(format!("`{name}` is not a credential name (letters, digits, `-`, `_`)")));
    }
    let value = read_secret(&format!("value for credential `{name}`: "))?;
    if value.is_empty() {
        return Err(err("no value was entered; nothing was written"));
    }
    let inputs = ctx.inputs(&[], false)?;
    // A model route reads its own `provider.<name>.key`; anything else is a named `cred:<name>`.
    let schema = aho_compose::schema_of(&ctx.comp);
    let route_key = format!("provider.{name}.key");
    let key = if schema.keys.contains_key(&route_key) { route_key } else { format!("credentials.{name}") };
    // Credentials live in the user's home layer, never in a project file that may be committed.
    let file = inputs.home_config();
    apply(ctx, caps::propose_in(&ctx.comp, &inputs, file, vec![(key, Value::String(value))])?)
}

fn read_secret(prompt: &str) -> Result<String, Fail> {
    use std::io::{BufRead, Write};
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut value = String::new();
    if crate::ctx::stdin_tty() && crossterm::terminal::enable_raw_mode().is_ok() {
        loop {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(k)) if k.kind == crossterm::event::KeyEventKind::Press => match k.code {
                    crossterm::event::KeyCode::Enter => break,
                    crossterm::event::KeyCode::Esc => {
                        value.clear();
                        break;
                    }
                    crossterm::event::KeyCode::Backspace => {
                        value.pop();
                    }
                    crossterm::event::KeyCode::Char('c' | 'd') if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                        value.clear();
                        break;
                    }
                    crossterm::event::KeyCode::Char(c) => value.push(c),
                    _ => {}
                },
                Ok(_) => {}
                Err(e) => {
                    crossterm::terminal::disable_raw_mode()?;
                    return Err(err(format!("could not read the terminal ({e})")));
                }
            }
        }
        crossterm::terminal::disable_raw_mode()?;
        println!();
    } else {
        std::io::stdin().lock().read_line(&mut value)?;
        println!();
    }
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

pub fn add_mcp(ctx: &Ctx, name: &str, cmd: &str, env: &[(String, String)], class: Option<&str>) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let base = format!("mcp.servers.{name}");
    caps::require_key(&p, &format!("{base}.cmd"))
        .map_err(|_| err("this build links no MCP provider (`mcp.servers` is not a setting); nothing was written"))?;
    if let Some(c) = class.filter(|c| Consequence::parse(c).is_none()) {
        let all: Vec<&str> = Consequence::ALL.iter().map(|c| c.name()).collect();
        return Err(err(format!("`{c}` is not a consequence class; use one of {}", all.join(", "))));
    }
    let mut changes = vec![(format!("{base}.cmd"), Value::String(cmd.to_string()))];
    for (k, v) in env {
        changes.push((format!("{base}.env.{k}"), Value::String(v.clone())));
    }
    if let Some(c) = class {
        changes.push((format!("{base}.class"), Value::String(c.to_string())));
    }
    apply(ctx, propose(ctx, changes)?)?;
    if class.is_none() {
        println!("without a class the server loads quarantined; classify it with `aho set {base}.class NetworkEgress`");
    }
    Ok(())
}

// ───────────────────────────── loadouts ─────────────────────────────

fn current_saved(p: &Planned) -> loadouts::Saved {
    loadouts::Saved {
        name: "(current)".into(),
        values: p
            .loadout
            .config
            .iter()
            .filter(|(k, _)| !p.loadout.secrets.contains_key(*k))
            .map(|(k, s)| (k.clone(), s.value.clone()))
            .collect(),
        disabled: p.loadout.disabled.iter().map(|(d, _)| d.clone()).collect(),
    }
}

fn render_loadout_diff(rows: &[(String, Option<Value>, Option<Value>)], a: &str, b: &str) -> String {
    if rows.is_empty() {
        return format!("{a} and {b} resolve identically\n");
    }
    let s = |v: &Option<Value>| v.as_ref().map(show).unwrap_or_else(|| "(unset)".into());
    let mut out = format!("{a} -> {b}\n");
    for (k, x, y) in rows {
        out.push_str(&format!("  ~ {k}  {} -> {}\n", s(x), s(y)));
    }
    out
}

pub fn loadout_save(ctx: &Ctx, name: &str) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let path = loadouts::save(&inputs.home, name, &p.loadout)?;
    println!("wrote {}  (loadout {})", path.display(), p.loadout.hash.short());
    Ok(())
}

pub fn loadout_diff(ctx: &Ctx, a: &str, b: Option<&str>) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let left = loadouts::read(&inputs.home, a)?;
    let right = match b {
        Some(b) => loadouts::read(&inputs.home, b)?,
        None => current_saved(&ctx.load(&inputs)?),
    };
    print!("{}", render_loadout_diff(&loadouts::diff(&left, &right), &left.name, &right.name));
    Ok(())
}

pub fn loadout_load(ctx: &Ctx, name: &str) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let before = ctx.load(&inputs)?;
    let after = ctx.load(&inputs.clone().with_layer(loadouts::load(&inputs.home, name)?))?;
    println!("plan with loadout `{name}`:");
    print!("{}", aho_compose::render_plan_diff(&aho_compose::plan_diff(&before, &after)));
    println!("use it with `aho --loadout {name} <verb>` (it ranks like a flag; files are not changed)");
    Ok(())
}

pub fn loadout_list(ctx: &Ctx) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let names = loadouts::list(&inputs.home)?;
    if names.is_empty() {
        println!("no saved loadouts in {}; save one with `aho loadout save <name>`", loadouts::dir(&inputs.home).display());
    }
    for n in names {
        println!("{n}");
    }
    Ok(())
}
