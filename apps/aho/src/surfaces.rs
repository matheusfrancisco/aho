//! The long-lived surfaces over one session: the terminal UI, the web UI (HTTPS, or plain
//! HTTP on loopback for development), the GitHub webhook listener, and the Agent Client Protocol
//! server. `aho cert` writes the local TLS pair the web and webhook surfaces serve with.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aho_types::BoxFuture;
use surface_acp::{AcpOptions, Opened as AcpOpened, SessionFactory, SessionRequest};

use crate::boot::stop_drivers;
use crate::ctx::{Ctx, Fail, err, hex};
use crate::session::open;

/// The directory `aho cert` writes, and `aho web` / `aho webhook` read when no pair is passed.
fn tls_dir(home: &Path) -> PathBuf {
    home.join(".aho").join("tls")
}

fn default_pair(home: &Path) -> Option<(PathBuf, PathBuf)> {
    let d = tls_dir(home);
    let (c, k) = (d.join("cert.pem"), d.join("key.pem"));
    (c.is_file() && k.is_file()).then_some((c, k))
}

fn read_pem(p: &Path, what: &str) -> Result<String, Fail> {
    std::fs::read_to_string(p).map_err(|e| err(format!("the TLS {what} {} cannot be read ({e}); nothing was served", p.display())))
}

fn resumed_note(id: aho_types::SessionId, resumed: bool) {
    eprintln!("session {} ({})", hex(id), if resumed { "resumed" } else { "new" });
}

pub async fn tui(ctx: &Ctx, session: Option<&str>, headless: bool, fullscreen: bool, width: u16, height: u16) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    if headless {
        // "Visual debug": the normalized screen of the session as it is now; nothing runs.
        // The drivers are hosted for the boot so the schedule/workflow tools are not logged Broken.
        let o = open(ctx, &p, &inputs.home, session, false, true).await?;
        let frame = surface_tui::headless(&o.session, &p, width, height).map_err(err);
        stop_drivers(o.drivers).await;
        println!("{}", serde_json::to_string_pretty(&frame?)?);
        return Ok(());
    }
    let commands = crate::slash::commands().map_err(err)?;
    let o = open(ctx, &p, &inputs.home, session, true, true).await?;
    let approvals = o.approvals.ok_or_else(|| err("the session has no approval channel, so the terminal UI cannot answer cards"))?;
    // Colours from the terminal: NO_COLOR, COLORTERM/TERM depth, COLORFGBG or AHO_THEME background.
    let options = surface_tui::RunOptions { fullscreen, theme: surface_tui::Theme::detect(&|k| std::env::var(k).ok()) };
    let ran = surface_tui::run(o.session.clone(), approvals, p, commands, options).await;
    stop_drivers(o.drivers).await;
    ran.map_err(err)?;
    println!("session {} · {}  (resume: aho tui --session {})", hex(o.id), o.path.display(), &hex(o.id)[..12.min(hex(o.id).len())]);
    Ok(())
}

pub async fn web(
    ctx: &Ctx,
    session: Option<&str>,
    bind: &str,
    cert: Option<&Path>,
    key: Option<&Path>,
    insecure_loopback: bool,
) -> Result<(), Fail> {
    let addr: SocketAddr =
        bind.parse().map_err(|e| err(format!("`{bind}` is not an address to listen on ({e}); use e.g. 127.0.0.1:8787")))?;
    let inputs = ctx.inputs(&[], false)?;
    let tls = match (cert, key) {
        (Some(c), Some(k)) => Some((read_pem(&ctx.typed_path(c), "certificate")?, read_pem(&ctx.typed_path(k), "key")?)),
        _ if insecure_loopback => {
            if !addr.ip().is_loopback() {
                return Err(err(format!(
                    "--insecure-loopback serves plain HTTP, which is only allowed on a loopback address, and {addr} is not one. Bind 127.0.0.1, or serve HTTPS with `aho cert`"
                )));
            }
            None
        }
        _ => match default_pair(&inputs.home) {
            Some((c, k)) => Some((read_pem(&c, "certificate")?, read_pem(&k, "key")?)),
            None => {
                return Err(err(format!(
                    "the web UI serves HTTPS only and no certificate was given, so nothing was started. Run `aho cert` (writes {}), pass --cert/--key, or use --insecure-loopback for development on 127.0.0.1",
                    tls_dir(&inputs.home).display()
                )));
            }
        },
    };
    let p = ctx.load(&inputs)?;
    let commands = crate::slash::commands().map_err(err)?;
    let o = open(ctx, &p, &inputs.home, session, true, true).await?;
    resumed_note(o.id, o.resumed);
    let approvals = o.approvals.ok_or_else(|| err("the session has no approval channel, so the web UI cannot answer cards"))?;
    let served = tokio::select! {
        r = surface_web::serve(o.session.clone(), approvals, p, commands, addr, tls) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    stop_drivers(o.drivers).await;
    served.map_err(err)
}

/// `aho cert`: a self-signed pair for localhost with `openssl`, key readable by you only.
pub fn cert(ctx: &Ctx, hosts: &[String], force: bool) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let dir = tls_dir(&inputs.home);
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    if !force && (cert.exists() || key.exists()) {
        return Err(err(format!("{} already holds a TLS pair, so nothing was written. Pass --force to replace it", dir.display())));
    }
    let mut san = vec!["DNS:localhost".to_string(), "IP:127.0.0.1".into(), "IP:::1".into()];
    for h in hosts {
        let h = h.trim();
        if h.is_empty() || h.contains([',', '/', ' ']) {
            return Err(err(format!("`{h}` is not a host name or IP address; nothing was written")));
        }
        san.push(if h.parse::<std::net::IpAddr>().is_ok() { format!("IP:{h}") } else { format!("DNS:{h}") });
    }
    std::fs::create_dir_all(&dir)?;
    let tmp_key = dir.join(".key.pem.tmp");
    let tmp_cert = dir.join(".cert.pem.tmp");
    let out = std::process::Command::new("openssl")
        .args(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-days", "825", "-subj", "/CN=localhost"])
        .arg("-addext")
        .arg(format!("subjectAltName={}", san.join(",")))
        .arg("-keyout")
        .arg(&tmp_key)
        .arg("-out")
        .arg(&tmp_cert)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| err(format!("could not run openssl ({e}), so no certificate was made. Install it (`brew install openssl`), or pass your own pair to `aho web --cert --key`")))?;
    if !out.status.success() {
        return Err(err(format!("openssl could not make the certificate: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    std::fs::set_permissions(&tmp_key, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    std::fs::rename(&tmp_key, &key)?;
    std::fs::rename(&tmp_cert, &cert)?;
    println!("wrote {} and {} (self-signed, valid 825 days, for {})", cert.display(), key.display(), san.join(" "));
    println!("`aho web` and `aho webhook` now serve HTTPS with it. Browsers warn until you trust it, e.g. on macOS:");
    println!("  security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db {}", cert.display());
    Ok(())
}

pub async fn webhook(ctx: &Ctx, session: Option<&str>, insecure_loopback: bool) -> Result<(), Fail> {
    let inputs = ctx.inputs(&[], false)?;
    let p = ctx.load(&inputs)?;
    if let Some(row) = p.plan.row("surface-webhook").filter(|r| !matches!(r.state, aho_log::CapState::Active)) {
        let fixes: Vec<String> = row.missing.iter().filter_map(|m| m.fix.as_ref().map(|f| format!("`{f}`"))).collect();
        return Err(Fail::Plan(format!(
            "the webhook surface is {}, so nothing was started. Fix: {}",
            aho_inspect::state_name(&row.state),
            if fixes.is_empty() { "`aho why surface-webhook`".to_string() } else { fixes.join(" · ") }
        )));
    }
    let mut opts = <surface_webhook::WebhookOptions as aho_cap::Config>::from_loadout(&p.loadout).map_err(|e| {
        err(format!("the webhook settings do not resolve ({e:?}); `aho explain surface.webhook.secret` shows where they come from"))
    })?;
    if insecure_loopback {
        opts.transport = Some(surface_webhook::Transport::InsecureLoopback);
    } else if opts.transport.is_none()
        && let Some((cert, key)) = default_pair(&inputs.home)
    {
        opts.transport = Some(surface_webhook::Transport::Tls { cert, key });
    }
    let which = session.map(str::to_string).or_else(|| opts.session.clone());
    // Headless: no one answers cards here, so calls that would ask are refused.
    let o = open(ctx, &p, &inputs.home, which.as_deref(), false, true).await?;
    resumed_note(o.id, o.resumed);
    let sink = Arc::new(surface_webhook::SessionSink::new(o.session.clone()));
    let served = tokio::select! {
        r = surface_webhook::serve(opts, sink) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    stop_drivers(o.drivers).await;
    served.map_err(err)
}

/// Boots one aho session per ACP `session/new` (or resumes one on `session/load`).
struct AcpHost {
    ctx: Ctx,
    drivers: Mutex<Vec<session_drive::Drivers>>,
}

impl SessionFactory for AcpHost {
    fn open(&self, req: SessionRequest) -> BoxFuture<'_, Result<AcpOpened, String>> {
        Box::pin(async move {
            let sets = surface_acp::config_sets(&req)?;
            let inputs = self.ctx.inputs(&sets, false).map_err(|e| e.to_string())?;
            let p = self.ctx.load(&inputs).map_err(|e| e.to_string())?;
            let o = open(&self.ctx, &p, &inputs.home, req.load.as_deref(), true, true).await.map_err(|e| e.to_string())?;
            if let Some(d) = o.drivers {
                self.drivers.lock().unwrap_or_else(|x| x.into_inner()).push(d);
            }
            Ok(AcpOpened { session: o.session, approvals: o.approvals })
        })
    }

    fn supports_load(&self) -> bool {
        true
    }
}

/// `aho acp`: stdout is the protocol; everything else goes to stderr.
pub async fn acp(ctx: Ctx) -> Result<(), Fail> {
    let host = Arc::new(AcpHost { ctx, drivers: Mutex::default() });
    let served = surface_acp::serve_stdio(host.clone(), AcpOptions::default()).await;
    let drivers = std::mem::take(&mut *host.drivers.lock().unwrap_or_else(|x| x.into_inner()));
    for d in drivers {
        stop_drivers(Some(d)).await;
    }
    served.map_err(|e| err(e.to_string()))
}
