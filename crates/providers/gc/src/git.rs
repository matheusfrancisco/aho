//! `git`, shelled out as world-host does, with the caller's repository environment removed so a
//! `GIT_DIR` in the operator's shell cannot redirect a command to another repository.

use std::ffi::OsStr;
use std::process::{Command, Output};

const REPO_ENV: &[&str] =
    &["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_CEILING_DIRECTORIES"];

#[allow(clippy::disallowed_methods, reason = "authority: gc-git-command")]
pub(crate) fn command<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut c = Command::new("git");
    for k in REPO_ENV {
        c.env_remove(k);
    }
    c.args(args);
    c
}

/// Runs to completion; a non-zero exit is an error carrying git's own message.
#[allow(clippy::disallowed_methods, reason = "authority: gc-git-run")]
pub(crate) fn run<I, S>(args: I, what: &str) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = command(args).output().map_err(|e| format!("could not run git to {what} ({e}); install git or put it on PATH"))?;
    if !out.status.success() {
        return Err(format!("git could not {what}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out)
}
