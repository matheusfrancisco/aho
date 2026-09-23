//! Operator verbs over plans, logs, and capabilities that do not fit the older groups:
//! `apply` (03), `doctor` (12), `caps show` (15), `schedule` / `ralph` / `plan-mode` (00, 04,
//! 10), and `gc` (04, 11, 12). `invariants` (13) lives in `invariants.rs`.

use std::path::Path;
use std::time::Duration;

use aho_log::{EntryKind, JsonlLog, LogStore};
use aho_types::*;
use serde_json::json;
use surface_rpc::caps;

use crate::ctx::{Ctx, Fail, SessionFile, branch, confirm, err, find_session, hex};
use crate::record::{Writer, record_change};
use crate::session::{open, state_of};

fn print_json(v: &Value) -> Result<(), Fail> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Leaves of a layer file as dotted config keys, each written into its table like `aho set`
/// does (a key under a table-typed setting, `mcp.servers.x.cmd`, is known through its table).
fn flatten(p: &aho_compose::Planned, prefix: &str, v: &toml::Value, out: &mut Vec<(String, Value)>) -> Result<(), Fail> {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(p, &key, v, out)?;
            }
            Ok(())
        }
        _ => {
            caps::require_key(p, prefix)?;
            out.push((prefix.to_string(), serde_json::to_value(v)?));
            Ok(())
        }
    }
}

pub async fn apply(ctx: &Ctx, file: &Path, session: Option<&str>) -> Result<(), Fail> {
    let path = ctx.typed_path(file);
    let text = std::fs::read_to_string(&path).map_err(|e| err(format!("could not read {} ({e}); nothing was written", path.display())))?;
    let layer: toml::Table =
        toml::from_str(&text).map_err(|e| err(format!("{} is not TOML ({}); nothing was written", path.display(), e.message())))?;
    let inputs = ctx.inputs(&[], false)?;
    let before = ctx.load(&inputs)?;
    let mut changes = Vec::new();
    flatten(&before, "", &toml::Value::Table(layer), &mut changes)?;
    if changes.is_empty() {
        return Err(err(format!("{} sets no keys; nothing to apply", path.display())));
    }
    let prop = caps::propose(&ctx.comp, &inputs, changes)?;
    print!("{}", prop.render());
    if !confirm(ctx.global.yes)? {
        println!("nothing written.");
        return Ok(());
    }
    for line in prop.commit()? {
        println!("{line}");
    }
    // 03: the running session sees the change as entries in its log.
    record_change(ctx, &prop.after, session, "apply", vec![path.display().to_string()]).await
}

// ───────────────────────────── doctor ─────────────────────────────

const DOCTOR_NS: &str = "Doctor";
const STALE_AFTER_MS: u64 = 7 * 24 * 3600 * 1000;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Offline,
    Catalog,
    Full,
}

struct Ledger {
    log: JsonlLog,
    rows: Vec<Value>,
}

impl Ledger {
    fn check(&mut self, route: &str, tier: &str, step: &str, status: &str, detail: String) -> Result<(), Fail> {
        let body = json!({"route": route, "tier": tier, "step": step, "status": status, "detail": detail});
        self.log.append(
            None,
            Principal::Operator,
            EntryKind::Custom { ns: DOCTOR_NS.into(), name: "Checkpoint".into(), body: body.clone() },
        )?;
        let mark = match status {
            "pass" => "✓",
            "skip" => "·",
            _ => "✕",
        };
        println!("  {mark} {step:<11} {status:<5} {detail}");
        self.rows.push(body);
        Ok(())
    }
}

/// `aho doctor provider <id>`: ordered checkpoints, logged; lighter tiers record the API
/// checkpoints as skipped, never passed (12).
pub async fn doctor_provider(ctx: &Ctx, id: &str, tier: &str, json_out: bool) -> Result<(), Fail> {
    let t = match tier {
        "offline" => Tier::Offline,
        "catalog" => Tier::Catalog,
        "full" => Tier::Full,
        other => return Err(err(format!("`{other}` is not a tier; use offline, catalog, or full"))),
    };
    let inputs = ctx.inputs(&[("model.provider".into(), id.into())], false)?;
    let dir = inputs.home.join(".aho").join("doctor");
    std::fs::create_dir_all(&dir)?;
    let log = JsonlLog::open(dir.join(format!("{}.jsonl", id.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_', "_"))))?;
    if let Some(prev) = log.head().and_then(|h| log.read_branch(h).ok()).and_then(|es| {
        es.into_iter().rev().find(|e| matches!(&e.kind, EntryKind::Custom { ns, name, .. } if ns == DOCTOR_NS && name == "Done"))
    }) {
        let age = Timestamp::now().0.saturating_sub(prev.at.0);
        if let EntryKind::Custom { body, .. } = &prev.kind {
            println!(
                "last run {} ago: tier {} {}{}",
                human_age(age),
                body.get("tier").and_then(Value::as_str).unwrap_or("?"),
                if body.get("ok") == Some(&Value::Bool(true)) { "passed" } else { "failed" },
                if age > STALE_AFTER_MS { "  (stale: older than 7 days)" } else { "" }
            );
        }
    }
    println!("doctor provider {id} --tier {tier}");
    let mut led = Ledger { log, rows: Vec::new() };
    let mut ok = true;
    let p = match ctx.load(&inputs) {
        Ok(p) => p,
        Err(e) => {
            led.check(id, tier, "config", "fail", e.to_string())?;
            return finish(led, id, tier, false, Usage::default(), json_out);
        }
    };
    let keys: Vec<String> = p.loadout.config.keys().filter(|k| k.starts_with(&format!("provider.{id}."))).cloned().collect();
    led.check(id, tier, "config", "pass", if keys.is_empty() { "built-in route (no provider.* keys set)".into() } else { keys.join(" ") })?;
    let key = format!("provider.{id}.key");
    match p.loadout.config.get(&key).filter(|s| !s.value.is_null()) {
        Some(s) => led.check(id, tier, "credential", "pass", format!("{key} ← {}", s.winner.render()))?,
        None => led.check(id, tier, "credential", "skip", format!("{key} is not set (routes like ollama and scripted need none)"))?,
    }
    let adapters: Vec<&aho_cap::PlanRow> =
        p.plan.rows.iter().filter(|r| p.manifests.iter().any(|m| m.id == r.id && m.provides.iter().any(|s| s.0 == "llm"))).collect();
    let (reg, broken) = ctx.comp.start(&p.plan, &p.loadout);
    let route = reg.service::<aho_llm::LlmRoute>(&format!("llm:{id}"));
    match &route {
        Some(r) => {
            let info = r.0.info();
            led.check(
                id,
                tier,
                "adapter",
                "pass",
                format!("route `{}` over {} (late results: {:?})", info.route, info.protocol.tag(), info.late_results),
            )?;
        }
        None => {
            ok = false;
            let why: Vec<String> = adapters
                .iter()
                .map(|r| {
                    let b = broken.iter().find(|(c, _)| *c == r.id).map(|(_, e)| format!(": {e}")).unwrap_or_default();
                    let fix = r.missing.iter().filter_map(|m| m.fix.clone()).next().map(|f| format!(" fix `{f}`")).unwrap_or_default();
                    format!("{} {}{b}{fix}", r.id.name, aho_inspect::state_name(&r.state))
                })
                .collect();
            led.check(id, tier, "adapter", "fail", format!("no active adapter serves `llm:{id}` ({})", why.join("; ")))?;
        }
    }
    let model = p.loadout.str("model.name").unwrap_or("").to_string();
    let mut spend = Usage::default();
    if t >= Tier::Catalog {
        if model.is_empty() {
            ok = false;
            led.check(id, tier, "model", "fail", "model.name is not set: `aho set model.name <name>`".into())?;
        } else {
            led.check(
                id,
                tier,
                "model",
                "skip",
                format!("`{model}`: the adapter has no model list to check it against; `--tier full` proves it with one request"),
            )?;
        }
    } else {
        led.check(id, tier, "model", "skip", "catalog tier not requested".into())?;
    }
    match (t, &route) {
        (Tier::Full, Some(r)) if !model.is_empty() => {
            let req = aho_llm::Request {
                model: model.clone(),
                system: Vec::new(),
                messages: vec![Message { role: Role::User, content: vec![ContentBlock::text("Reply with the single word: ok")] }],
                tools: Vec::new(),
                max_tokens: 16,
                cache_key: None,
            };
            match tokio::time::timeout(Duration::from_secs(60), r.0.stream(req, Box::new(|_| {}))).await {
                Ok(Ok(resp)) => {
                    spend = resp.usage;
                    let text: String = resp.content.iter().filter_map(|b| b.as_text()).collect();
                    led.check(
                        id,
                        tier,
                        "request",
                        "pass",
                        format!(
                            "answered {:?} ({} in / {} out tokens)",
                            surface_print::clip(&text, 40),
                            spend.input_tokens,
                            spend.output_tokens
                        ),
                    )?;
                }
                Ok(Err(e)) => {
                    ok = false;
                    led.check(id, tier, "request", "fail", e.to_string())?;
                }
                Err(_) => {
                    ok = false;
                    led.check(id, tier, "request", "fail", "no answer within 60 s".into())?;
                }
            }
        }
        (Tier::Full, _) => led.check(id, tier, "request", "skip", "the earlier checkpoints failed, so no request was sent".into())?,
        _ => led.check(id, tier, "request", "skip", "full tier not requested (spends tokens)".into())?,
    }
    finish(led, id, tier, ok, spend, json_out)
}

fn finish(led: Ledger, id: &str, tier: &str, ok: bool, spend: Usage, json_out: bool) -> Result<(), Fail> {
    led.log.append(
        None,
        Principal::Operator,
        EntryKind::Custom { ns: DOCTOR_NS.into(), name: "Done".into(), body: json!({"route": id, "tier": tier, "ok": ok, "spend": spend}) },
    )?;
    if json_out {
        print_json(&json!({"route": id, "tier": tier, "ok": ok, "checkpoints": led.rows, "spend": spend}))?;
    }
    if ok { Ok(()) } else { Err(Fail::Plan(format!("provider {id} is not ready at tier {tier}"))) }
}

fn human_age(ms: u64) -> String {
    match ms / 1000 {
        s if s < 120 => format!("{s} s"),
        s if s < 7200 => format!("{} min", s / 60),
        s if s < 172_800 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86_400),
    }
}

const TLA_URL: &str = "https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar";
const ELAN_URL: &str = "https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh";

/// `aho doctor formal [--install tla|lean]` — the fix the formal providers name (13).
pub fn doctor_formal(ctx: &Ctx, install: Option<&str>) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let Some(which) = install else {
        for id in ["formal-tlc", "formal-lean", "tool-formal"] {
            if let Some(r) = p.plan.row(id) {
                println!("{} {:<12} {}", aho_inspect::glyph(&r.state), id, aho_inspect::state_name(&r.state));
                for m in &r.missing {
                    println!("    {}{}", m.requirement, m.fix.as_deref().map(|f| format!("   fix: `{f}`")).unwrap_or_default());
                }
            }
        }
        println!("install: `aho doctor formal --install tla` (TLC; needs java) · `aho doctor formal --install lean` (elan + lake)");
        return Ok(());
    };
    match which {
        "tla" => {
            let dest = inputs.home.join(".aho").join("tools").join("tla2tools.jar");
            println!("plan:\n  download {TLA_URL}\n  to {}\n  set formal.tla.jar to it in ~/.aho/config.toml", dest.display());
            if which_bin("java").is_none() {
                println!("  !  java is not on PATH; TLC needs it (`brew install openjdk`)");
            }
            if !confirm(ctx.global.yes)? {
                println!("nothing changed.");
                return Ok(());
            }
            if let Some(d) = dest.parent() {
                std::fs::create_dir_all(d)?;
            }
            let st =
                std::process::Command::new("curl").args(["-fsSL", "--retry", "2", "-o"]).arg(&dest).arg(TLA_URL).status().map_err(|e| {
                    err(format!("could not run curl ({e}); download {TLA_URL} yourself and `aho set formal.tla.jar <path>`"))
                })?;
            if !st.success() {
                return Err(err(format!("the download failed ({st}); nothing was configured")));
            }
            let prop = caps::propose_in(
                &ctx.comp,
                &inputs,
                inputs.home_config(),
                vec![("formal.tla.jar".into(), json!(dest.display().to_string()))],
            )?;
            for l in prop.commit()? {
                println!("{l}");
            }
            Ok(())
        }
        "lean" => {
            println!(
                "plan:\n  run the Lean toolchain installer: curl {ELAN_URL} | sh -s -- -y\n  (installs elan, lean and lake under ~/.elan; add ~/.elan/bin to PATH)"
            );
            if !confirm(ctx.global.yes)? {
                println!("nothing changed.");
                return Ok(());
            }
            let st = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("curl -fsSL {ELAN_URL} | sh -s -- -y"))
                .status()
                .map_err(|e| err(format!("could not run the installer ({e})")))?;
            if !st.success() {
                return Err(err(format!("the installer failed ({st}); see https://leanprover-community.github.io/get_started.html")));
            }
            println!("installed; then point spec mode at a Lake project: `aho set formal.lean.project <dir>`");
            Ok(())
        }
        other => Err(err(format!("`{other}` is not a checker aho installs; use tla or lean"))),
    }
}

/// `aho doctor embed [--install]` (12): the selected embedder, and for MiniLM whether its pinned
/// artifacts and ONNX Runtime check out. `--install` is the only embedding path that downloads.
pub async fn doctor_embed(ctx: &Ctx, install: bool) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let cfg = <embed_local::EmbedLocalConfig as aho_cap::Config>::from_loadout(&p.loadout).map_err(|e| err(e.to_string()))?;
    if let Some(r) = p.plan.row("embed-local") {
        println!("{} embed-local   {}   model {}", aho_inspect::glyph(&r.state), aho_inspect::state_name(&r.state), cfg.model);
    }
    minilm_doctor(ctx, &inputs, &cfg, install).await
}

#[cfg(not(feature = "minilm"))]
async fn minilm_doctor(_: &Ctx, _: &aho_compose::Inputs, _: &embed_local::EmbedLocalConfig, install: bool) -> Result<(), Fail> {
    if install {
        return Err(err(
            "this aho was built without MiniLM, so nothing was downloaded; rebuild with `cargo build -p aho --features minilm`",
        ));
    }
    println!("  this build embeds with hash-ngram-384 only; semantic MiniLM needs `cargo build -p aho --features minilm`");
    Ok(())
}

#[cfg(feature = "minilm")]
async fn minilm_doctor(ctx: &Ctx, inputs: &aho_compose::Inputs, cfg: &embed_local::EmbedLocalConfig, install: bool) -> Result<(), Fail> {
    use embed_local::{MINILM_MODEL, MINILM_REVISION, MiniLmArtifacts, check_onnx_runtime, provision_minilm};
    let dir = if cfg.model_dir.is_empty() {
        inputs.home.join(".aho").join("models").join(MINILM_MODEL)
    } else {
        std::path::PathBuf::from(&cfg.model_dir)
    };
    if install {
        println!(
            "plan:\n  download {MINILM_MODEL} (revision {MINILM_REVISION}) model.onnx + tokenizer.json, verified by pinned digests\n  into {}",
            dir.display()
        );
        if cfg.model_dir.is_empty() {
            println!("  set embed.local.model_dir to it in ~/.aho/config.toml");
        }
        if !confirm(ctx.global.yes)? {
            println!("nothing changed.");
            return Ok(());
        }
        let artifacts = provision_minilm(&dir).await.map_err(|e| err(format!("{e}; nothing was configured")))?;
        println!("verified {}", artifacts.model());
        if cfg.model_dir.is_empty() {
            let prop = caps::propose_in(
                &ctx.comp,
                inputs,
                inputs.home_config(),
                vec![("embed.local.model_dir".into(), json!(dir.display().to_string()))],
            )?;
            for l in prop.commit()? {
                println!("{l}");
            }
        }
    } else {
        match MiniLmArtifacts::open(&dir) {
            Ok(a) => println!("  artifacts   {} verified ({})", dir.display(), a.model()),
            Err(e) => println!("  artifacts   {}: {e}   fix: `aho doctor embed --install`", dir.display()),
        }
    }
    if cfg.runtime_library.is_empty() {
        println!("  runtime     unset   fix: `aho set embed.local.runtime_library <libonnxruntime 1.23 or newer>`");
    } else {
        match check_onnx_runtime(std::path::Path::new(&cfg.runtime_library)) {
            Ok((major, minor)) => println!("  runtime     {} (ONNX Runtime {major}.{minor})", cfg.runtime_library),
            Err(e) => println!("  runtime     {e}"),
        }
    }
    if cfg.model != MINILM_MODEL {
        println!("  select it:  `aho set embed.local.model {MINILM_MODEL}`");
    }
    Ok(())
}

fn which_bin(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(name)).find(|f| f.is_file()))
}

// ───────────────────────────── caps show ─────────────────────────────

pub fn caps_show(ctx: &Ctx, id: &str, json_out: bool) -> Result<(), Fail> {
    let manifests = ctx.comp.manifests();
    let Some(m) = manifests.iter().find(|m| m.id.name == id) else {
        let near: Vec<&str> = manifests.iter().map(|m| m.id.name).filter(|n| n.contains(id) || id.contains(n)).take(5).collect();
        return Err(err(format!(
            "this binary links no capability `{id}`{}; `aho inventory` lists them",
            if near.is_empty() { String::new() } else { format!(" (did you mean {}?)", near.join(", ")) }
        )));
    };
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let row = p.plan.row(id);
    let host: Vec<String> = m
        .handles
        .iter()
        .chain(m.provides_handles.iter())
        .map(|h| format!("{h:?}"))
        .filter(|h| h.starts_with("HostFs") || h.contains("Host"))
        .collect();
    let late = match id {
        "llm-anthropic-messages" => Some(format!("{:?}", llm_anthropic_messages::LATE_RESULTS)),
        "llm-openai-completions" => Some(format!("{:?}", llm_openai_completions::LATE_RESULTS)),
        "llm-scripted" => Some(format!("{:?}", aho_llm::LateResults::SameCallId)),
        _ => None,
    };
    let keys = (m.config_keys)();
    if json_out {
        return print_json(&json!({
            "id": m.id.name,
            "major": m.id.major,
            "kind": format!("{:?}", m.kind),
            "doc": m.doc,
            "provides": m.provides.iter().map(|s| s.0).collect::<Vec<_>>(),
            "exclusive": m.exclusive,
            "provides_handles": m.provides_handles.iter().map(|h| format!("{h:?}")).collect::<Vec<_>>(),
            "holds_handles": m.handles.iter().map(|h| format!("{h:?}")).collect::<Vec<_>>(),
            "host_handles": host,
            "requires": m.requires.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>(),
            "tools": m.tools.iter().map(|t| json!({"name": t.name, "class": t.class.name()})).collect::<Vec<_>>(),
            "config": keys.iter().map(|k| json!({"key": k.path, "type": format!("{:?}", k.ty), "default": k.default, "doc": k.doc})).collect::<Vec<_>>(),
            "entry_kinds": m.entry_kinds,
            "enforced_by": m.enforced_by,
            "invariants": m.invariants.iter().map(|i| i.name).collect::<Vec<_>>(),
            "late_results": late,
            "state": row.map(|r| aho_inspect::state_name(&r.state)),
            "manifest_hash": m.hash(),
        }));
    }
    println!("{} v{}  {:?}{}", m.id.name, m.id.major, m.kind, if m.exclusive { "  (exclusive bundle)" } else { "" });
    println!("  {}", m.doc);
    if let Some(r) = row {
        println!("  state      {} {}", aho_inspect::glyph(&r.state), aho_inspect::state_name(&r.state));
        for miss in &r.missing {
            println!("             {}{}", miss.requirement, miss.fix.as_deref().map(|f| format!("   fix: `{f}`")).unwrap_or_default());
        }
    }
    let list = |v: Vec<String>| if v.is_empty() { "—".to_string() } else { v.join(", ") };
    println!("  provides   {}", list(m.provides.iter().map(|s| s.0.to_string()).collect()));
    println!("  puts       {}", list(m.provides_handles.iter().map(|h| format!("{h:?}")).collect()));
    println!("  holds      {}", list(m.handles.iter().map(|h| format!("{h:?}")).collect()));
    if !host.is_empty() {
        println!("  host       {}   (touches the host directly)", host.join(", "));
    }
    println!("  requires   {}", list(m.requires.iter().map(|r| format!("{r:?}")).collect()));
    if let Some(e) = m.enforced_by {
        println!("  enforced   {e}");
    }
    if let Some(l) = &late {
        println!("  late results  {l}");
    }
    if !m.tools.is_empty() {
        println!("  tools:");
        for t in m.tools {
            println!("    {:<24} {}", t.name, t.class.name());
        }
    }
    if !keys.is_empty() {
        println!("  settings:");
        for k in &keys {
            let d = k.default.as_ref().map(|d| format!(" = {d}")).unwrap_or_default();
            println!("    {}{d}  ({:?})  {}", k.path, k.ty, surface_print::clip(&k.doc, 70));
        }
    }
    if !m.entry_kinds.is_empty() {
        println!("  entries    {}", m.entry_kinds.join(" "));
    }
    if !m.invariants.is_empty() {
        println!("  invariants {}", m.invariants.iter().map(|i| i.name).collect::<Vec<_>>().join(" "));
    }
    println!("  manifest   {}", m.hash().short());
    Ok(())
}

// ───────────────────────────── schedule / ralph / plan mode ─────────────────────────────

fn session_entries(ctx: &Ctx, session: Option<&str>) -> Result<(std::path::PathBuf, SessionFile, Vec<aho_log::Entry>), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let dir = ctx.session_dir(&p);
    let s = find_session(&dir, session)?;
    let (_, entries) = branch(&s.path)?;
    Ok((dir, s, entries))
}

pub fn schedule_list(ctx: &Ctx, session: Option<&str>, json_out: bool) -> Result<(), Fail> {
    let (_, s, entries) = session_entries(ctx, session)?;
    let all = tool_schedule::load(&entries).map_err(|e| err(format!("the schedule entries do not fold ({e})")))?;
    if json_out {
        return print_json(&json!({"session": hex(s.id), "schedules": all.all()}));
    }
    if all.all().is_empty() {
        println!("session {} has no schedules (the agent creates them with `schedule_create`)", hex(s.id));
    }
    for sc in all.all() {
        println!("{}", sc.summary());
    }
    Ok(())
}

/// Appends one operator entry to the session: here, or through the daemon when it holds it
/// (`session.command`); refused while another process or client drives it. Returns how it went
/// (empty, or through the daemon).
async fn append_operator(ctx: &Ctx, dir: &Path, s: &SessionFile, entries: &[aho_log::Entry], kind: EntryKind) -> Result<String, Fail> {
    let st = state_of(entries)?;
    let refused = |why: String| {
        err(format!(
            "{why}, so nothing was appended: two processes never write one log. Stop it, or run this from the client that drives it"
        ))
    };
    let w = Writer::open(ctx, dir, s, &st).await.map_err(refused)?;
    let appended = w.append(kind).await;
    let via = w.finish(false).await.map_err(err)?;
    appended.map_err(|e| err(format!("{e}; nothing was appended")))?;
    Ok(via.map(|v| format!(" {v}")).unwrap_or_default())
}

pub async fn schedule_cancel(ctx: &Ctx, id: &str, session: Option<&str>) -> Result<(), Fail> {
    let (dir, s, entries) = session_entries(ctx, session)?;
    let all = tool_schedule::load(&entries).map_err(|e| err(format!("the schedule entries do not fold ({e})")))?;
    let ev = all.cancel(id, &aho_goals::Actor::Operator, Timestamp::now()).map_err(|e| err(e.to_string()))?;
    let kind = ev.to_entry().map_err(|e| err(e.to_string()))?;
    let via = append_operator(ctx, &dir, &s, &entries, kind).await?;
    println!("cancelled {id} in session {} (by Operator{via}); it will not fire again", hex(s.id));
    Ok(())
}

pub async fn ralph_stop(ctx: &Ctx, id: Option<&str>, reason: Option<&str>, session: Option<&str>) -> Result<(), Fail> {
    let (dir, s, entries) = session_entries(ctx, session)?;
    let all = tool_workflow::load(&entries).map_err(|e| err(format!("the workflow entries do not fold ({e})")))?;
    let ev = all.stop(id, &aho_goals::Actor::Operator, Timestamp::now(), reason.map(str::to_string)).map_err(|e| err(e.to_string()))?;
    let kind = ev.to_entry().map_err(|e| err(e.to_string()))?;
    let via = append_operator(ctx, &dir, &s, &entries, kind).await?;
    println!("stopped the loop in session {} (by Operator{via}); no further iterations are fed", hex(s.id));
    Ok(())
}

/// `/plan` from the CLI: the live session's ceiling narrows — the daemon's copy when it holds the
/// session (`session.call plan-mode.enter`), else a session this process resumes.
pub async fn plan_mode_on(ctx: &Ctx, reason: Option<&str>, session: Option<&str>) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let dir = ctx.session_dir(&p);
    let s = find_session(&dir, session)?;
    if crate::serve::holding(ctx, &dir, s.id).await?.is_some() {
        let (_, entries) = branch(&s.path)?;
        let w = Writer::open(ctx, &dir, &s, &state_of(&entries)?)
            .await
            .map_err(|why| err(format!("{why}, so plan mode was not entered. Use `/plan` in that client")))?;
        let called = w.call("plan-mode.enter", json!({ "reason": reason })).await;
        let via = w.finish(false).await.map_err(err)?;
        let answer = called
            .ok_or_else(|| err("the daemon let go of the session before plan mode could be entered; run this again"))?
            .map_err(err)?;
        let msg = answer["message"].as_str().unwrap_or("Plan mode is on.");
        println!("{msg} (session {}{})", hex(s.id), via.map(|v| format!(", {v}")).unwrap_or_default());
        return Ok(());
    }
    let o = open(ctx, &p, &inputs.home, Some(&hex(s.id)), false, false).await?;
    let msg = crate::slash::plan_mode(&o.session, reason.map(str::to_string)).map_err(err)?;
    println!("{msg} (session {})", hex(o.id));
    Ok(())
}

// ───────────────────────────── gc ─────────────────────────────

pub fn gc(ctx: &Ctx, dry_run: bool) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let shadow = match p.loadout.str("workspace.shadow_dir").filter(|s| !s.is_empty()) {
        Some(d) => ctx.project.join(d),
        None => inputs.home.join(".aho").join("shadow"),
    };
    let mut scan = gc::ScanInputs::new(ctx.project.clone(), ctx.session_dir(&p), shadow, Timestamp::now());
    // VM machines of this project's sessions (their owners record which session dir they belong to).
    let vm = <world_vm::VmSettings as aho_cap::Config>::from_loadout(&p.loadout).map_err(|e| err(format!("{e:?}")))?;
    scan.vm_state = Some(vm.root(&inputs.home).join("state"));
    let report = gc::scan(&scan);
    if let Some(why) = &report.blind {
        return Err(err(format!("{why}. Check session.dir, then run aho gc again.")));
    }
    print!("{}", gc::render_report(&report));
    if dry_run || report.items.is_empty() {
        return Ok(());
    }
    if !confirm(ctx.global.yes)? {
        println!("nothing reclaimed.");
        return Ok(());
    }
    // `reclaim` scans again and removes only what the fresh scan still lists.
    let outcome = gc::reclaim(&report, &gc::ReclaimOpts::default()).map_err(err)?;
    print!("{}", gc::render_outcome(&outcome));
    Ok(())
}
