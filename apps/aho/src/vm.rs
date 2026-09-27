//! The operator's side of the VM lab: `aho lease grant|revoke|status` (what a
//! session's machines may use; the agent can only request), `aho media import|list` and
//! `aho add recipe` (the library machines are built from; no agent tool writes it), and the
//! `vm` world under `aho sandbox` and `aho export`. None of them opens the session's network.

use std::path::{Path, PathBuf};

use aho_cap::Config;
use aho_log::EntryKind;
use aho_types::{LeaseId, Timestamp};
use aho_vm::{Acceleration, Architecture, LeaseLimits, Recipe};
use serde_json::json;
use world_vm::{Catalog, LeaseTerms, VmSettings, session_name};

use crate::cli::{LeaseVerb, MediaVerb, RecipeArgs};
use crate::ctx::{Ctx, Fail, SessionFile, branch, confirm, err, find_session, hex};
use crate::record::Writer;
use crate::session::state_of;

fn settings(p: &aho_compose::Planned) -> Result<VmSettings, Fail> {
    VmSettings::from_loadout(&p.loadout).map_err(|e| err(format!("{e:?}")))
}

fn catalog(ctx: &Ctx, p: &aho_compose::Planned) -> Result<Catalog, Fail> {
    let s = settings(p)?;
    let home = ctx.inputs(&[], false)?.home;
    Catalog::open(&s.root(&home), &s.qemu_img).map_err(|e| err(e.to_string()))
}

fn state_dir(ctx: &Ctx, p: &aho_compose::Planned) -> Result<PathBuf, Fail> {
    Ok(settings(p)?.root(&ctx.inputs(&[], false)?.home).join("state"))
}

/// `6h`, `30m`, `2d`, `90s`: milliseconds.
pub fn duration_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = digits.parse().ok()?;
    let mul = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" | "" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(mul).filter(|ms| *ms > 0)
}

fn bytes(s: &str, what: &str) -> Result<u64, Fail> {
    tool_vm::size(s).filter(|b| *b > 0).ok_or_else(|| err(format!("{what} `{s}` is not a size; write e.g. 512M, 8G")))
}

// ───────────────────────────── leases ─────────────────────────────

pub async fn lease(ctx: &Ctx, sub: &LeaseVerb, session: Option<&str>, json_out: bool) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let dir = ctx.session_dir(&p);
    let s = find_session(&dir, session)?;
    let state = state_dir(ctx, &p)?;
    let (_, entries) = branch(&s.path)?;
    let live = world_vm::lease_of(&entries);
    match sub {
        LeaseVerb::Status => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&json!({"session": hex(s.id), "lease": live}))?);
                return Ok(());
            }
            match live {
                None => println!("session {} holds no VM lease; `aho lease grant` gives it one", hex(s.id)),
                Some(l) => {
                    let left = l.expires_at_ms.saturating_sub(Timestamp::now().0) / 60_000;
                    println!(
                        "session {}: lease {} · {} machine(s), {} MiB disk, {} s CPU · expires in {left} min (granted at entry #{})",
                        hex(s.id),
                        l.id,
                        l.max_machines,
                        l.max_disk_bytes >> 20,
                        l.max_cpu_s,
                        l.granted_ref
                    );
                }
            }
            Ok(())
        }
        LeaseVerb::Grant { machines, disk, cpu, expires } => {
            let terms = LeaseTerms {
                max_machines: *machines,
                max_disk_bytes: bytes(disk, "--disk")?,
                max_cpu_s: duration_ms(cpu).ok_or_else(|| err(format!("--cpu `{cpu}` is a duration (e.g. 6h)")))? / 1000,
            };
            let ttl = duration_ms(expires).ok_or_else(|| err(format!("--expires `{expires}` is a duration (e.g. 6h)")))?;
            if terms.max_machines == 0 {
                return Err(err("--machines must be at least 1 (the sandbox VM counts)"));
            }
            let next = entries
                .iter()
                .filter_map(|e| match &e.kind {
                    EntryKind::LeaseGranted { lease, .. } => Some(lease.0),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
                + 1;
            let expires_at = Timestamp(Timestamp::now().0 + ttl);
            let w = writer(ctx, &dir, &s, &entries, "the lease was not granted").await?;
            let appended = w.append(EntryKind::LeaseGranted { lease: LeaseId(next), subject: terms.subject(), expires: expires_at }).await;
            let via = w.finish(false).await.map_err(err)?;
            let at = appended.map_err(|e| err(format!("{e}; the lease was not granted")))?;
            let limits = LeaseLimits {
                id: next.to_string(),
                granted_ref: at.0.to_string(),
                expires_at_ms: expires_at.0,
                max_machines: terms.max_machines,
                max_disk_bytes: terms.max_disk_bytes,
                max_cpu_s: terms.max_cpu_s,
                revoked: false,
            };
            world_vm::set_lease(&state, &session_name(s.id), &limits)
                .map_err(|e| err(format!("the grant is logged (entry #{}) but the VM state was not updated: {e}", at.0)))?;
            let via = via.map(|v| format!(" {v}")).unwrap_or_default();
            println!(
                "granted lease {next} to session {} (by Operator{via}): {} machine(s), {disk} disk, {cpu} CPU, for {expires}. A session started before this picks it up when it reopens.",
                hex(s.id),
                terms.max_machines
            );
            Ok(())
        }
        LeaseVerb::Revoke => {
            let Some(l) = live else {
                println!("session {} holds no VM lease; nothing to revoke", hex(s.id));
                return Ok(());
            };
            let w = writer(ctx, &dir, &s, &entries, "the lease was not revoked").await?;
            let lease = LeaseId(l.id.parse().unwrap_or_default());
            let appended = w.append(EntryKind::LeaseRevoked { lease, proof_next_attempt: None }).await;
            let via = w.finish(false).await.map_err(err)?;
            appended.map_err(|e| err(format!("{e}; the lease was not revoked")))?;
            world_vm::revoke_lease(&state, &session_name(s.id))
                .map_err(|e| err(format!("the revocation is logged but the VM state was not updated: {e}")))?;
            let via = via.map(|v| format!(" {v}")).unwrap_or_default();
            println!(
                "revoked lease {} of session {} (by Operator{via}); its machines stop at their next check and no VM effect runs",
                l.id,
                hex(s.id)
            );
            Ok(())
        }
    }
}

async fn writer(ctx: &Ctx, dir: &Path, s: &SessionFile, entries: &[aho_log::Entry], what: &str) -> Result<Writer, Fail> {
    Writer::open(ctx, dir, s, &state_of(entries)?)
        .await
        .map_err(|why| err(format!("{why}, so {what}: two processes never write one log. Run this from the client that drives it")))
}

// ───────────────────────────── the library ─────────────────────────────

pub fn media(ctx: &Ctx, sub: &MediaVerb) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let c = catalog(ctx, &p)?;
    match sub {
        MediaVerb::List { json } => {
            let all = c.media().map_err(|e| err(e.to_string()))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&all)?);
            } else if all.is_empty() {
                println!("no media in {} yet; `aho media import <file>` adds one", c.dir().display());
            }
            if !*json {
                for m in all {
                    println!(
                        "{:<28} {:<8} {:>8} MiB  blake3 {}",
                        m.name,
                        format!("{:?}", m.kind).to_lowercase(),
                        m.bytes >> 20,
                        &m.hash[..12.min(m.hash.len())]
                    );
                }
            }
            Ok(())
        }
        MediaVerb::Import { file, name, kind } => {
            let name = match name {
                Some(n) => n.clone(),
                None => file.file_stem().map(|s| s.to_string_lossy().replace('.', "-")).unwrap_or_default(),
            };
            let kind =
                match kind {
                    Some(k) => world_vm::media_kind(k)
                        .ok_or_else(|| err(format!("--kind `{k}`: use raw, qcow2, iso, kernel, initrd or firmware")))?,
                    None => kind_of(file)
                        .ok_or_else(|| err(format!("cannot tell what {} is from its extension; pass --kind", file.display())))?,
                };
            let m = c.import_media(&name, file, kind).map_err(|e| err(e.to_string()))?;
            println!(
                "imported {} as media:{} ({:?}, {} MiB, blake3 {}); recipes name it with --base/--cdrom/--kernel media:{}",
                file.display(),
                m.name,
                m.kind,
                m.bytes >> 20,
                &m.hash[..12],
                m.name
            );
            Ok(())
        }
    }
}

fn kind_of(file: &Path) -> Option<aho_vm::MediaKind> {
    use aho_vm::MediaKind::*;
    Some(match file.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "iso" => Iso,
        "qcow2" => Qcow2,
        "img" | "raw" => Raw,
        "fd" => Firmware,
        _ => return None,
    })
}

fn host_arch() -> Architecture {
    if cfg!(target_arch = "aarch64") { Architecture::Aarch64 } else { Architecture::X86_64 }
}

pub fn add_recipe(ctx: &Ctx, r: &RecipeArgs) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let c = catalog(ctx, &p)?;
    let media = |v: &Option<String>, flag: &str| -> Result<Option<String>, Fail> {
        v.as_deref()
            .map(|m| {
                m.strip_prefix("media:")
                    .map(str::to_string)
                    .ok_or_else(|| err(format!("{flag} names catalog media as `media:<name>` (`aho media list`)")))
            })
            .transpose()
    };
    let architecture = match r.arch.as_deref() {
        None => host_arch(),
        Some("aarch64" | "arm64") => Architecture::Aarch64,
        Some("x86_64" | "amd64") => Architecture::X86_64,
        Some(a) => return Err(err(format!("--arch `{a}`: use aarch64 or x86_64"))),
    };
    let acceleration = match r.accel.as_deref() {
        None if architecture != host_arch() => Acceleration::Tcg,
        None if cfg!(target_os = "macos") => Acceleration::Hvf,
        None if cfg!(target_os = "linux") => Acceleration::Kvm,
        None | Some("tcg") => Acceleration::Tcg,
        Some("kvm") => Acceleration::Kvm,
        Some("hvf") => Acceleration::Hvf,
        Some(a) => return Err(err(format!("--accel `{a}`: use tcg, kvm or hvf"))),
    };
    let memory_mib = u32::try_from(bytes(&r.mem, "--mem")? >> 20).map_err(|_| err("--mem is too large"))?;
    let recipe = Recipe {
        name: r.name.clone(),
        architecture,
        acceleration,
        memory_mib,
        cpus: r.cpus,
        disk_bytes: bytes(&r.disk, "--disk")?,
        base_media: media(&r.base, "--base")?,
        cdrom_media: media(&r.cdrom, "--cdrom")?,
        kernel_media: media(&r.kernel, "--kernel")?,
        initrd_media: media(&r.initrd, "--initrd")?,
        firmware_media: media(&r.firmware, "--firmware")?,
        kernel_args: r.append.clone().unwrap_or_default(),
        workspace: r.workspace.clone(),
    };
    if c.recipes().map_err(|e| err(e.to_string()))?.iter().any(|x| x.name == recipe.name) && !r.replace {
        return Err(err(format!("recipe {} exists; pass --replace to overwrite it (machines built from it keep their copy)", recipe.name)));
    }
    println!(
        "recipe {}: {:?} ({:?}), {} vCPU, {memory_mib} MiB, {} disk, workspace {}",
        recipe.name, recipe.architecture, recipe.acceleration, recipe.cpus, r.disk, recipe.workspace
    );
    if !confirm(ctx.global.yes)? {
        println!("nothing written.");
        return Ok(());
    }
    c.put_recipe(&recipe).map_err(|e| err(e.to_string()))?;
    println!(
        "wrote recipe {} to {}; the agent creates machines from it (`vm_create`), or `aho set vm.recipe {}` makes it the sandbox VM",
        recipe.name,
        c.dir().join("recipes").display(),
        recipe.name
    );
    Ok(())
}

// ───────────────────────────── the vm world's sandbox ─────────────────────────────

pub fn sandbox_status(ctx: &Ctx, p: &aho_compose::Planned, s: &SessionFile, json_out: bool) -> Result<(), Fail> {
    let record = world_vm::sandbox_record(&state_dir(ctx, p)?, &session_name(s.id)).map_err(|e| err(e.to_string()))?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({"session": hex(s.id), "world": "vm", "sandbox": record}))?);
        return Ok(());
    }
    let Some(m) = record else {
        println!("session {} has no sandbox VM yet; it is created from `vm.recipe` when the session first runs", hex(s.id));
        return Ok(());
    };
    println!("sandbox VM of session {}: {:?}", hex(s.id), m.state);
    println!("  recipe      {} ({:?}, {} vCPU, {} MiB)", m.recipe.name, m.recipe.architecture, m.recipe.cpus, m.recipe.memory_mib);
    println!("  workspace   {} (inside)", m.recipe.workspace);
    println!("  disks       {}", m.disks.iter().map(|d| format!("{} {} MiB", d.name, d.bytes >> 20)).collect::<Vec<_>>().join(", "));
    println!("  cpu used    {} s (lease {})", m.cpu_budget_used_ms / 1000, m.lease_id);
    println!("  enforced    {}", world_vm::ENFORCED_BY);
    Ok(())
}

pub async fn sandbox_destroy(ctx: &Ctx, p: &aho_compose::Planned, dir: &Path, s: &SessionFile, yes: bool) -> Result<(), Fail> {
    let state = state_dir(ctx, p)?;
    let owner = session_name(s.id);
    if world_vm::sandbox_record(&state, &owner).map_err(|e| err(e.to_string()))?.is_none() {
        println!("session {} has no sandbox VM; nothing to destroy", hex(s.id));
        return Ok(());
    }
    println!("destroy the sandbox VM of session {}: power it off and delete its disks", hex(s.id));
    if !confirm(yes)? {
        println!("nothing was destroyed");
        return Ok(());
    }
    let (_, entries) = branch(&s.path)?;
    let w = writer(ctx, dir, s, &entries, "the sandbox VM was not destroyed").await?;
    let removed = world_vm::destroy_machine(&state, &owner, world_vm::SANDBOX_NAME);
    let removed = match removed {
        Ok(r) => r,
        Err(e) => {
            w.finish(false).await.map_err(err)?;
            return Err(err(format!("the sandbox VM could not be destroyed: {e}")));
        }
    };
    let appended =
        w.append(EntryKind::Custom { ns: "Sandbox".into(), name: "Destroyed".into(), body: json!({"world": "vm", "owner": owner}) }).await;
    let via = w.finish(false).await.map_err(err)?;
    appended.map_err(|e| err(format!("the sandbox VM was destroyed but it could not be recorded: {e}")))?;
    if removed {
        let via = via.map(|v| format!(" {v}")).unwrap_or_default();
        println!("destroyed the sandbox VM of session {} (by Operator{via}); the next turn creates a fresh one", hex(s.id));
    }
    Ok(())
}

/// The sandbox VM's files, through its guest agent.
pub fn sandbox_fs(ctx: &Ctx, p: &aho_compose::Planned, s: &SessionFile) -> Result<std::sync::Arc<dyn aho_fs::Fs>, Fail> {
    let guest = world_vm::sandbox_guest(&state_dir(ctx, p)?, &session_name(s.id)).map_err(|e| err(e.to_string()))?;
    Ok(guest.fs())
}

pub fn find(ctx: &Ctx, p: &aho_compose::Planned, session: Option<&str>) -> Result<(PathBuf, SessionFile), Fail> {
    let dir = ctx.session_dir(p);
    let s = find_session(&dir, session)?;
    Ok((dir, s))
}

#[cfg(test)]
mod tests {
    #[test]
    fn durations_take_s_m_h_d_and_refuse_zero_or_garbage() {
        assert_eq!(super::duration_ms("6h"), Some(6 * 3_600_000));
        assert_eq!(super::duration_ms("30m"), Some(1_800_000));
        assert_eq!(super::duration_ms("2d"), Some(172_800_000));
        assert_eq!(super::duration_ms("4"), Some(4 * 3_600_000), "a bare number is hours");
        for bad in ["0h", "", "h", "5w", "-1h"] {
            assert_eq!(super::duration_ms(bad), None, "{bad}");
        }
    }
}
