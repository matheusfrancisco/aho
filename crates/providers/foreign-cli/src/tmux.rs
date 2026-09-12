//! The PTY host: a dedicated tmux server (`tmux -L <socket>`) with one session per child.
//!
//! tmux 3.5a reports a missing target on stderr and still exits 0, so presence is always read
//! from `list-sessions`, and any stderr from a command that should be silent is an error.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// One tmux command may take this long before it is abandoned (and killed).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// What `list-sessions` says about one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionState {
    pub name: String,
    /// Unix seconds; with the name, identifies this session across a crash.
    pub created: u64,
    pub pane_pid: u32,
    pub dead: bool,
    pub dead_status: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct Tmux {
    bin: PathBuf,
    socket: String,
}

fn no_server(stderr: &str) -> bool {
    stderr.contains("no server running") || stderr.contains("error connecting to") || stderr.contains("No such file or directory")
}

impl Tmux {
    pub fn new(bin: impl Into<PathBuf>, socket: impl Into<String>) -> Tmux {
        Tmux { bin: bin.into(), socket: socket.into() }
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// `-L <name>` for a socket name, `-S <path>` when the socket contains a `/`.
    fn socket_flag(&self) -> &'static str {
        if self.socket.contains('/') { "-S" } else { "-L" }
    }

    /// The command an operator runs to watch a child live.
    pub fn attach_hint(&self, session: &str) -> String {
        format!("tmux {} {} attach -t {session}", self.socket_flag(), self.socket)
    }

    #[allow(clippy::disallowed_methods, reason = "authority: foreign-cli-tmux")]
    async fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Result<(bool, String, String), String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(self.socket_flag()).arg(&self.socket).args(args);
        cmd.env_remove("AHO_READY_FD"); // the daemon's readiness fd (daemon::ready::READY_FD_ENV) is never a child's to write (12)
        cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("could not run {} ({e})", self.bin.display()))?;
        if let (Some(bytes), Some(mut w)) = (stdin, child.stdin.take()) {
            w.write_all(bytes).await.map_err(|e| format!("could not write to tmux ({e})"))?;
            drop(w);
        }
        let out = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| format!("tmux did not answer within {} s", COMMAND_TIMEOUT.as_secs()))?
            .map_err(|e| format!("tmux failed ({e})"))?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }

    /// Runs a command that prints nothing on success.
    async fn quiet(&self, args: &[String], stdin: Option<&[u8]>) -> Result<(), String> {
        let (ok, _, err) = self.run(args, stdin).await?;
        if !ok || !err.is_empty() {
            return Err(if err.is_empty() { "tmux exited with an error".into() } else { format!("tmux: {err}") });
        }
        Ok(())
    }

    fn pane(session: &str) -> String {
        format!("={session}:")
    }

    /// Starts `argv` in a new detached session `name` with working directory `cwd`. The
    /// server keeps dead panes (`remain-on-exit`) so an exit status can be read afterwards.
    pub async fn launch(&self, name: &str, cwd: &Path, env: &[(String, String)], argv: &[String]) -> Result<(), String> {
        let mut args: Vec<String> =
            ["start-server", ";", "set-option", "-g", "remain-on-exit", "on", ";", "set-option", "-g", "history-limit", "5000", ";"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        args.extend(["new-session", "-d", "-s", name, "-x", "200", "-y", "50", "-c"].iter().map(|s| s.to_string()));
        args.push(cwd.display().to_string());
        for (k, v) in env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push("--".into());
        args.extend(argv.iter().cloned());
        self.quiet(&args, None).await
    }

    /// Every session on this server; empty when the server is not running.
    pub async fn sessions(&self) -> Result<Vec<SessionState>, String> {
        let fmt = "#{session_name}\t#{session_created}\t#{pane_pid}\t#{pane_dead}\t#{pane_dead_status}";
        let (ok, out, err) = self.run(&["list-sessions".into(), "-F".into(), fmt.into()], None).await?;
        if !ok || !err.is_empty() {
            if no_server(&err) {
                return Ok(Vec::new());
            }
            return Err(format!("tmux could not list sessions: {err}"));
        }
        Ok(out
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                if f.len() < 5 {
                    return None;
                }
                Some(SessionState {
                    name: f[0].to_string(),
                    created: f[1].parse().unwrap_or(0),
                    pane_pid: f[2].parse().unwrap_or(0),
                    dead: f[3] == "1",
                    dead_status: f[4].parse().ok(),
                })
            })
            .collect())
    }

    pub async fn session(&self, name: &str) -> Result<Option<SessionState>, String> {
        Ok(self.sessions().await?.into_iter().find(|s| s.name == name))
    }

    /// The visible screen, wrapped lines joined, trailing blank lines dropped.
    pub async fn capture(&self, name: &str) -> Result<String, String> {
        let (ok, out, err) = self.run(&["capture-pane".into(), "-p".into(), "-J".into(), "-t".into(), Self::pane(name)], None).await?;
        if !ok || !err.is_empty() {
            return Err(format!("tmux could not read the screen of {name}: {err}"));
        }
        Ok(out.trim_end().to_string())
    }

    /// Types `text` into the pane as one paste (bracketed when the program asked for it), then
    /// presses Enter.
    pub async fn submit(&self, name: &str, text: &str) -> Result<(), String> {
        let buffer = format!("aho-{name}");
        self.quiet(&["load-buffer".into(), "-b".into(), buffer.clone(), "-".into()], Some(text.as_bytes())).await?;
        self.quiet(&["paste-buffer".into(), "-p".into(), "-d".into(), "-b".into(), buffer, "-t".into(), Self::pane(name)], None).await?;
        self.keys(name, &["Enter"]).await
    }

    /// Presses keys by tmux name (`Escape`, `Enter`, `C-c`).
    pub async fn keys(&self, name: &str, keys: &[&str]) -> Result<(), String> {
        let mut args = vec!["send-keys".to_string(), "-t".into(), Self::pane(name)];
        args.extend(keys.iter().map(|k| k.to_string()));
        self.quiet(&args, None).await
    }

    /// Ends the session (and the program in it). A session that is already gone is fine.
    pub async fn kill(&self, name: &str) -> Result<(), String> {
        let (_, _, err) = self.run(&["kill-session".into(), "-t".into(), format!("={name}")], None).await?;
        if err.is_empty() || err.contains("can't find session") || no_server(&err) {
            return Ok(());
        }
        Err(format!("tmux could not end session {name}: {err}"))
    }
}
