//! Immutable build store + channel symlinks.
//!
//! Layout under `root`:
//! * `versions/<hash>/aho`: one published binary per content hash, file and directory `0o555`;
//! * `current`, `stable`, `canary`: relative symlinks to `versions/<hash>`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aho_types::{Hash, Value};
use serde::{Deserialize, Serialize};

/// Every smoke `detail` fits in this many bytes.
pub const DETAIL_MAX: usize = 2048;
/// The readiness fd variable of the daemon handshake; a smoke boot must never inherit it.
const READY_FD_ENV: &str = "AHO_READY_FD";
const READ_ONLY: u32 = 0o555;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Current,
    Stable,
    Canary,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::Current, Channel::Stable, Channel::Canary];

    pub fn name(self) -> &'static str {
        match self {
            Channel::Current => "current",
            Channel::Stable => "stable",
            Channel::Canary => "canary",
        }
    }

    pub fn parse(s: &str) -> Option<Channel> {
        Channel::ALL.into_iter().find(|c| c.name() == s)
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Published {
    pub hash: String,
    /// `root/versions/<hash>/aho`.
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Smoke {
    pub ok: bool,
    /// At most [`DETAIL_MAX`] bytes: what ran and, on failure, why (stderr tail).
    pub detail: String,
}

/// The changes `activate` made, enough to undo them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Activation {
    pub hash: String,
    pub previous: BTreeMap<Channel, Option<String>>,
}

#[derive(Debug)]
pub enum SelfdevError {
    Io {
        doing: String,
        path: PathBuf,
        error: io::Error,
    },
    /// A published build no longer hashes to its name.
    Tampered {
        hash: String,
        path: PathBuf,
        actual: String,
    },
    NotPublished {
        hash: String,
        path: PathBuf,
    },
    /// Flipping one channel failed; the channels flipped before it were put back.
    Flip {
        hash: String,
        channel: Channel,
        error: io::Error,
        restore: Option<String>,
    },
    SmokeFailed {
        hash: String,
        detail: String,
    },
    Log(String),
    HandoffFailed {
        hash: String,
        reason: String,
        rollback: Option<String>,
    },
}

fn short(h: &str) -> &str {
    h.get(..12).unwrap_or(h)
}

impl std::fmt::Display for SelfdevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelfdevError::Io { doing, path, error } => write!(
                f,
                "could not {doing} `{}` ({error}); we stopped at that step. Check the permissions of the builds directory (`selfdev.builds_dir`) and run `aho selfdev reload` again.",
                path.display()
            ),
            SelfdevError::Tampered { hash, path, actual } => write!(
                f,
                "build {} at `{}` no longer matches its hash (it now hashes to {}); an immutable build was modified. We left it as is and published nothing. Remove that version directory (`chmod -R u+w` first) and publish again.",
                short(hash),
                path.display(),
                short(actual)
            ),
            SelfdevError::NotPublished { hash, path } => write!(
                f,
                "build `{}` is not published (no `{}`); no channel was changed. Publish it first with `aho selfdev reload`.",
                short(hash),
                path.display()
            ),
            SelfdevError::Flip { hash, channel, error, restore } => {
                write!(f, "could not point `{channel}` at build {} ({error}); ", short(hash))?;
                match restore {
                    None => write!(f, "we put back the channels flipped before it.")?,
                    Some(r) => write!(f, "putting back the channels flipped before it also failed ({r}); repoint them by hand.")?,
                }
                write!(f, " Check the permissions of the builds directory and retry.")
            }
            SelfdevError::SmokeFailed { hash, detail } => write!(
                f,
                "the new build {} failed its smoke boot: {detail}. We did not activate it and no channel was changed; fix the build and run `aho selfdev reload` again.",
                short(hash)
            ),
            SelfdevError::Log(e) => write!(
                f,
                "the session log refused a Selfdev entry ({e}); we stopped at that step. `aho session verify` shows the log's state."
            ),
            SelfdevError::HandoffFailed { hash, reason, rollback } => {
                write!(f, "the new build {} did not take over ({reason}); ", short(hash))?;
                match rollback {
                    None => write!(f, "we restored the previous `current` and `canary` links and logged the rollback.")?,
                    Some(r) => write!(f, "restoring the previous links also failed ({r}); repoint `current` and `canary` by hand.")?,
                }
                write!(f, " Fix the build and run `aho selfdev reload` again.")
            }
        }
    }
}

impl std::error::Error for SelfdevError {}

fn io_err(doing: &str, path: &Path) -> impl FnOnce(io::Error) -> SelfdevError {
    let (doing, path) = (doing.to_string(), path.to_path_buf());
    move |error| SelfdevError::Io { doing, path, error }
}

fn is_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[allow(clippy::disallowed_methods, reason = "authority: selfdev-remove-channel")]
fn remove_if_present(p: &Path) -> io::Result<()> {
    match fs::remove_file(p) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[derive(Clone, Debug)]
pub struct Builds {
    root: PathBuf,
}

impl Builds {
    pub fn new(root: impl Into<PathBuf>) -> Builds {
        Builds { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn versions(&self) -> PathBuf {
        self.root.join("versions")
    }

    /// `root/versions/<hash>/aho`.
    pub fn binary(&self, hash: &str) -> PathBuf {
        self.versions().join(hash).join("aho")
    }

    /// Copies `binary` into `versions/<hash>/aho`, read-only. Publishing the same bytes again
    /// returns the same path after re-hashing what is on disk.
    #[allow(clippy::disallowed_methods, reason = "authority: selfdev-publish")]
    pub fn publish(&self, binary: &Path) -> Result<Published, SelfdevError> {
        let bytes = fs::read(binary).map_err(io_err("read the binary to publish", binary))?;
        let hash = Hash::of(&bytes).to_hex();
        let versions = self.versions();
        let dir = versions.join(&hash);
        let path = dir.join("aho");
        if dir.exists() {
            self.verify(&hash)?;
            return Ok(Published { hash, path });
        }
        fs::create_dir_all(&versions).map_err(io_err("create the versions directory", &versions))?;
        let tmp = versions.join(format!(".tmp-{hash}-{}", std::process::id()));
        if tmp.exists() {
            fs::remove_dir_all(&tmp).map_err(io_err("clear a leftover staging directory", &tmp))?;
        }
        fs::create_dir(&tmp).map_err(io_err("create the staging directory", &tmp))?;
        let staged = tmp.join("aho");
        let stage = fs::write(&staged, &bytes).map_err(io_err("write the staged binary", &staged)).and_then(|()| {
            fs::set_permissions(&staged, fs::Permissions::from_mode(READ_ONLY)).map_err(io_err("make the staged binary read-only", &staged))
        });
        if let Err(e) = stage {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        if let Err(error) = fs::rename(&tmp, &dir) {
            let _ = fs::remove_dir_all(&tmp);
            if !dir.exists() {
                return Err(SelfdevError::Io { doing: "move the staged build into place at".into(), path: dir, error });
            }
            // Another publisher of the same bytes won the rename; theirs must hash the same.
            self.verify(&hash)?;
            return Ok(Published { hash, path });
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(READ_ONLY)).map_err(io_err("make the version directory read-only", &dir))?;
        Ok(Published { hash, path })
    }

    #[allow(clippy::disallowed_methods, reason = "authority: selfdev-verify-build")]
    fn verify(&self, hash: &str) -> Result<(), SelfdevError> {
        let path = self.binary(hash);
        let bytes = fs::read(&path).map_err(io_err("re-read the published build", &path))?;
        let actual = Hash::of(&bytes).to_hex();
        if actual != hash {
            return Err(SelfdevError::Tampered { hash: hash.to_string(), path, actual });
        }
        Ok(())
    }

    /// Runs `<bin> plan --json` in a fresh project with a fresh `HOME`; ok iff it exits 0 and
    /// prints JSON within `timeout`. On timeout the process group is killed.
    pub fn smoke(&self, p: &Published, timeout: Duration) -> Smoke {
        run_smoke(&p.path, timeout).unwrap_or_else(|detail| Smoke { ok: false, detail: clip(detail, "") })
    }

    /// The build a channel points at, if the link exists.
    #[allow(clippy::disallowed_methods, reason = "authority: selfdev-read-channel")]
    pub fn channel(&self, c: Channel) -> Option<String> {
        let target = fs::read_link(self.root.join(c.name())).ok()?;
        target.file_name().map(|n| n.to_string_lossy().into_owned())
    }

    #[allow(clippy::disallowed_methods, reason = "authority: selfdev-point-channel")]
    fn point(&self, c: Channel, hash: Option<&str>) -> io::Result<()> {
        let link = self.root.join(c.name());
        let Some(hash) = hash else { return remove_if_present(&link) };
        let tmp = self.root.join(format!(".{}.tmp", c.name()));
        remove_if_present(&tmp)?;
        std::os::unix::fs::symlink(Path::new("versions").join(hash), &tmp)?;
        fs::rename(&tmp, &link)
    }

    /// Points every channel at `hash`, each flip atomic (`.<name>.tmp` symlink renamed over).
    #[allow(clippy::disallowed_methods, reason = "authority: selfdev-activate")]
    pub fn activate(&self, hash: &str, channels: &[Channel]) -> Result<Activation, SelfdevError> {
        let path = self.binary(hash);
        if !is_hash(hash) || !path.is_file() {
            return Err(SelfdevError::NotPublished { hash: hash.to_string(), path });
        }
        let previous: BTreeMap<Channel, Option<String>> = channels.iter().map(|c| (*c, self.channel(*c))).collect();
        for (i, c) in channels.iter().enumerate() {
            if let Err(error) = self.point(*c, Some(hash)) {
                let mut restore = None;
                for done in channels[..i].iter().rev() {
                    let prev = previous.get(done).cloned().flatten();
                    if let Err(e) = self.point(*done, prev.as_deref()) {
                        restore.get_or_insert_with(|| format!("`{done}`: {e}"));
                    }
                }
                return Err(SelfdevError::Flip { hash: hash.to_string(), channel: *c, error, restore });
            }
        }
        Ok(Activation { hash: hash.to_string(), previous })
    }

    /// Restores every channel `a` changed to its previous target (removing links that did not exist).
    pub fn rollback(&self, a: &Activation) -> Result<(), SelfdevError> {
        let mut first = None;
        for (c, prev) in &a.previous {
            if let Err(e) = self.point(*c, prev.as_deref()) {
                first.get_or_insert(SelfdevError::Io {
                    doing: format!("restore the `{c}` link"),
                    path: self.root.join(c.name()),
                    error: e,
                });
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// `stable` → `hash`.
    pub fn promote(&self, hash: &str) -> Result<Activation, SelfdevError> {
        self.activate(hash, &[Channel::Stable])
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: selfdev-smoke")]
fn run_smoke(bin: &Path, timeout: Duration) -> Result<Smoke, String> {
    let scratch = tempfile::Builder::new()
        .prefix("aho-smoke-")
        .tempdir()
        .map_err(|e| format!("could not create a scratch project for the smoke boot ({e})"))?;
    let (project, home) = (scratch.path().join("project"), scratch.path().join("home"));
    for d in [&project, &home] {
        fs::create_dir(d).map_err(|e| format!("could not create `{}` for the smoke boot ({e})", d.display()))?;
    }
    let (out_path, err_path) = (scratch.path().join("stdout"), scratch.path().join("stderr"));
    let open = |p: &Path| fs::File::create(p).map_err(|e| format!("could not create `{}` for the smoke boot ({e})", p.display()));
    let (out, err) = (open(&out_path)?, open(&err_path)?);
    let started = Instant::now();
    let mut child = Command::new(bin)
        .args(["plan", "--json"])
        .current_dir(&project)
        .env("HOME", &home)
        .env_remove(READY_FD_ENV)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not start `{}` ({e})", bin.display()))?;
    let stderr_tail = || fs::read(&err_path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => return Err(format!("lost track of the smoke process ({e})")),
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            let head = format!("`plan --json` did not finish within {} ms, so we killed it", timeout.as_millis());
            return Ok(Smoke { ok: false, detail: clip(head, &stderr_tail()) });
        }
        std::thread::sleep((timeout - elapsed).min(Duration::from_millis(5)));
    };
    let ms = started.elapsed().as_millis();
    if !status.success() {
        return Ok(Smoke { ok: false, detail: clip(format!("`plan --json` exited with {status}"), &stderr_tail()) });
    }
    let stdout = fs::read(&out_path).map_err(|e| format!("could not read the smoke output ({e})"))?;
    match serde_json::from_slice::<Value>(&stdout) {
        Ok(_) => Ok(Smoke { ok: true, detail: format!("`plan --json` exited 0 with JSON output in {ms} ms") }),
        Err(e) => {
            let head = format!("`plan --json` exited 0 but its stdout is not JSON ({e})");
            Ok(Smoke { ok: false, detail: clip(head, &String::from_utf8_lossy(&stdout)) })
        }
    }
}

/// Grandchildren of the smoke binary share its process group (`process_group(0)`).
#[allow(clippy::disallowed_methods, reason = "authority: selfdev-kill-smoke")]
fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-s", "KILL", "--", &format!("-{pid}")])
        .env_remove(READY_FD_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `head` plus as much of the end of `tail` as fits in [`DETAIL_MAX`] bytes.
fn clip(mut head: String, tail: &str) -> String {
    if head.len() > DETAIL_MAX {
        let mut cut = DETAIL_MAX;
        while !head.is_char_boundary(cut) {
            cut -= 1;
        }
        head.truncate(cut);
        return head;
    }
    let tail = tail.trim();
    if tail.is_empty() {
        return head;
    }
    let room = DETAIL_MAX.saturating_sub(head.len() + 2);
    if room == 0 {
        return head;
    }
    let mut start = tail.len().saturating_sub(room);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    head.push_str(": ");
    head.push_str(&tail[start..]);
    head
}
