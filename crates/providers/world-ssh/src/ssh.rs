//! The transport: the system `ssh` client, multiplexed over one ControlMaster per world.
//!
//! Every operation is one `ssh … host -- <command>` run over the master's socket (a new session
//! on an existing connection, a few ms). The remote command is always
//! `exec /bin/sh -c '<script>' aho-ssh '<arg>'…` with every piece single-quoted by [`quote`], so
//! the remote login shell (sh, bash, zsh, dash, ksh, fish) only ever sees quoted words. Data goes
//! over stdin/stdout, which are binary-clean because no pty is requested (`-T`).
//!
//! Host keys: aho passes `BatchMode=yes` and never sets `StrictHostKeyChecking`, so an unknown or
//! changed host key fails the connection, which [`classify`] turns into a `Locked` error naming
//! `ssh-keyscan` / `ssh-keygen -R` and the effective known_hosts file.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use aho_types::Hash;
use tokio::io::AsyncWriteExt;

/// Seconds the master stays up after its last session; it is also closed when the world drops.
pub const CONTROL_PERSIST_S: u32 = 300;
/// macOS `sun_path` holds 103 bytes plus NUL, and ssh binds `<ControlPath>.<16 random chars>`
/// first, so the configured path must leave room for that suffix.
pub const MAX_CONTROL_PATH: usize = 103 - 17;
/// ssh's own exit status for "could not connect / authenticate"; no aho script exits with it.
pub const SSH_FAILED: i32 = 255;

/// The connection target. `host` may be an alias from the ssh config.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// Private key file (`-i`, with `IdentitiesOnly=yes`).
    pub identity: Option<PathBuf>,
    /// ssh_config file passed as `-F` instead of `~/.ssh/config`.
    pub config: Option<PathBuf>,
    /// `UserKnownHostsFile` override.
    pub known_hosts: Option<PathBuf>,
}

impl Target {
    /// `user@host:port` as the operator wrote it, for messages.
    pub fn display(&self) -> String {
        let mut s = String::new();
        if let Some(u) = &self.user {
            s.push_str(u);
            s.push('@');
        }
        s.push_str(&self.host);
        if let Some(p) = self.port {
            s.push_str(&format!(":{p}"));
        }
        s
    }

    fn validate(&self) -> Result<(), String> {
        let bad = |what: &str, v: &str| {
            format!(
                "world.ssh.{what} `{v}` is not usable (empty, starts with `-`, or contains spaces or control characters); nothing was connected. Set it to a plain host name, alias, or user name."
            )
        };
        let plain = |v: &str| !v.is_empty() && !v.starts_with('-') && !v.chars().any(|c| c.is_whitespace() || c.is_control());
        if !plain(&self.host) {
            return Err(bad("host", &self.host));
        }
        if let Some(u) = &self.user
            && !plain(u)
        {
            return Err(bad("user", u));
        }
        if self.port == Some(0) {
            return Err("world.ssh.port 0 is not a port; nothing was connected. Set 1–65535 or unset it.".into());
        }
        Ok(())
    }
}

/// The effective connection parameters `ssh -G` computes (config aliases resolved).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effective {
    pub hostname: String,
    pub port: u16,
    pub user: String,
    /// First `UserKnownHostsFile` entry.
    pub known_hosts: String,
}

impl Effective {
    fn keyscan(&self) -> String {
        format!("ssh-keyscan -p {} {} >> {}", self.port, self.hostname, self.known_hosts)
    }
    fn key_name(&self) -> String {
        if self.port == 22 { self.hostname.clone() } else { format!("[{}]:{}", self.hostname, self.port) }
    }
}

/// An ssh failure. `Locked` means nothing will work until the operator acts on `fix`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshError {
    Locked { what: String, fix: String },
    Transport(String),
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshError::Locked { what, fix } => write!(f, "Locked: {what}; nothing ran on the remote. Fix: {fix}"),
            SshError::Transport(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SshError {}

/// 32 hex chars, distinct per call (random per-instance hasher keys + clock); for temp names,
/// output boundaries and control-socket names.
pub fn nonce() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let seed = format!("{}:{nanos}:{}", std::process::id(), h.finish());
    Hash::of(seed.as_bytes()).to_hex()[..32].to_string()
}

/// Result of one remote command.
#[derive(Debug)]
pub struct Out {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// POSIX single-quoting that also survives fish: backslashes and quotes are emitted outside the
/// quotes as `\\` and `\'`, which every sh-family shell and fish read the same way.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./:@+,".contains(&b)) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    let mut open = false;
    for c in s.chars() {
        match c {
            '\'' | '\\' => {
                if open {
                    out.push('\'');
                    open = false;
                }
                out.push('\\');
                out.push(c);
            }
            c => {
                if !open {
                    out.push('\'');
                    open = true;
                }
                out.push(c);
            }
        }
    }
    if open {
        out.push('\'');
    }
    if out.is_empty() { "''".into() } else { out }
}

/// `exec /bin/sh -c '<script>' aho-ssh '<arg>'…`, the only shape of remote command aho sends.
pub fn sh_command(script: &str, args: &[&str]) -> String {
    let mut cmd = format!("exec /bin/sh -c {} aho-ssh", quote(script));
    for a in args {
        cmd.push(' ');
        cmd.push_str(&quote(a));
    }
    cmd
}

/// Turns ssh's own stderr (exit 255) into an operator-facing error.
pub fn classify(target: &Target, eff: &Effective, stderr: &str) -> SshError {
    let who = target.display();
    let detail = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" | ");
    if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return SshError::Locked {
            what: format!("the host key of {who} changed since it was recorded in {}, so ssh refused to connect", eff.known_hosts),
            fix: format!(
                "confirm the new key with whoever runs the host, then `ssh-keygen -R '{}' -f {}` and `{}`",
                eff.key_name(),
                eff.known_hosts,
                eff.keyscan()
            ),
        };
    }
    if stderr.contains("Host key verification failed") || stderr.contains("host key is known") {
        return SshError::Locked {
            what: format!(
                "the host key of {who} is not in {} and aho never turns StrictHostKeyChecking off, so ssh refused to connect",
                eff.known_hosts
            ),
            fix: format!(
                "`{}` after checking the printed fingerprint against the host (or connect once with `ssh -p {} {}@{}` and accept it)",
                eff.keyscan(),
                eff.port,
                eff.user,
                eff.hostname
            ),
        };
    }
    if stderr.contains("Permission denied") || stderr.contains("Too many authentication failures") {
        return SshError::Locked {
            what: format!("{who} refused the login as `{}` ({detail})", eff.user),
            fix: format!(
                "install your public key on the host (`ssh-copy-id -p {} {}@{}`) or set world.ssh.identity",
                eff.port, eff.user, eff.hostname
            ),
        };
    }
    SshError::Transport(format!(
        "ssh to {who} failed ({detail}); nothing ran on the remote. Check world.ssh.host / world.ssh.port and that `ssh -p {} {}@{} true` works.",
        eff.port, eff.user, eff.hostname
    ))
}

/// One ControlMaster per world; closed (`ssh -O exit`) when the last clone drops.
pub struct Ssh {
    target: Target,
    control: PathBuf,
    eff: Effective,
}

impl Ssh {
    /// Validates the target, creates `control_dir` (0700), picks a unique socket name in it and
    /// resolves the effective host/port/user with `ssh -G` (local, no network).
    pub fn new(target: Target, control_dir: &Path) -> Result<Ssh, String> {
        target.validate()?;
        create_private_dir(control_dir)?;
        let control = control_dir.join(&nonce()[..12]);
        let len = control.as_os_str().len();
        if len > MAX_CONTROL_PATH {
            return Err(format!(
                "the ssh control socket path {} is {len} bytes, over the {MAX_CONTROL_PATH} a unix socket allows here; nothing was connected. Set world.ssh.control_dir to a shorter directory (e.g. ~/.aho/ssh).",
                control.display()
            ));
        }
        let eff = effective(&target)?;
        Ok(Ssh { target, control, eff })
    }

    pub fn target(&self) -> &Target {
        &self.target
    }
    pub fn effective(&self) -> &Effective {
        &self.eff
    }
    pub fn control_path(&self) -> &Path {
        &self.control
    }

    fn options(&self, tty: bool) -> Vec<OsString> {
        let mut a: Vec<OsString> = Vec::new();
        if let Some(f) = &self.target.config {
            a.push("-F".into());
            a.push(f.into());
        }
        for o in [
            "BatchMode=yes".to_string(),
            "ControlMaster=auto".to_string(),
            format!("ControlPersist={CONTROL_PERSIST_S}"),
            "ServerAliveInterval=15".to_string(),
            "ServerAliveCountMax=4".to_string(),
            "ConnectTimeout=15".to_string(),
            "LogLevel=ERROR".to_string(),
        ] {
            a.push("-o".into());
            a.push(o.into());
        }
        let mut cp = OsString::from("ControlPath=");
        cp.push(&self.control);
        a.push("-o".into());
        a.push(cp);
        a.extend(target_args(&self.target));
        a.push(if tty { "-tt" } else { "-T" }.into());
        a
    }

    /// `ssh <options> -- <host> <remote>`, stdio unset.
    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-command")]
    pub fn command(&self, remote: &str) -> tokio::process::Command {
        let mut c = tokio::process::Command::new("ssh");
        c.args(self.options(false)).arg("--").arg(&self.target.host).arg(remote);
        c
    }

    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-std-command")]
    pub(crate) fn std_command(&self, remote: &str) -> std::process::Command {
        let mut c = std::process::Command::new("ssh");
        c.args(self.options(false)).arg("--").arg(&self.target.host).arg(remote);
        c
    }

    /// argv of an interactive `ssh -tt` running `remote` (for `aho shell`).
    pub fn tty_argv(&self, remote: &str) -> Vec<String> {
        let mut v = vec!["ssh".to_string()];
        v.extend(self.options(true).into_iter().map(|o| o.to_string_lossy().into_owned()));
        v.push("--".into());
        v.push(self.target.host.clone());
        v.push(remote.to_string());
        v
    }

    /// Runs `script` with `args` on the remote; stdin gets `input` (or `/dev/null`).
    /// `Err` only for ssh-level failures; a script's non-zero exit is `Ok(Out{code, …})`.
    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-run")]
    pub async fn run(&self, script: &str, args: &[&str], input: Option<Vec<u8>>) -> Result<Out, SshError> {
        let mut cmd = self.command(&sh_command(script, args));
        cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| self.spawn_error(e))?;
        let writer = match (input, child.stdin.take()) {
            (Some(bytes), Some(mut pipe)) => Some(tokio::spawn(async move {
                let r = pipe.write_all(&bytes).await;
                drop(pipe);
                r
            })),
            _ => None,
        };
        let out = child.wait_with_output().await.map_err(|e| SshError::Transport(format!("lost track of ssh: {e}")))?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let code = out.status.code().unwrap_or(SSH_FAILED);
        if code == SSH_FAILED {
            return Err(classify(&self.target, &self.eff, &stderr));
        }
        if let Some(w) = writer {
            match w.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(SshError::Transport(format!("could not send data to {}: {e}", self.target.display()))),
                Err(e) => return Err(SshError::Transport(format!("the ssh stdin writer stopped: {e}"))),
            }
        }
        Ok(Out { code, stdout: out.stdout, stderr })
    }

    /// Blocking variant for `Capability::build` (which is synchronous).
    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-run-blocking")]
    pub fn run_blocking(&self, script: &str, args: &[&str]) -> Result<Out, SshError> {
        let out = self.std_command(&sh_command(script, args)).stdin(Stdio::null()).output().map_err(|e| self.spawn_error(e))?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let code = out.status.code().unwrap_or(SSH_FAILED);
        if code == SSH_FAILED {
            return Err(classify(&self.target, &self.eff, &stderr));
        }
        Ok(Out { code, stdout: out.stdout, stderr })
    }

    fn spawn_error(&self, e: std::io::Error) -> SshError {
        if e.kind() == std::io::ErrorKind::NotFound {
            SshError::Locked {
                what: "the `ssh` client is not on PATH".into(),
                fix: "./install-system-deps.sh (installs openssh-client)".into(),
            }
        } else {
            SshError::Transport(format!("could not start ssh for {}: {e}; nothing ran on the remote.", self.target.display()))
        }
    }
}

impl Drop for Ssh {
    /// Stops the master so no connection outlives the world (`ControlPersist` covers crashes).
    #[allow(clippy::disallowed_methods, reason = "authority: world-ssh-close-master")]
    fn drop(&mut self) {
        let mut c = std::process::Command::new("ssh");
        if let Some(f) = &self.target.config {
            c.arg("-F").arg(f);
        }
        let mut cp = OsString::from("ControlPath=");
        cp.push(&self.control);
        let status = c
            .arg("-o")
            .arg(cp)
            .args(target_args(&self.target))
            .args(["-O", "exit", "--"])
            .arg(&self.target.host)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // No master running (never connected, or already gone) is the normal case here.
        drop(status);
    }
}

fn target_args(t: &Target) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    if let Some(kh) = &t.known_hosts {
        let mut o = OsString::from("UserKnownHostsFile=");
        o.push(kh);
        a.push("-o".into());
        a.push(o);
    }
    if let Some(p) = t.port {
        a.push("-p".into());
        a.push(p.to_string().into());
    }
    if let Some(u) = &t.user {
        a.push("-l".into());
        a.push(u.into());
    }
    if let Some(i) = &t.identity {
        a.push("-i".into());
        a.push(i.into());
        a.push("-o".into());
        a.push("IdentitiesOnly=yes".into());
    }
    a
}

/// `ssh -G`: the effective hostname/port/user/known_hosts after the ssh config is applied.
#[allow(clippy::disallowed_methods, reason = "authority: world-ssh-effective-config")]
fn effective(t: &Target) -> Result<Effective, String> {
    let mut c = std::process::Command::new("ssh");
    if let Some(f) = &t.config {
        c.arg("-F").arg(f);
    }
    let out = c.args(target_args(t)).arg("-G").arg("--").arg(&t.host).stdin(Stdio::null()).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "the `ssh` client is not on PATH, so the ssh world cannot start. Run ./install-system-deps.sh (openssh-client).".to_string()
        } else {
            format!("could not run `ssh -G` ({e}); nothing was connected.")
        }
    })?;
    if !out.status.success() {
        return Err(format!(
            "`ssh -G {}` rejected the ssh configuration: {}; nothing was connected. Fix world.ssh.config or the ssh config file.",
            t.host,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut e =
        Effective { hostname: t.host.clone(), port: t.port.unwrap_or(22), user: String::new(), known_hosts: "~/.ssh/known_hosts".into() };
    for line in text.lines() {
        let Some((k, v)) = line.split_once(' ') else { continue };
        match k {
            "hostname" => e.hostname = v.trim().to_string(),
            "port" => e.port = v.trim().parse().unwrap_or(e.port),
            "user" => e.user = v.trim().to_string(),
            "userknownhostsfile" => {
                if let Some(first) = v.split_whitespace().next() {
                    e.known_hosts = first.to_string();
                }
            }
            _ => {}
        }
    }
    Ok(e)
}

#[allow(clippy::disallowed_methods, reason = "authority: world-ssh-control-directory")]
fn create_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    let r = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    r.map_err(|e| {
        format!("could not create the ssh control directory {} ({e}); nothing was connected. Set world.ssh.control_dir.", dir.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The word `/bin/sh` reads from a quoted one.
    fn sh_echo(word: &str) -> String {
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(format!("printf %s {}", quote(word))).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn quoting_round_trips_through_sh() {
        for w in ["", "plain", "two words", "it's", "back\\slash", "a'b\\c'd", "$HOME `id` $(id)", "new\nline", "-n", "*", "~", "\\'", "'"]
        {
            assert_eq!(sh_echo(w), w, "{w:?} -> {}", quote(w));
        }
    }

    #[test]
    fn quoting_never_emits_backslashes_inside_quotes() {
        let q = quote("a\\b'c");
        assert_eq!(q, "'a'\\\\'b'\\''c'");
    }

    #[test]
    fn a_target_that_looks_like_an_option_is_refused() {
        for host in ["-oProxyCommand=x", "", "a b", "a\nb"] {
            let t = Target { host: host.into(), ..Target::default() };
            assert!(t.validate().is_err(), "{host:?}");
        }
        let t = Target { host: "h".into(), user: Some("-l".into()), ..Target::default() };
        assert!(t.validate().is_err());
        assert!(Target { host: "box.example".into(), user: Some("dev".into()), port: Some(2222), ..Target::default() }.validate().is_ok());
    }

    #[test]
    fn host_key_failures_are_locked_with_a_keyscan_fix() {
        let t = Target { host: "box".into(), port: Some(2222), ..Target::default() };
        let eff = Effective { hostname: "10.0.0.5".into(), port: 2222, user: "dev".into(), known_hosts: "/k/known_hosts".into() };
        match classify(
            &t,
            &eff,
            "No ED25519 host key is known for [10.0.0.5]:2222 and you have requested strict checking.\nHost key verification failed.\n",
        ) {
            SshError::Locked { what, fix } => {
                assert!(what.contains("not in /k/known_hosts") && what.contains("StrictHostKeyChecking"), "{what}");
                assert!(fix.contains("ssh-keyscan -p 2222 10.0.0.5 >> /k/known_hosts"), "{fix}");
            }
            other => panic!("{other:?}"),
        }
        match classify(&t, &eff, "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\nHost key verification failed.") {
            SshError::Locked { fix, .. } => assert!(fix.contains("ssh-keygen -R '[10.0.0.5]:2222'"), "{fix}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(classify(&t, &eff, "dev@10.0.0.5: Permission denied (publickey)."), SshError::Locked { .. }));
        assert!(matches!(classify(&t, &eff, "ssh: connect to host 10.0.0.5 port 2222: Connection refused"), SshError::Transport(_)));
    }

    #[test]
    fn an_overlong_control_directory_is_refused_with_the_key_to_change() {
        let base = tempfile::tempdir().unwrap();
        let deep = base.path().join("x".repeat(90));
        let t = Target { host: "h".into(), ..Target::default() };
        match Ssh::new(t, &deep) {
            Err(m) => assert!(m.contains("world.ssh.control_dir"), "{m}"),
            Ok(_) => panic!("accepted a control path longer than a unix socket allows"),
        }
    }
}
