//! Branches and worktrees: `aho fork` branches a session's log at an entry (optionally
//! under another loadout), `aho session diff` compares the folds of two heads, `aho session
//! merge|destroy` bring a worktree session's branch back or remove its worktree, and `aho
//! checkpoint export` turns a checkpoint tree into a real commit in the operator's repository.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use aho_compose::Planned;
use aho_log::{CapState, Entry, EntryId, EntryKind, JsonlLog, LogStore, State};
use aho_types::*;
use surface_rpc::caps::plan_entries;

use crate::ctx::{Ctx, Fail, SessionFile, branch, confirm, err, find_session, hex, owner_alive};
use crate::session::{marks, parse_checkpoint, state_of};

// ───────────────────────────── shared ─────────────────────────────

fn cfg_str<'a>(st: &'a State, key: &str) -> Option<&'a str> {
    st.config.get(key).and_then(|s| s.value.as_str()).filter(|s| !s.is_empty())
}

/// The shadow store the session's world wrote, keyed by its workspace root: where its
/// checkpoint trees live, and what `Snapshot::tree` hashes the workspace against; with the
/// canonical workspace root it is keyed by.
pub(crate) fn session_shadow(ctx: &Ctx, home: &Path, st: &State) -> Result<(PathBuf, world_host::ShadowGit), Fail> {
    let base = cfg_str(st, "workspace.root").map(|r| ctx.project.join(r)).unwrap_or_else(|| ctx.project.clone());
    let root = match (cfg_str(st, "workspace.layout"), cfg_str(st, "workspace.worktree")) {
        (Some("worktree"), Some(n)) => base.join(".aho").join("worktrees").join(n),
        _ => base,
    };
    let root =
        std::fs::canonicalize(&root).map_err(|e| err(format!("the session's workspace {} cannot be opened ({e})", root.display())))?;
    let shadow_home = cfg_str(st, "workspace.shadow_dir").map(PathBuf::from).unwrap_or_else(|| home.join(".aho").join("shadow"));
    let shadow = world_host::ShadowGit::new(&root, &shadow_home);
    Ok((root, shadow))
}

/// Refuses when another process drives the session (its recorded owner still runs, or the daemon holds it).
async fn quiet(ctx: &Ctx, dir: &Path, s: &SessionFile, st: &State) -> Result<(), Fail> {
    crate::serve::refuse_if_daemon_owns(ctx, dir, s.id).await?;
    if let Some(p) = st.meta.as_ref().and_then(|(_, _, p)| p.as_ref()).filter(|p| owner_alive(p)) {
        return Err(err(format!(
            "session {} is driven by process {} right now, so nothing was changed: two processes never write one log. Stop it first",
            hex(s.id),
            p.pid
        )));
    }
    Ok(())
}

/// Appends [`plan_entries`] by `Operator`; returns how many it wrote.
pub fn record_plan(log: &dyn LogStore, st: &State, p: &Planned, run: EntryId, why: &str) -> Result<usize, Fail> {
    let kinds = plan_entries(st, p, run, why);
    let n = kinds.len();
    for k in kinds {
        log.append(None, Principal::Operator, k)?;
    }
    Ok(n)
}

fn balanced_points(entries: &[Entry]) -> Vec<u64> {
    entries.iter().filter(|e| matches!(e.kind, EntryKind::TurnEnd { .. })).map(|e| e.id.0).rev().take(5).collect()
}

// ───────────────────────────── fork ─────────────────────────────

pub async fn fork(ctx: &Ctx, session: Option<&str>, at: u64) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let dir = ctx.session_dir(&p);
    let s = find_session(&dir, session)?;
    let log = crate::ctx::open_writer(&dir, s.id)?;
    let head_entries = match log.head() {
        Some(h) => log.read_branch(h)?,
        None => Vec::new(),
    };
    quiet(ctx, &dir, &s, &state_of(&head_entries)?).await?;
    let point = EntryId(at);
    if log.get(point).is_none() {
        return Err(err(format!("session {} has no entry {at}; `aho session show {}` lists them", hex(s.id), hex(s.id))));
    }
    let upto = log.read_branch(point)?;
    let st = state_of(&upto)?;
    if st.turn.is_some() || !st.open_calls.is_empty() {
        let pts: Vec<String> = balanced_points(&upto).iter().map(|n| n.to_string()).collect();
        return Err(err(format!(
            "entry {at} is inside a turn (the turn has not ended there), so a branch from it could never continue. Fork at a turn end instead: {}",
            if pts.is_empty() { "this branch has none yet".to_string() } else { pts.join(", ") }
        )));
    }
    let under = ctx.global.loadout.as_deref();
    log.fork(point)?;
    let mut args = vec!["--at".to_string(), at.to_string()];
    if let Some(l) = under {
        args.extend(["--loadout".to_string(), l.to_string()]);
    }
    let run = log.append(None, Principal::Operator, EntryKind::CommandRun { name: "fork".into(), args, key: None })?.id;
    let n = record_plan(&log, &st, &p, run, "aho fork")?;
    log.append(None, Principal::Operator, EntryKind::CommandDone { run, kind: "ok".into() })?;
    let head = log.head().map(|h| h.0).unwrap_or(0);
    println!(
        "forked session {} at entry {at}: new branch head entry {head}{}",
        hex(s.id),
        if n > 0 { format!(" ({n} entries record the plan it runs under)") } else { String::new() }
    );
    let before = head_entries.last().map(|e| e.id.0).unwrap_or(0);
    println!(
        "continue it with `aho run --session {}{}`; compare the branches with `aho session diff {before} {head} --session {}`",
        &hex(s.id)[..12.min(hex(s.id).len())],
        under.map(|l| format!(" --loadout {l}")).unwrap_or_default(),
        &hex(s.id)[..12.min(hex(s.id).len())]
    );
    Ok(())
}

// ───────────────────────────── session diff ─────────────────────────────

fn show(v: Option<&Value>) -> String {
    match v {
        None => "(unset)".into(),
        Some(Value::String(s)) => format!("\"{s}\""),
        Some(Value::Object(m)) if m.len() == 1 && m.contains_key("digest") => "(secret)".into(),
        Some(o) => surface_print::clip(&o.to_string(), 60),
    }
}

pub fn diff(ctx: &Ctx, a: u64, b: u64, session: Option<&str>) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let s = find_session(&ctx.session_dir(&p), session)?;
    let log = JsonlLog::open(&s.path)?;
    let mut heads = log.heads();
    heads.sort();
    let known = heads.iter().map(|h| h.0.to_string()).collect::<Vec<_>>().join(", ");
    let read = |n: u64| -> Result<Vec<Entry>, Fail> {
        if log.get(EntryId(n)).is_none() {
            return Err(err(format!("session {} has no entry {n}; its branch heads are {known}", hex(s.id))));
        }
        Ok(log.read_branch(EntryId(n))?)
    };
    let (ea, eb) = (read(a)?, read(b)?);
    let (sa, sb) = (state_of(&ea)?, state_of(&eb)?);
    let common = ea.iter().zip(eb.iter()).take_while(|(x, y)| x.id == y.id).count();
    println!("session {}: entry {a} vs entry {b}", hex(s.id));
    println!(
        "  shared prefix: {} entries{}",
        common,
        ea.get(common.wrapping_sub(1)).map(|e| format!(" (they part after entry {})", e.id.0)).unwrap_or_default()
    );
    println!("  a: {} entries, {} turns · b: {} entries, {} turns", ea.len(), sa.turns, eb.len(), sb.turns);
    let mut rows = 0;
    let keys: BTreeSet<&String> = sa.config.keys().chain(sb.config.keys()).collect();
    for k in keys {
        let (x, y) = (sa.config.get(k).map(|s| &s.value), sb.config.get(k).map(|s| &s.value));
        if x != y {
            if rows == 0 {
                println!("config:");
            }
            rows += 1;
            println!("  ~ {k:<34} {} → {}", show(x), show(y));
        }
    }
    let ids: BTreeSet<&String> = sa.caps.keys().chain(sb.caps.keys()).collect();
    let mut caps = 0;
    for id in ids {
        let (x, y) = (sa.caps.get(id), sb.caps.get(id));
        if x != y {
            if caps == 0 {
                println!("capabilities:");
            }
            caps += 1;
            let name = |s: Option<&CapState>| {
                s.map(|s| format!("{} {}", aho_inspect::glyph(s), aho_inspect::state_name(s))).unwrap_or_else(|| "(absent)".into())
            };
            println!("  ~ {id:<26} {} → {}", name(x), name(y));
        }
    }
    if sa.policy != sb.policy {
        println!(
            "policy: epoch {} {} [{}] → epoch {} {} [{}]",
            sa.policy.epoch.0,
            sa.policy.preset,
            aho_inspect::render_classes(sa.policy.allowed),
            sb.policy.epoch.0,
            sb.policy.preset,
            aho_inspect::render_classes(sb.policy.allowed)
        );
    }
    if sa.plan_hash != sb.plan_hash {
        println!("plan: {} → {}", sa.plan_hash.map(|h| h.short()).unwrap_or_default(), sb.plan_hash.map(|h| h.short()).unwrap_or_default());
    }
    let (ca, cb) = (aho_inspect::catch_up(&sa), aho_inspect::catch_up(&sb));
    let tools = |c: &aho_inspect::CatchUp| c.tool_counts.iter().cloned().collect::<BTreeMap<String, u32>>();
    let (ta, tb) = (tools(&ca), tools(&cb));
    let names: BTreeSet<&String> = ta.keys().chain(tb.keys()).collect();
    let calls: Vec<String> = names
        .into_iter()
        .filter(|n| ta.get(*n) != tb.get(*n))
        .map(|n| format!("{n} {}→{}", ta.get(n).copied().unwrap_or(0), tb.get(n).copied().unwrap_or(0)))
        .collect();
    if !calls.is_empty() {
        println!("tool calls: {}", calls.join("  "));
    }
    let (fa, fb): (BTreeSet<&String>, BTreeSet<&String>) = (ca.files_touched.iter().collect(), cb.files_touched.iter().collect());
    let only_a: Vec<&str> = fa.difference(&fb).map(|s| s.as_str()).collect();
    let only_b: Vec<&str> = fb.difference(&fa).map(|s| s.as_str()).collect();
    if !only_a.is_empty() || !only_b.is_empty() {
        println!("files touched: only a: {} · only b: {}", only_a.join(" "), only_b.join(" "));
    }
    let u = |s: &State| format!("{} in / {} out tokens", s.usage.input_tokens, s.usage.output_tokens);
    println!("usage: a {} · b {}", u(&sa), u(&sb));
    let last = |c: &aho_inspect::CatchUp| c.last_response.as_deref().map(|r| surface_print::clip(r, 70)).unwrap_or_else(|| "(none)".into());
    println!("last response: a: {}\n               b: {}", last(&ca), last(&cb));
    if rows == 0 && caps == 0 && sa.policy == sb.policy {
        println!("(same configuration, capabilities and policy on both branches)");
    }
    Ok(())
}

// ───────────────────────────── worktree sessions ─────────────────────────────

fn git(dir: &Path, args: &[&str]) -> Result<String, Fail> {
    let out = clean_git(Command::new("git").arg("-C").arg(dir).args(args))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(format!("could not run git ({e}); install git")))?;
    if !out.status.success() {
        return Err(err(format!("`git {}` failed in {}: {}", args.join(" "), dir.display(), String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn clean_git(cmd: &mut Command) -> &mut Command {
    for k in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY"] {
        cmd.env_remove(k);
    }
    cmd
}

struct Worktree {
    s: SessionFile,
    base: PathBuf,
    path: PathBuf,
    name: String,
    st: State,
}

/// The worktree a session worked in, from its own logged config.
async fn worktree(ctx: &Ctx, id: Option<&str>) -> Result<Worktree, Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let dir = ctx.session_dir(&p);
    let s = find_session(&dir, id)?;
    let (_, entries) = branch(&s.path)?;
    let st = state_of(&entries)?;
    quiet(ctx, &dir, &s, &st).await?;
    let base = cfg_str(&st, "workspace.root").map(|r| ctx.project.join(r)).unwrap_or_else(|| ctx.project.clone());
    if cfg_str(&st, "workspace.layout") != Some("worktree") {
        return Err(err(format!(
            "session {} worked in place (workspace.layout = in-place), so its changes are already in {} and there is no worktree. Sessions get one with `aho set workspace.layout worktree`",
            hex(s.id),
            base.display()
        )));
    }
    let name = cfg_str(&st, "workspace.worktree")
        .ok_or_else(|| err("the session names no workspace.worktree, so its worktree is unknown"))?
        .to_string();
    let path = base.join(".aho").join("worktrees").join(&name);
    Ok(Worktree { s, base, path, name, st })
}

fn log_command(w: &Worktree, name: &str, args: Vec<String>, kind: &str) -> Result<(), Fail> {
    let dir = w.s.path.parent().ok_or_else(|| err(format!("{} has no directory", w.s.path.display())))?;
    let log = crate::ctx::open_writer(dir, w.s.id)?;
    let run = log.append(None, Principal::Operator, EntryKind::CommandRun { name: name.into(), args, key: None })?.id;
    log.append(None, Principal::Operator, EntryKind::CommandDone { run, kind: kind.into() })?;
    Ok(())
}

/// Non-blank lines of SPEC.md §9 (Coverage: spec debt), when the file parses.
fn spec_debt(path: &Path) -> Option<usize> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc = aho_formal::SpecDoc::parse(&text).ok()?;
    let sec = doc.section(aho_formal::SpecSection::Coverage)?;
    Some(sec.body.lines().filter(|l| !l.trim().is_empty()).count())
}

pub async fn merge(ctx: &Ctx, id: Option<&str>, reason: Option<&str>) -> Result<(), Fail> {
    let w = worktree(ctx, id).await?;
    let br = format!("aho/{}", w.name);
    if !w.path.is_dir() {
        return Err(err(format!("the worktree {} is gone; branch {br} may still hold the work (`git log {br}`)", w.path.display())));
    }
    let doc = cfg_str(&w.st, "formal.tools.spec_doc").unwrap_or("spec/SPEC.md").to_string();
    if let (Some(before), Some(after)) = (spec_debt(&w.base.join(&doc)), spec_debt(&w.path.join(&doc)))
        && after > before
        && reason.is_none()
    {
        return Err(Fail::Plan(format!(
            "the spec debt in {doc} §9 grew ({before} → {after} lines) on {br}, so nothing was merged. Cover it in spec mode (`aho spec`), or merge anyway with --reason \"…\" (logged)"
        )));
    }

    let dirty = git(&w.path, &["status", "--porcelain"])?;
    let changed = dirty.lines().count();
    let into = git(&w.base, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    println!("plan:");
    if changed > 0 {
        println!("  commit {changed} changed path(s) in {} on {br}", w.path.display());
    }
    println!("  merge {br} into {into} in {} (a merge commit; nothing is rebased)", w.base.display());
    if let Some(r) = reason {
        println!("  log the override reason in session {}: {r}", hex(w.s.id));
    }
    if !confirm(ctx.global.yes)? {
        println!("nothing changed.");
        return Ok(());
    }
    if changed > 0 {
        git(&w.path, &["add", "-A"])?;
        git(&w.path, &["commit", "-q", "-m", &format!("aho session {}: work at merge", hex(w.s.id))])?;
    }
    let mut args = vec![br.clone()];
    if let Some(r) = reason {
        args.extend(["--reason".to_string(), r.to_string()]);
    }
    match git(&w.base, &["merge", "--no-ff", "-m", &format!("Merge {br} (aho session {})", hex(w.s.id)), &br]) {
        Ok(_) => {
            log_command(&w, "session merge", args, "ok")?;
            println!("merged {br} into {into}; `aho session destroy {}` removes the worktree", &hex(w.s.id)[..12.min(hex(w.s.id).len())]);
            Ok(())
        }
        Err(e) => {
            log_command(&w, "session merge", args, "conflict")?;
            Err(err(format!("{e}\nthe merge stopped in {}: resolve and `git commit`, or `git merge --abort`", w.base.display())))
        }
    }
}

pub async fn destroy(ctx: &Ctx, id: Option<&str>) -> Result<(), Fail> {
    let w = worktree(ctx, id).await?;
    if !w.path.is_dir() {
        return Err(err(format!("the worktree {} is already gone; nothing to remove", w.path.display())));
    }
    let dirty = git(&w.path, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(err(format!(
            "the worktree {} has {} uncommitted change(s), so it was kept. Bring them back with `aho session merge`, or commit them on aho/{} first",
            w.path.display(),
            dirty.lines().count(),
            w.name
        )));
    }
    println!("plan:\n  remove the worktree {} (branch aho/{} and the session log stay)", w.path.display(), w.name);
    if !confirm(ctx.global.yes)? {
        println!("nothing changed.");
        return Ok(());
    }
    git(&w.base, &["worktree", "remove", &w.path.display().to_string()])?;
    log_command(&w, "session destroy", vec![w.path.display().to_string()], "ok")?;
    println!("removed {}; the branch aho/{} keeps its commits", w.path.display(), w.name);
    Ok(())
}

// ───────────────────────────── checkpoint export ─────────────────────────────

pub fn export(ctx: &Ctx, checkpoint: &str, branch_name: &str, session: Option<&str>) -> Result<(), Fail> {
    let to = parse_checkpoint(checkpoint)?;
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    let s = find_session(&ctx.session_dir(&p), session)?;
    let (_, entries) = branch(&s.path)?;
    let ms = marks(&entries);
    let m = ms
        .iter()
        .find(|m| m.id == to)
        .ok_or_else(|| err(format!("session {} has no checkpoint c{}; `aho checkpoints` lists them", hex(s.id), to.0)))?;
    let st = state_of(&entries)?;
    let (root, shadow) = session_shadow(ctx, &inputs.home, &st)?;
    let git_dir = format!("--git-dir={}", shadow.git_dir().display());
    let tree = m.tree.to_hex();
    let kind = clean_git(Command::new("git").arg(&git_dir).args(["cat-file", "-t", &tree]))
        .output()
        .map_err(|e| err(format!("could not run git ({e})")))?;
    if String::from_utf8_lossy(&kind.stdout).trim() != "tree" {
        return Err(err(format!(
            "checkpoint c{} (tree {}) is not in the shadow store {}; `aho gc` may have pruned it",
            to.0,
            m.tree.short(),
            shadow.git_dir().display()
        )));
    }
    let top = git(&root, &["rev-parse", "--show-toplevel"])
        .map_err(|_| err(format!("{} is not in a git repository; `git init` it to export checkpoints as commits", root.display())))?;
    let top = PathBuf::from(top);
    git(&top, &["check-ref-format", "--branch", branch_name]).map_err(|_| err(format!("`{branch_name}` is not a valid branch name")))?;
    let refname = format!("refs/heads/{branch_name}");
    let checked_out = git(&top, &["worktree", "list", "--porcelain"])?.lines().any(|l| l == format!("branch {refname}"));
    if checked_out {
        return Err(err(format!("{branch_name} is checked out, so its ref was not moved under your files. Export to another --branch")));
    }
    // Object formats differ (the shadow store is SHA-256), so the files cross as a tar stream.
    let scratch = tempfile::tempdir().map_err(|e| err(format!("could not create a scratch directory ({e})")))?;
    let files = scratch.path().join("tree");
    std::fs::create_dir(&files)?;
    let archive = clean_git(Command::new("git").arg(&git_dir).args(["archive", "--format=tar", &tree]))
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| err(format!("could not run git archive ({e})")))?;
    let tar_in = archive.stdout.ok_or_else(|| err("git archive gave no output"))?;
    let untar =
        Command::new("tar").arg("-x").arg("-C").arg(&files).stdin(tar_in).status().map_err(|e| err(format!("could not run tar ({e})")))?;
    if !untar.success() {
        return Err(err("unpacking the checkpoint tree failed; nothing was exported"));
    }
    let index = scratch.path().join("index");
    let with_index = |args: &[&str]| -> Result<String, Fail> {
        let out = clean_git(
            Command::new("git")
                .arg("-C")
                .arg(&top)
                .env("GIT_INDEX_FILE", &index)
                .arg(format!("--work-tree={}", files.display()))
                .args(args),
        )
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(format!("could not run git ({e})")))?;
        if !out.status.success() {
            return Err(err(format!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    with_index(&["add", "-A", "-f", "."])?;
    let commit_tree = with_index(&["write-tree"])?;
    // `rev-parse --verify` fails exactly when the ref does not exist (an unborn repo has no HEAD).
    let parent = git(&top, &["rev-parse", "--verify", "-q", &refname])
        .or_else(|_| git(&top, &["rev-parse", "--verify", "-q", "HEAD"]))
        .ok()
        .filter(|h| !h.is_empty());
    let msg = format!(
        "aho checkpoint c{} of session {}\n\nturn {}: {}\ncheckpoint tree {} (aho shadow store)\n",
        to.0,
        hex(s.id),
        m.turn,
        surface_print::clip(&m.prompt, 60),
        tree
    );
    let mut ct = Command::new("git");
    clean_git(ct.arg("-C").arg(&top).arg("commit-tree").arg(&commit_tree));
    if let Some(p) = &parent {
        ct.arg("-p").arg(p);
    }
    let mut child = ct
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| err(format!("could not run git ({e})")))?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(msg.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(err(format!("`git commit-tree` failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut update = vec!["update-ref", refname.as_str(), commit.as_str()];
    let old = git(&top, &["rev-parse", "--verify", "-q", &refname]).ok().filter(|h| !h.is_empty());
    if let Some(o) = &old {
        update.push(o.as_str());
    }
    git(&top, &update)?;
    println!(
        "c{} → commit {} on {branch_name}{} in {}",
        to.0,
        &commit[..12.min(commit.len())],
        parent.map(|p| format!(" (parent {})", &p[..12.min(p.len())])).unwrap_or_default(),
        top.display()
    );
    Ok(())
}
