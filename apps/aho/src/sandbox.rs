//! Operator verbs over a session's sandbox (07): `aho sandbox status|destroy`, `aho export`, and
//! the interactive `aho shell` into a namespace sandbox. None of them needs the session's
//! network, so they work while the session runs in another process.

use std::path::{Path, PathBuf};

use aho_cap::Config;
use aho_fs::Fs;
use aho_log::EntryKind;
use serde_json::json;
use world_ns::{NamespaceWorld, NsConfig, NsSettings};

use crate::cli::SandboxVerb;
use crate::ctx::{Ctx, Fail, SessionFile, branch, confirm, err, find_session, hex};
use crate::record::Writer;
use crate::session::state_of;

/// The session a sandbox verb acts on, and its namespace sandbox configuration.
struct Target {
    dir: PathBuf,
    session: SessionFile,
    cfg: NsConfig,
}

fn namespace(ctx: &Ctx, p: &aho_compose::Planned, session: Option<&str>) -> Result<Target, Fail> {
    let world = &p.plan.world.name;
    if world != "namespace" {
        return Err(err(format!(
            "this project's world is `{world}`, which has no sandbox to act on; `aho set seams.world namespace` (Linux) or `vm` selects a sandbox"
        )));
    }
    let dir = ctx.session_dir(p);
    let s = find_session(&dir, session)?;
    let settings = NsSettings::from_loadout(&p.loadout).map_err(|e| err(format!("{e:?}")))?;
    let cfg = NsConfig::new(&settings, &dir, s.id, &ctx.workspace(p));
    Ok(Target { dir, session: s, cfg })
}

pub async fn run(ctx: &Ctx, sub: &SandboxVerb, session: Option<&str>, json_out: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    if p.plan.world.name == "vm" {
        let (dir, s) = crate::vm::find(ctx, &p, session)?;
        return match sub {
            SandboxVerb::Status => crate::vm::sandbox_status(ctx, &p, &s, json_out),
            SandboxVerb::Destroy { yes } => crate::vm::sandbox_destroy(ctx, &p, &dir, &s, *yes).await,
        };
    }
    let t = namespace(ctx, &p, session)?;
    match sub {
        SandboxVerb::Status => status(&t, json_out),
        SandboxVerb::Destroy { yes } => destroy(ctx, &t, *yes).await,
    }
}

fn status(t: &Target, json_out: bool) -> Result<(), Fail> {
    let st = world_ns::inspect(&t.cfg.dir);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({"session": hex(t.session.id), "world": "namespace", "sandbox": st}))?);
        return Ok(());
    }
    if !st.exists {
        println!("session {} has no sandbox yet; it is created when the session first runs", hex(t.session.id));
        return Ok(());
    }
    let state = match st.keeper_pid {
        Some(pid) => format!("running (keeper pid {pid})"),
        None => "stopped (restarted when the session or `aho shell` next enters it)".into(),
    };
    println!("sandbox of session {}: {state}", hex(t.session.id));
    println!("  workspace   {} (inside)", st.workspace);
    println!("  recipe      {}", t.cfg.rootfs.display());
    println!("  state       {}", st.state_dir.display());
    if let Some(nic) = &st.nic {
        println!("  nic         {} {} · {} (via {})", nic.name, nic.ipv4, nic.ipv6, nic.gateway_v4);
    }
    println!("  enforced    {}", st.enforced_by);
    Ok(())
}

async fn destroy(ctx: &Ctx, t: &Target, yes: bool) -> Result<(), Fail> {
    if !world_ns::inspect(&t.cfg.dir).exists && !t.cfg.dir.exists() {
        println!("session {} has no sandbox; nothing to destroy", hex(t.session.id));
        return Ok(());
    }
    println!(
        "destroy the sandbox of session {}: stop every command in it and delete its workspace, /session volume and installed packages ({})",
        hex(t.session.id),
        t.cfg.dir.display()
    );
    println!("  files not exported (`aho export`) or pushed are lost; checkpoints of the workspace stay in the session log");
    if !confirm(yes)? {
        println!("nothing was destroyed");
        return Ok(());
    }
    // Recorded as the operator: secure the writer first, so a refusal leaves the sandbox intact.
    let (_, entries) = branch(&t.session.path)?;
    let w = Writer::open(ctx, &t.dir, &t.session, &state_of(&entries)?)
        .await
        .map_err(|why| err(format!("{why}, so the sandbox was not destroyed. Run this from the client that drives it")))?;
    let removed = match world_ns::destroy(&t.cfg.dir) {
        Ok(removed) => removed,
        Err(e) => {
            w.finish(false).await.map_err(err)?;
            return Err(err(format!("the sandbox could not be destroyed: {e}")));
        }
    };
    let kind = EntryKind::Custom { ns: "Sandbox".into(), name: "Destroyed".into(), body: json!({"world": "namespace", "dir": t.cfg.dir}) };
    let appended = w.append(kind).await;
    let via = w.finish(false).await.map_err(err)?;
    appended.map_err(|e| err(format!("the sandbox was destroyed but it could not be recorded: {e}")))?;
    let via = via.map(|v| format!(" {v}")).unwrap_or_default();
    if removed {
        println!(
            "destroyed the sandbox of session {} (by Operator{via}); the next turn creates a fresh one from the recipe",
            hex(t.session.id)
        );
    }
    Ok(())
}

/// `aho shell` with no command under the namespace world: the operator's shell inside the
/// session's sandbox, on this terminal.
pub fn namespace_shell(ctx: &Ctx, p: &aho_compose::Planned, session: Option<&str>) -> Result<(), Fail> {
    let t = namespace(ctx, p, session)?;
    let world = NamespaceWorld::existing(t.cfg).map_err(|e| err(format!("{e:?}")))?;
    let argv = aho_sandbox::Sandbox::operator_shell(&*world.sandbox());
    let Some((prog, args)) = argv.split_first() else {
        return Err(err("the namespace world returned no shell command; nothing was started"));
    };
    println!(
        "operator shell in {} of session {} (enforced by: {}); exit to return",
        world.root().display(),
        hex(t.session.id),
        world_ns::ENFORCED_BY
    );
    let status = std::process::Command::new(prog).args(args).status().map_err(|e| err(format!("could not start {prog} ({e})")))?;
    if status.success() { Ok(()) } else { Err(Fail::Reported) }
}

/// `aho export SRC DST`: copies a file or directory out of the session's world to the host.
/// Nothing on the host is overwritten: every target is checked before the first byte is written.
pub async fn export(ctx: &Ctx, src: &str, dst: &Path, session: Option<&str>) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let ((files, bytes), from) = match p.plan.world.name.as_str() {
        "namespace" => {
            let t = namespace(ctx, &p, session)?;
            let world = NamespaceWorld::existing(t.cfg).map_err(|e| err(format!("{e:?}")))?;
            (copy_out(&*world.fs(), src, dst).await?, format!("the sandbox of session {}", hex(t.session.id)))
        }
        "vm" => {
            let (_, s) = crate::vm::find(ctx, &p, session)?;
            let fs = crate::vm::sandbox_fs(ctx, &p, &s)?;
            (copy_out(&*fs, src, dst).await?, format!("the sandbox VM of session {}", hex(s.id)))
        }
        "ssh" => {
            let cfg = <world_ssh::SshConfig as Config>::from_loadout(&p.loadout).map_err(|e| err(format!("{e:?}")))?;
            let world = world_ssh::SshWorld::connect(cfg).map_err(|e| err(format!("{e:?}")))?;
            (copy_out(world.fs(), src, dst).await?, format!("the ssh world ({})", world.root()))
        }
        world => {
            return Err(err(format!(
                "this project's world is `{world}`: its workspace is already on this disk at {}; copy from there",
                ctx.workspace(&p).display()
            )));
        }
    };
    println!("exported {files} file(s), {bytes} bytes, from {from}: {src} → {}", dst.display());
    Ok(())
}

/// Plans every (sandbox path, host path) pair first, refuses any existing host file, then copies.
async fn copy_out(fs: &dyn Fs, src: &str, dst: &Path) -> Result<(usize, u64), Fail> {
    let e = |what: String| move |x: aho_fs::FsError| err(format!("{what}: {x}; nothing was exported"));
    let top = fs.stat(src).await.map_err(e(format!("`{src}` in the sandbox")))?;
    let name = Path::new(&top.path).file_name().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("workspace"));
    // `cp -r` semantics: into an existing directory, else as the new name.
    let base = if dst.is_dir() { dst.join(name) } else { dst.to_path_buf() };
    let mut plan: Vec<(String, PathBuf)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if top.is_dir {
        let mut stack = vec![(top.path.clone(), base.clone())];
        while let Some((dir, host)) = stack.pop() {
            dirs.push(host.clone());
            for entry in fs.list(&dir).await.map_err(e(format!("listing `{dir}`")))? {
                let leaf = Path::new(&entry.path).file_name().map(PathBuf::from).unwrap_or_default();
                if entry.is_dir {
                    stack.push((entry.path, host.join(leaf)));
                } else {
                    plan.push((entry.path, host.join(leaf)));
                }
            }
        }
    } else {
        plan.push((top.path.clone(), base.clone()));
    }
    if let Some((_, taken)) = plan.iter().find(|(_, h)| std::fs::symlink_metadata(h).is_ok()) {
        return Err(err(format!("{} already exists; nothing was exported (export into a new or empty directory)", taken.display())));
    }
    if let Some(taken) = dirs.iter().find(|d| std::fs::symlink_metadata(d).is_ok_and(|m| !m.is_dir())) {
        return Err(err(format!("{} exists and is not a directory; nothing was exported", taken.display())));
    }
    for d in &dirs {
        std::fs::create_dir_all(d)?;
    }
    let mut bytes = 0u64;
    for (done, (inside, host)) in plan.iter().enumerate() {
        let data =
            fs.read_bytes(inside).await.map_err(|x| err(format!("reading `{inside}`: {x}; {done} file(s) were exported before it")))?;
        if let Some(parent) = host.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::OpenOptions::new().write(true).create_new(true).open(host).and_then(|mut f| std::io::Write::write_all(&mut f, &data))?;
        bytes += data.len() as u64;
    }
    Ok((plan.len(), bytes))
}
