//! The operator's formal verbs: `aho invariants` (the table), `aho invariants show | check |
//! mutate`, and the `--formal` half of `aho session verify`.
//!
//! They drive the checkers as a library, never through a model turn: TLC through formal-tlc on
//! this host, with specs and models resolved exactly as tool-formal's `spec_check` /
//! `invariant_mutate` resolve them (`tool_formal::resolve_target` / `check_config`), and verdicts
//! bound to the workspace tree when the check starts (the quantity `Workspace/Checkpoint`
//! records), so `aho_inspect::current_invariants` judges them like the model's own.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::{Config, ConfigError, ExecOutcome};
use aho_compose::{Inputs, Planned};
use aho_formal::{CheckConfig, CheckMode, Counterexample, Formal, FormalError, FormalKind, Ledger, SpecDoc, SpecRef, Verdict, kind_name};
use aho_fs::Snapshot;
use aho_log::{Entry, EntryKind, State};
use aho_proc::SubprocessHandle;
use aho_types::*;
use formal_tlc::{INSTALL, Tlc, TlcConfig};
use serde_json::json;
use tool_formal::{FormalToolConfig, Target};

use crate::cli::InvariantsVerb;
use crate::ctx::{Ctx, Fail, SessionFile, branch, err, find_session, hex, sessions};
use crate::session::state_of;

pub async fn run(ctx: &Ctx, sub: Option<&InvariantsVerb>, session: Option<&str>, json: bool) -> Result<(), Fail> {
    match sub {
        None => table(ctx, session, json).await,
        Some(InvariantsVerb::Show { id }) => show(ctx, id, session, json).await,
        Some(InvariantsVerb::Check { id, all: _, mode, bound }) => check(ctx, id.as_deref(), mode.as_deref(), *bound, session, json).await,
        Some(InvariantsVerb::Mutate { id }) => mutate(ctx, id, session, json).await,
    }
}

fn print_json(v: &Value) -> Result<(), Fail> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn verdict_text(v: &Verdict) -> String {
    match v {
        Verdict::Pass => "Pass".into(),
        Verdict::Fail { counterexample } => format!("Fail ✕ (counterexample {})", counterexample.short()),
        Verdict::Unknown { reason } => format!("Unknown ({reason})"),
        Verdict::Stale => "Stale".into(),
        Verdict::Vacuous => "Vacuous ⚠".into(),
    }
}

fn mode_text(m: CheckMode) -> String {
    match m {
        CheckMode::Bounded(n) => format!("Bounded({n})"),
        CheckMode::Inductive => "Inductive".into(),
        CheckMode::Proof => "Proof".into(),
        CheckMode::Trace => "Trace".into(),
    }
}

fn cost_text(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// A tool-formal refusal (`ExecOutcome::Err`) as the operator's error.
fn refused(o: ExecOutcome) -> String {
    match o {
        ExecOutcome::Err { reason, .. } => reason,
        ExecOutcome::Done { .. } => "the spec resolved to a result instead of a target".into(),
        ExecOutcome::Cancelled => "the spec lookup was cancelled".into(),
    }
}

// ───────────────────────────── where things are ─────────────────────────────

/// The session a verb reads (and may record into).
struct Sess {
    file: SessionFile,
    entries: Vec<Entry>,
    state: State,
}

/// The resolved configuration, the formal tools' settings, and the session.
struct Here {
    inputs: Inputs,
    p: Planned,
    workspace: PathBuf,
    read: aho_fs::FsRead,
    write: aho_fs::FsWrite,
    tools: FormalToolConfig,
    dir: PathBuf,
    sess: Option<Sess>,
}

/// `required`: fail when there is no session; else `--session` must exist and the default is
/// the most recent one, if any.
fn here(ctx: &Ctx, session: Option<&str>, required: bool) -> Result<Here, Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let dir = ctx.session_dir(&p);
    let file = match (session, required) {
        (Some(_), _) | (None, true) => Some(find_session(&dir, session)?),
        (None, false) => sessions(&dir)?.into_iter().next(),
    };
    let sess = match file {
        Some(file) => {
            let (_, entries) = branch(&file.path)?;
            let state = state_of(&entries)?;
            Some(Sess { file, entries, state })
        }
        None => None,
    };
    let tools = FormalToolConfig::from_loadout(&p.loadout)
        .map_err(|e| err(format!("the formal tools' settings do not resolve ({e}); nothing was checked. Fix it with `aho set`")))?;
    let fs: Arc<dyn aho_fs::Fs> = Arc::new(world_host::HostFs::new(&ctx.workspace(&p)).map_err(err)?);
    Ok(Here { workspace: ctx.workspace(&p), read: aho_fs::FsRead(fs.clone()), write: aho_fs::FsWrite(fs), inputs, p, tools, dir, sess })
}

/// TLC as formal-tlc runs it, on this host, or why it cannot run. The jar is `formal.tla.jar`
/// (what `aho doctor formal --install tla` sets), else `~/.aho/tools/tla2tools.jar`, the path
/// formal-tlc names; the Java runtime must start.
async fn tlc(ctx: &Ctx, inputs: &Inputs, p: &Planned) -> Result<Tlc, String> {
    let cfg = match TlcConfig::from_loadout(&p.loadout) {
        Ok(c) => c,
        Err(ConfigError::MissingKey(_)) => {
            let jar = inputs.home.join(".aho").join("tools").join("tla2tools.jar");
            let with_jar = ctx
                .inputs(&[("formal.tla.jar".into(), jar.display().to_string())], false)
                .and_then(|i| ctx.load(&i))
                .map_err(|e| e.to_string())?;
            TlcConfig::from_loadout(&with_jar.loadout).map_err(|e| format!("formal.tla.* does not resolve ({e})"))?
        }
        Err(e) => return Err(format!("formal.tla.* does not resolve ({e}); fix it with `aho set`")),
    };
    if !cfg.jar.is_file() {
        return Err(format!(
            "TLC's tla2tools.jar is not at {}, so nothing was checked and no verdict was recorded. Install it with `{INSTALL}`, or `aho set formal.tla.jar <path to tla2tools.jar>`",
            cfg.jar.display()
        ));
    }
    let probe = tokio::process::Command::new(&cfg.java).arg("-version").output().await;
    let why = match probe {
        Ok(o) if o.status.success() => None,
        Ok(o) => Some(String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("it exited with an error").trim().to_string()),
        Err(e) => Some(e.to_string()),
    };
    if let Some(why) = why {
        return Err(format!(
            "`{} -version` does not run ({why}), and TLC needs a Java runtime; nothing was checked and no verdict was recorded. Install one (`brew install openjdk`), or `aho set formal.tla.java <path to java>`; then `{INSTALL}` if the jar is missing too",
            cfg.java
        ));
    }
    let proc = SubprocessHandle(Arc::new(world_host::HostSubprocess::new(&ctx.project)));
    let fs: Arc<dyn aho_fs::Fs> = Arc::new(world_host::HostFs::new(&ctx.project).map_err(|e| e.to_string())?);
    Ok(Tlc::new(proc, aho_fs::FsWrite(fs), cfg))
}

/// The code a verdict is bound to: the session world's workspace tree now, as its
/// `Workspace/Checkpoint` entries record it. `Hash::ZERO` (as tool-formal does) when it cannot
/// be read; the verdict then shows Stale at the next checkpoint.
async fn code_tree(ctx: &Ctx, h: &Here, s: &Sess) -> (Hash, Option<String>) {
    let tree = match crate::history::session_shadow(ctx, &h.inputs.home, &s.state) {
        Ok((_, shadow)) => shadow.tree().await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match tree {
        Ok(t) => (t, None),
        Err(e) => (Hash::ZERO, Some(format!("the workspace tree could not be hashed ({e}); verdicts are bound to code 0 and show Stale"))),
    }
}

/// The outcome for the entries a verb wanted to record.
enum Recorded {
    Appended(usize),
    Refused(String),
    NoSession,
}

/// The single place these verbs write a session log, as the Operator: here when nothing drives
/// the session, through the daemon's `session.command` when it holds it. Nothing is appended
/// when another process or client drives it: two processes never drive one log.
async fn append_operator_entry(ctx: &Ctx, h: &Here, kinds: Vec<EntryKind>) -> Result<Recorded, Fail> {
    let Some(s) = &h.sess else { return Ok(Recorded::NoSession) };
    if kinds.is_empty() {
        return Ok(Recorded::Appended(0));
    }
    let w = match crate::record::Writer::open(ctx, &h.dir, &s.file, &s.state).await {
        Ok(w) => w,
        Err(why) => return Ok(Recorded::Refused(why)),
    };
    let mut n = 0;
    let mut failed = None;
    for k in kinds {
        match w.append(k).await {
            Ok(_) => n += 1,
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    w.finish(false).await.map_err(err)?;
    match failed {
        Some(e) => Err(err(format!("{e}; {n} of the entries were appended before that"))),
        None => Ok(Recorded::Appended(n)),
    }
}

fn report(r: &Recorded, sess: Option<&Sess>) {
    match (r, sess) {
        (Recorded::Appended(n), Some(s)) => println!("appended {n} entries by Operator → session {}", hex(s.file.id)),
        (Recorded::Refused(why), _) => {
            println!("not recorded: {why}; the verdicts above are not in its log (run this again once it is free)")
        }
        _ => println!("not recorded: there is no session in this project yet (`aho run` starts one)"),
    }
}

/// A failing check's trace, content-addressed by the hash its `Fail` verdict names.
fn spill_path(dir: &Path, s: &SessionFile, h: Hash) -> PathBuf {
    dir.join(format!("{}.spill", hex(s.id))).join(format!("{}.json", h.to_hex()))
}

fn spill(dir: &Path, s: &SessionFile, cx: &Counterexample) -> Result<(), Fail> {
    let path = spill_path(dir, s, cx.hash());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec(cx)?)
        .map_err(|e| err(format!("could not keep the counterexample at {} ({e}); the verdict was not recorded", path.display())))
}

/// The trace behind `Fail{counterexample: h}`: a formal tool's result in the log, else the
/// session's spill (where `aho invariants check` keeps it).
fn find_counterexample(dir: &Path, s: &Sess, h: Hash) -> Option<Counterexample> {
    let in_log = s.entries.iter().rev().find_map(|e| match &e.kind {
        EntryKind::ToolResult { value, .. } => {
            value.get("counterexample").and_then(|c| serde_json::from_value::<Counterexample>(c.clone()).ok()).filter(|c| c.hash() == h)
        }
        _ => None,
    });
    in_log.or_else(|| {
        let bytes = std::fs::read(spill_path(dir, &s.file, h)).ok()?;
        serde_json::from_slice::<Counterexample>(&bytes).ok().filter(|c| c.hash() == h)
    })
}

// ───────────────────────────── aho invariants ─────────────────────────────

async fn table(ctx: &Ctx, session: Option<&str>, json: bool) -> Result<(), Fail> {
    let h = here(ctx, session, true)?;
    let Some(s) = &h.sess else { return Err(err("no session")) };
    let doc_path = h.workspace.join(&h.tools.spec_doc);
    // No SPEC.md is normal (claims then come only from the log); one that does not parse is an error:
    // the table would silently drop its Draft claims.
    let doc = match doc_path.is_file() {
        true => Some(tool_formal::read_doc(&h.read, &h.tools.spec_doc).await.map_err(|o| {
            err(format!(
                "{} does not parse ({}), so the invariants table was not printed: it would miss its claims",
                doc_path.display(),
                refused(o)
            ))
        })?),
        false => None,
    };
    let rows = aho_inspect::current_invariants(&s.state);
    let code = s.state.checkpoints.last().map(|c| c.1);
    let mut unchecked: Vec<String> = s
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::InvariantRegistered { id, .. } if !s.state.invariants.contains_key(id) => Some(id.clone()),
            _ => None,
        })
        .collect();
    unchecked.dedup();
    if json {
        return print_json(&json!({"session": hex(s.file.id), "code": code, "rows": rows, "never_checked": unchecked}));
    }
    println!(
        "session {} · spec {} · code {}",
        hex(s.file.id),
        if doc.is_some() { doc_path.display().to_string() } else { "(none)".into() },
        code.map(|c| c.short()).unwrap_or_else(|| "no checkpoint yet (verdicts stand as recorded)".into())
    );
    if rows.is_empty() && unchecked.is_empty() {
        println!("no invariants checked or registered in this session yet (`aho invariants check`, or spec mode: `aho spec`)");
        return Ok(());
    }
    print!("{}", aho_inspect::render_invariants(&rows));
    if !unchecked.is_empty() {
        println!("registered, never checked: {}", unchecked.join(", "));
    }
    Ok(())
}

// ───────────────────────────── show ─────────────────────────────

async fn show(ctx: &Ctx, id: &str, session: Option<&str>, json: bool) -> Result<(), Fail> {
    let h = here(ctx, session, false)?;
    let doc: Option<SpecDoc> = tool_formal::read_doc(&h.read, &h.tools.spec_doc).await.ok().map(|(d, _)| d);
    let claim = doc.as_ref().and_then(|d| d.claim(id));
    let ledger = h.sess.as_ref().map(|s| Ledger::from_entries(&s.entries)).unwrap_or_default();
    let rec = ledger.invariants.get(id);
    if rec.is_none() && claim.is_none() {
        return Err(err(format!(
            "`{id}` is neither a claim in {} nor registered or checked in {}; `aho invariants` lists what is",
            h.tools.spec_doc,
            h.sess.as_ref().map(|s| format!("session {}", hex(s.file.id))).unwrap_or_else(|| "any session".into())
        )));
    }
    let kind = rec
        .and_then(|r| r.registrations.last().map(|x| x.1))
        .or_else(|| doc.as_ref().and_then(|d| d.blocks_for(id).find(|b| b.lang != aho_formal::BlockLang::Cfg)).map(|b| b.kind()));
    let statement = rec
        .and_then(|r| r.registrations.last().map(|x| x.2.clone()))
        .or_else(|| claim.map(|c| c.statement().to_string()))
        .unwrap_or_default();
    let covers = rec.and_then(|r| r.registrations.last().map(|x| x.3.clone())).or_else(|| claim.map(|c| c.covers())).unwrap_or_default();
    let checked = rec.and_then(|r| r.checked.clone());
    let stale = checked.as_ref().is_some_and(|(_, v, hash)| matches!(v, Verdict::Stale) || ledger.code.is_some_and(|c| c != *hash));
    let cx = match (&checked, &h.sess) {
        (Some((_, Verdict::Fail { counterexample }, _)), Some(s)) => {
            Some((*counterexample, find_counterexample(&h.dir, s, *counterexample)))
        }
        _ => None,
    };
    let mutants = rec.map(|r| r.mutants.clone()).unwrap_or_default();
    if json {
        return print_json(&json!({
            "id": id,
            "session": h.sess.as_ref().map(|s| hex(s.file.id)),
            "kind": kind.map(kind_name),
            "statement": statement,
            "covers": covers,
            "registrations": rec.map(|r| r.registrations.iter().map(|(e, k, st, cv)| json!({"entry": e.0, "kind": kind_name(*k), "statement": st, "covers": cv})).collect::<Vec<_>>()),
            "claim": claim.map(|c| json!({"line": c.line, "fields": c.fields, "mode": c.mode()})),
            "checked": checked.as_ref().map(|(m, v, hash)| json!({"mode": m, "verdict": v, "code_hash": hash})),
            "current_code": ledger.code,
            "stale": stale,
            "mutants": mutants.iter().map(|(m, c)| json!({"mutant": m, "caught": c})).collect::<Vec<_>>(),
            "counterexample": cx.as_ref().and_then(|(_, c)| c.as_ref()),
        }));
    }
    println!("{id}");
    println!("  statement   {}", if statement.is_empty() { "(none given)" } else { &statement });
    println!("  kind        {}{}", kind.map(kind_name).unwrap_or("?"), claim.map(|c| format!(" · {}", c.class())).unwrap_or_default());
    println!("  covers      {}", if covers.is_empty() { "—".to_string() } else { covers.join(", ") });
    match rec.and_then(|r| r.registrations.last()) {
        Some((e, ..)) => println!(
            "  registered  entry {}{}",
            e.0,
            rec.map(|r| r.registrations.len()).filter(|n| *n > 1).map(|n| format!(" ({n} registrations)")).unwrap_or_default()
        ),
        None => println!(
            "  registered  not in this session's log{}",
            claim.map(|c| format!(" (claim at {}:{})", h.tools.spec_doc, c.line)).unwrap_or_default()
        ),
    }
    if let Some((old, why)) = claim.and_then(|c| c.supersedes()) {
        println!("  supersedes  {old}: {why}");
    }
    match &checked {
        None => println!("  verdict     never checked (`aho invariants check {id}`)"),
        Some((mode, v, hash)) => {
            println!("  verdict     {}{}", verdict_text(v), mode.map(|m| format!(" · {}", mode_text(m))).unwrap_or_default());
            println!(
                "  code        {} checked · {}",
                hash.short(),
                match ledger.code {
                    None => "no checkpoint yet, so the verdict stands".to_string(),
                    Some(c) if stale => format!("Stale: the latest checkpoint is {} (the code changed since the check)", c.short()),
                    Some(c) => format!("current (latest checkpoint {})", c.short()),
                }
            );
        }
    }
    if mutants.is_empty() {
        println!("  mutants     none tried since the latest check (`aho invariants mutate {id}`)");
    } else {
        let caught = mutants.iter().filter(|(_, c)| *c).count();
        println!("  mutants     {caught} of {} caught{}", mutants.len(), if caught == 0 { " (Vacuous ⚠)" } else { "" });
    }
    match cx {
        Some((hash, Some(c))) => {
            println!("\ncounterexample {}:", hash.short());
            for line in c.render().lines() {
                println!("  {line}");
            }
        }
        Some((hash, None)) => println!(
            "\ncounterexample {}: its trace is in neither the log nor the session's spill; `aho invariants check {id}` finds it again",
            hash.short()
        ),
        None => {}
    }
    Ok(())
}

// ───────────────────────────── check ─────────────────────────────

/// One invariant's line in `aho invariants check`.
#[derive(serde::Serialize)]
struct Row {
    id: String,
    kind: Option<&'static str>,
    mode: Option<CheckMode>,
    verdict: Option<Verdict>,
    cost_ms: Option<u64>,
    counterexample: Option<Counterexample>,
    note: String,
}

impl Row {
    fn skipped(id: &str, kind: Option<FormalKind>, note: String) -> Row {
        Row { id: id.into(), kind: kind.map(kind_name), mode: None, verdict: None, cost_ms: None, counterexample: None, note }
    }
}

/// The `args` tool-formal's `spec_check` takes, from the CLI flags.
fn check_args(id: &str, mode: Option<&str>, bound: Option<u32>) -> Value {
    let mut a = json!({"invariant": id});
    if let Some(m) = mode {
        a["mode"] = json!(m);
    }
    if let Some(b) = bound {
        a["bound"] = json!(b);
    }
    a
}

async fn check_one(tlc: &Tlc, h: &Here, id: &str, mode: Option<&str>, bound: Option<u32>) -> Result<Row, Fail> {
    let args = check_args(id, mode, bound);
    let t: Target =
        match tool_formal::resolve_target(&h.read, &h.write, &h.tools.spec_doc, "invariants check", &args, "invariant", None).await {
            Ok(t) => t,
            Err(o) => return Ok(Row::skipped(id, None, refused(o))),
        };
    if t.spec.kind == FormalKind::Lean {
        return Ok(Row::skipped(
            id,
            Some(FormalKind::Lean),
            "a Lean proof: built by `lean_build` in a session (formal-lean); this verb runs TLC".into(),
        ));
    }
    let cfg: CheckConfig = match tool_formal::check_config(&h.read, "invariants check", &args, &t, h.tools.budget_ms).await {
        Ok(c) => c,
        Err(o) => return Ok(Row::skipped(id, Some(FormalKind::Tla), refused(o))),
    };
    let row = |mode, verdict: Verdict, cost_ms, counterexample: Option<Counterexample>, note: String| Row {
        id: id.into(),
        kind: Some("tla"),
        mode: Some(mode),
        verdict: Some(verdict),
        cost_ms,
        counterexample,
        note,
    };
    Ok(match tlc.check(&t.spec, &cfg).await {
        Ok(o) => {
            let note = match (&o.verdict, &o.counterexample) {
                (Verdict::Fail { .. }, Some(c)) => format!("violated: {}", c.violated),
                (Verdict::Unknown { .. }, _) => o.output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string(),
                _ => String::new(),
            };
            row(o.mode, o.verdict, Some(o.cost_ms), o.counterexample, note)
        }
        Err(e @ FormalError::ToolMissing { .. }) => return Err(err(format!("{e} No verdict was recorded."))),
        Err(e) => match e.verdict() {
            Some(v) => row(cfg.mode, v, None, None, e.to_string()),
            None => Row::skipped(id, Some(FormalKind::Tla), format!("not checked: {e}")),
        },
    })
}

async fn check(ctx: &Ctx, id: Option<&str>, mode: Option<&str>, bound: Option<u32>, session: Option<&str>, json: bool) -> Result<(), Fail> {
    let h = here(ctx, session, false)?;
    let (doc, doc_path) = tool_formal::read_doc(&h.read, &h.tools.spec_doc).await.map_err(|o| err(refused(o)))?;
    let ids: Vec<String> = match id {
        Some(i) => vec![i.to_string()],
        None => doc.formalized().into_iter().map(str::to_string).collect(),
    };
    if ids.is_empty() {
        return Err(err(format!(
            "{} has no formal blocks yet, so nothing was checked. Add a ```tla id=<invariant> block under §6 (spec mode: `aho spec`)",
            doc_path.display()
        )));
    }
    let tlc = tlc(ctx, &h.inputs, &h.p).await.map_err(err)?;
    // Bound before the spec is materialized: the scratch extracts are not the code.
    let (code, code_note) = match &h.sess {
        Some(s) => code_tree(ctx, &h, s).await,
        None => (Hash::ZERO, None),
    };
    let mut rows = Vec::new();
    for i in &ids {
        rows.push(check_one(&tlc, &h, i, mode, bound).await?);
    }
    if id.is_some() && rows.iter().all(|r| r.verdict.is_none()) {
        return Err(err(format!("{}; nothing was checked", rows.first().map(|r| r.note.as_str()).unwrap_or(""))));
    }
    let mut kinds = Vec::new();
    for r in &rows {
        let (Some(verdict), Some(mode)) = (&r.verdict, r.mode) else { continue };
        if let (Some(cx), Some(s)) = (&r.counterexample, &h.sess) {
            spill(&h.dir, &s.file, cx)?;
        }
        kinds.push(EntryKind::InvariantChecked { id: r.id.clone(), mode, verdict: verdict.clone(), code_hash: code });
    }
    let failed = rows.iter().filter(|r| matches!(r.verdict, Some(Verdict::Fail { .. }))).count();
    let unknown = rows.iter().filter(|r| matches!(r.verdict, Some(Verdict::Unknown { .. }))).count();
    if !json {
        println!(
            "spec {} · code {}{}",
            doc_path.display(),
            if h.sess.is_some() { code.short() } else { "—".into() },
            code_note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default()
        );
        println!("{:<28}{:<6}{:<14}{:<40}{:<8}note", "id", "kind", "mode", "verdict", "cost");
        for r in &rows {
            let line = format!(
                "{:<28}{:<6}{:<14}{:<40}{:<8}{}",
                r.id,
                r.kind.unwrap_or("?"),
                r.mode.map(mode_text).unwrap_or_else(|| "—".into()),
                r.verdict.as_ref().map(verdict_text).unwrap_or_else(|| "not checked".into()),
                r.cost_ms.map(cost_text).unwrap_or_else(|| "—".into()),
                r.note
            );
            println!("{}", line.trim_end());
        }
        if let Some(s) = h.sess.as_ref().filter(|_| failed > 0) {
            println!("→ `aho invariants show <id> --session {}` renders the counterexample", hex(s.file.id));
        }
        if let Some(last) = h.sess.as_ref().and_then(|s| s.state.checkpoints.last()).filter(|c| c.1 != code) {
            println!(
                "note: the workspace differs from the session's latest checkpoint {}, so `aho invariants` shows these verdicts Stale until the session checkpoints this tree",
                last.1.short()
            );
        }
    }
    let recorded = append_operator_entry(ctx, &h, kinds).await?;
    if json {
        print_json(&json!({"spec": doc_path, "code": code, "results": rows, "recorded": recorded_json(&recorded, h.sess.as_ref())}))?;
    } else {
        report(&recorded, h.sess.as_ref());
    }
    if failed > 0 {
        return Err(Fail::Plan(format!("{failed} invariant(s) failed")));
    }
    if unknown > 0 {
        return Err(err(format!("{unknown} invariant(s) got no verdict (Unknown); the notes say why")));
    }
    Ok(())
}

fn recorded_json(r: &Recorded, sess: Option<&Sess>) -> Value {
    match r {
        Recorded::Appended(n) => json!({"appended": n, "session": sess.map(|s| hex(s.file.id))}),
        Recorded::Refused(why) => json!({"appended": 0, "refused": why}),
        Recorded::NoSession => json!({"appended": 0, "refused": "no session"}),
    }
}

// ───────────────────────────── mutate ─────────────────────────────

async fn mutate(ctx: &Ctx, id: &str, session: Option<&str>, json: bool) -> Result<(), Fail> {
    let h = here(ctx, session, false)?;
    let args = json!({"id": id});
    let tool = "invariants mutate";
    let tlc = tlc(ctx, &h.inputs, &h.p).await.map_err(err)?;
    let (code, code_note) = match &h.sess {
        Some(s) => code_tree(ctx, &h, s).await,
        None => (Hash::ZERO, None),
    };
    let t =
        tool_formal::resolve_target(&h.read, &h.write, &h.tools.spec_doc, tool, &args, "id", None).await.map_err(|o| err(refused(o)))?;
    if t.spec.kind == FormalKind::Lean {
        return Err(err(format!(
            "`{id}` is a Lean proof; its mutants run through `invariant_mutate` in a session (formal-lean), not from here. Nothing was mutated"
        )));
    }
    let cfg = tool_formal::check_config(&h.read, tool, &args, &t, h.tools.budget_ms).await.map_err(|o| err(refused(o)))?;
    let r = tlc.mutate(&t.spec, &cfg).await.map_err(|e| err(format!("{e} Nothing was recorded.")))?;
    let caught = r.caught();
    let mut kinds: Vec<EntryKind> =
        r.mutants.iter().map(|m| EntryKind::InvariantMutated { id: id.to_string(), mutant: m.name.clone(), caught: m.caught }).collect();
    if caught == 0 {
        kinds.push(EntryKind::InvariantChecked { id: id.to_string(), mode: cfg.mode, verdict: Verdict::Vacuous, code_hash: code });
    }
    let summary = if r.mutants.is_empty() {
        format!(
            "No mutants could be made of {} (no guard, comparison or frame in its actions), so `{id}` is Vacuous ⚠ until one exists.",
            t.spec.module
        )
    } else if caught == 0 {
        format!(
            "Vacuous ⚠: `{id}` caught none of {} mutants; it does not constrain what the actions allow, and does not count. Strengthen the statement.",
            r.mutants.len()
        )
    } else {
        format!("`{id}` caught {caught} of {} mutants; it means something.", r.mutants.len())
    };
    if !json {
        println!(
            "mutants of {} ({}, {}){}",
            t.spec.module,
            mode_text(cfg.mode),
            cost_text(r.cost_ms),
            code_note.map(|n| format!(" · {n}")).unwrap_or_default()
        );
        for m in &r.mutants {
            let mark = if m.caught {
                "caught"
            } else if matches!(m.verdict, Verdict::Unknown { .. }) {
                "no verdict"
            } else {
                "not caught"
            };
            let why = match (&m.violated, &m.verdict) {
                (Some(v), _) => format!(" ({v})"),
                (None, Verdict::Unknown { reason }) => format!(" ({reason})"),
                _ => String::new(),
            };
            println!("  {mark:<11}{}: {}{why}", m.name, m.description);
        }
        println!("{summary}");
    }
    let recorded = append_operator_entry(ctx, &h, kinds).await?;
    if json {
        print_json(&json!({
            "id": id, "module": t.spec.module, "mode": cfg.mode, "mutants": r.mutants, "caught": caught,
            "verdict": r.verdict(), "cost_ms": r.cost_ms, "summary": summary, "recorded": recorded_json(&recorded, h.sess.as_ref()),
        }))?;
    } else {
        report(&recorded, h.sess.as_ref());
    }
    if caught == 0 { Err(Fail::Plan(format!("`{id}` is Vacuous: no mutant was caught"))) } else { Ok(()) }
}

// ───────────────────────────── session verify --formal ─────────────────────────────

/// aho's own specs (`aho/formal/`), as the binary was built with them: the harness validates its
/// logs against the protocol it implements, wherever it runs.
const OWN_SPECS: &[(&str, &str, &str)] = &[
    ("Log", include_str!("../../../formal/Log.tla"), include_str!("../../../formal/Log.cfg")),
    ("Turn", include_str!("../../../formal/Turn.tla"), include_str!("../../../formal/Turn.cfg")),
    ("Policy", include_str!("../../../formal/Policy.tla"), include_str!("../../../formal/Policy.cfg")),
    (aho_formal::TASKS_MODULE, include_str!("../../../formal/Tasks.tla"), include_str!("../../../formal/Tasks.cfg")),
];

/// The trace records a module takes a real step for (`TraceStep(e)`); a log with none of them
/// is not a behavior worth checking against it.
fn models(module: &str, entries: &[Entry], trace: &[Value]) -> bool {
    let actions: &[&str] = match module {
        "Log" => &["TurnStart", "TurnEnd", "ApprovalRequested", "ApprovalDecided", "Call", "Response", "Result", "Replace"],
        "Turn" => &["TurnStart", "TurnEnd", "StepStart", "StepEnd", "Input", "Request", "Response", "Call", "Result", "OpStart", "OpDone"],
        "Policy" => &["Epoch", "ApprovalRequested", "ApprovalDecided"],
        _ => return entries.iter().any(|e| matches!(&e.kind, EntryKind::Custom { ns, .. } if ns == aho_tasks::NS)),
    };
    trace.iter().any(|r| r.get("action").and_then(Value::as_str).is_some_and(|a| actions.contains(&a)))
}

/// `aho session verify --formal` (every session log is a behavior): the log's trace against
/// each of aho's own TLA+ modules it has entries for, by TLC. Prints `✓`/`✕` lines in verify's
/// style and returns how many failed; without TLC it says why and fails nothing.
pub async fn verify_formal(ctx: &Ctx, entries: &[Entry]) -> Result<usize, Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let tlc = match tlc(ctx, &inputs, &p).await {
        Ok(t) => t,
        Err(why) => {
            println!("  · formal: skipped; {why}");
            return Ok(0);
        }
    };
    let aho_dir = ctx.project.join(".aho");
    std::fs::create_dir_all(&aho_dir)?;
    let scratch = tempfile::Builder::new()
        .prefix(".aho-formal-")
        .tempdir_in(&aho_dir)
        .map_err(|e| err(format!("could not create a scratch dir in {} ({e}); nothing was validated", aho_dir.display())))?;
    let mut failures = 0;
    for (module, tla, cfg) in OWN_SPECS {
        let path = scratch.path().join(format!("{module}.tla"));
        std::fs::write(&path, tla).and_then(|_| std::fs::write(path.with_extension("cfg"), cfg))?;
        let trace = aho_formal::trace_for(module, entries);
        if !models(module, entries, &trace) {
            println!("  · formal {module}.tla: no entries it models");
            continue;
        }
        let (ok, text) = match tlc.trace_validate(&SpecRef::tla(&path), &trace).await {
            Ok(o) => match (&o.verdict, &o.counterexample) {
                (Verdict::Pass, _) => {
                    (true, format!("formal {module}.tla: the log is a behavior ({} records, {})", trace.len(), cost_text(o.cost_ms)))
                }
                (Verdict::Fail { .. }, Some(c)) => (false, format!("formal {module}.tla rejects the log: {}", c.violated)),
                (Verdict::Unknown { reason }, _) => (false, format!("formal {module}.tla gave no verdict ({reason})")),
                (v, _) => (false, format!("formal {module}.tla: {}", verdict_text(v))),
            },
            Err(e @ FormalError::ToolMissing { .. }) => {
                println!("  · formal: skipped; {e}");
                return Ok(failures);
            }
            Err(e) => (false, format!("formal {module}.tla could not validate the log: {e}")),
        };
        println!("  {} {text}", if ok { "✓" } else { "✕" });
        if !ok {
            failures += 1;
        }
    }
    Ok(failures)
}
