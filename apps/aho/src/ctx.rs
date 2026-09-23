//! Shared plumbing for every verb: the failure type (exit codes), the composition, the operator's
//! inputs, session files, and the terminal helpers.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use aho_cap::Composition;
use aho_compose::{ComposeError, Inputs, Planned};
use aho_log::{Entry, IndexedLog, JsonlLog, LogStore, ProcessIdentity, SessionIndex};
use aho_types::*;
use surface_rpc::CapsError;

use crate::cli::Global;

/// `Error` exits 1, `Plan` exits 2 (plan or verify failure), `Reported` exits 1 silently because
/// the transcript already said what happened.
#[derive(Debug)]
pub enum Fail {
    Error(String),
    Plan(String),
    Reported,
}

impl Fail {
    pub fn code(&self) -> u8 {
        match self {
            Fail::Plan(_) => 2,
            Fail::Error(_) | Fail::Reported => 1,
        }
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Error(m) | Fail::Plan(m) => f.write_str(m),
            Fail::Reported => f.write_str("the transcript above says what happened"),
        }
    }
}

pub fn err(s: impl Into<String>) -> Fail {
    Fail::Error(s.into())
}

impl From<ComposeError> for Fail {
    fn from(e: ComposeError) -> Self {
        CapsError::Compose(e).into()
    }
}

impl From<CapsError> for Fail {
    fn from(e: CapsError) -> Self {
        if e.is_plan_failure() { Fail::Plan(e.to_string()) } else { Fail::Error(e.to_string()) }
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::Error(e.to_string())
    }
}

impl From<aho_session::SessionError> for Fail {
    fn from(e: aho_session::SessionError) -> Self {
        Fail::Error(e.to_string())
    }
}

impl From<aho_log::LogError> for Fail {
    fn from(e: aho_log::LogError) -> Self {
        Fail::Error(e.to_string())
    }
}

impl From<serde_json::Error> for Fail {
    fn from(e: serde_json::Error) -> Self {
        Fail::Error(format!("could not encode JSON: {e}"))
    }
}

/// The product this binary is.
pub fn composition() -> Composition {
    aho_lab::lab()
}

#[derive(Clone)]
pub struct Ctx {
    pub global: Global,
    /// Canonical project directory; also the process working directory from here on, so the host
    /// world's default `workspace.root` ("the directory aho was started in") is the project.
    pub project: PathBuf,
    /// Where aho was launched, for resolving relative paths the operator typed.
    pub launched_in: PathBuf,
    pub comp: Composition,
}

impl Ctx {
    pub fn new(global: Global) -> Result<Ctx, Fail> {
        let launched_in = std::env::current_dir().map_err(|e| err(format!("the current directory cannot be read ({e})")))?;
        let dir = match &global.project {
            Some(p) => launched_in.join(p),
            None => launched_in.clone(),
        };
        let project = std::fs::canonicalize(&dir)
            .map_err(|e| err(format!("project directory {} cannot be opened ({e}); pass --project <dir>", dir.display())))?;
        std::env::set_current_dir(&project).map_err(|e| err(format!("cannot enter {} ({e})", project.display())))?;
        Ok(Ctx { global, project, launched_in, comp: composition() })
    }

    /// A path the operator typed, relative to where aho was launched.
    pub fn typed_path(&self, p: &Path) -> PathBuf {
        self.launched_in.join(p)
    }

    pub fn inputs(&self, extra: &[(String, String)], strict: bool) -> Result<Inputs, Fail> {
        let mut sets = self.global.sets.clone();
        sets.extend(extra.iter().cloned());
        let mut i = Inputs::from_process(self.project.clone(), sets, self.global.disable.clone(), self.global.profile.clone(), strict)?;
        if let Some(name) = &self.global.loadout {
            let layer = aho_compose::loadouts::load(&i.home, name)?;
            i = i.with_layer(layer);
        }
        Ok(i)
    }

    pub fn load(&self, inputs: &Inputs) -> Result<Planned, Fail> {
        Ok(aho_compose::load(&self.comp, inputs)?)
    }

    pub fn caps(&self, inputs: Inputs) -> surface_rpc::CapsContext {
        surface_rpc::CapsContext::new(self.comp.clone(), inputs)
    }

    /// `session.dir`, relative to the project.
    pub fn session_dir(&self, p: &Planned) -> PathBuf {
        session_dir_of(&self.project, p)
    }

    /// The host world's workspace root: `workspace.root` if set, else the project.
    pub fn workspace(&self, p: &Planned) -> PathBuf {
        workspace_of(&self.project, p)
    }

    /// The daemon's socket (12): `--socket`, else `<project>/.aho/aho.sock`.
    pub fn socket(&self, typed: Option<&Path>) -> PathBuf {
        match typed {
            Some(p) => self.typed_path(p),
            None => self.project.join(".aho").join("aho.sock"),
        }
    }
}

pub fn session_dir_of(project: &Path, p: &Planned) -> PathBuf {
    project.join(p.loadout.str("session.dir").unwrap_or(".aho/sessions"))
}

pub fn workspace_of(project: &Path, p: &Planned) -> PathBuf {
    match p.loadout.str("workspace.root").filter(|s| !s.is_empty()) {
        Some(r) => std::fs::canonicalize(project.join(r)).unwrap_or_else(|_| project.join(r)),
        None => project.to_path_buf(),
    }
}

// ───────────────────────────── terminal ─────────────────────────────

pub fn stdin_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Colour on a terminal unless `NO_COLOR` is set (https://no-color.org).
pub fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// `apply? [y/N]`. `--yes` answers yes; end of input answers no.
pub fn confirm(yes: bool) -> Result<bool, Fail> {
    if yes {
        return Ok(true);
    }
    print!("apply? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    let n = std::io::stdin().lock().read_line(&mut answer)?;
    if n == 0 {
        println!();
    }
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// `2026-09-26 14:02` (UTC) from a system time.
pub fn when(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // civil-from-days (Howard Hinnant), proleptic Gregorian.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem / 60 % 60)
}

// ───────────────────────────── sessions ─────────────────────────────

/// Time-ordered id: milliseconds in the high bits, then pid and sub-millisecond noise.
pub fn new_session_id() -> SessionId {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let ms = now.as_millis() & ((1u128 << 48) - 1);
    let pid = u128::from(std::process::id() & 0xffff);
    let noise = u128::from(now.subsec_nanos() % 1_000_000);
    SessionId((ms << 64) | (pid << 32) | noise)
}

pub fn hex(id: SessionId) -> String {
    format!("{:x}", id.0)
}

pub struct SessionFile {
    pub id: SessionId,
    pub path: PathBuf,
    pub modified: SystemTime,
}

/// Every `<id>.jsonl` under `dir`, newest first.
pub fn sessions(dir: &Path) -> Result<Vec<SessionFile>, Fail> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(err(format!("could not list {} ({e})", dir.display()))),
    };
    let mut out: Vec<SessionFile> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?.to_string();
            let id = u128::from_str_radix(&stem, 16).ok()?;
            let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            Some(SessionFile { id: SessionId(id), path, modified })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.id.cmp(&a.id)));
    Ok(out)
}

/// A session by id or unique prefix; `None` = the most recent one.
pub fn find_session(dir: &Path, which: Option<&str>) -> Result<SessionFile, Fail> {
    let all = sessions(dir)?;
    let Some(prefix) = which else {
        return all.into_iter().next().ok_or_else(|| err(format!("no session in {} yet; start one with `aho run`", dir.display())));
    };
    let mut hits: Vec<SessionFile> = all.into_iter().filter(|s| hex(s.id).starts_with(prefix)).collect();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(err(format!("no session `{prefix}` in {}; `aho session list` shows them", dir.display()))),
        n => Err(err(format!("`{prefix}` matches {n} sessions; give more of the id"))),
    }
}

/// Entries on the session's current branch, oldest first.
pub fn branch(path: &Path) -> Result<(JsonlLog, Vec<Entry>), Fail> {
    let log = JsonlLog::open(path)?;
    let entries = match log.head() {
        Some(h) => log.read_branch(h)?,
        None => Vec::new(),
    };
    Ok((log, entries))
}

/// The derived cross-session index next to the logs (06: JSONL stays the truth; this is a
/// rebuildable projection). Opening it scans the directory only when the database is new.
pub fn session_index(dir: &Path) -> Result<std::sync::Arc<SessionIndex>, Fail> {
    let db = dir.join("index.sqlite3");
    SessionIndex::open(&db, dir).map(std::sync::Arc::new).map_err(|e| {
        err(format!("the session index {} could not be opened ({e}); move it aside and it is rebuilt from the logs", db.display()))
    })
}

/// The one way this process writes session `id`'s log: appends keep the index current.
pub fn open_writer(dir: &Path, id: SessionId) -> Result<IndexedLog, Fail> {
    session_index(dir)?.open_log(id).map_err(|e| err(format!("the log of session {} could not be opened ({e})", hex(id))))
}

/// Crash-honest liveness (12): the pid runs *and* started no later than the session recorded.
pub fn owner_alive(p: &ProcessIdentity) -> bool {
    if p.pid == std::process::id() {
        return true;
    }
    let Ok(out) = std::process::Command::new("ps").args(["-o", "etime=", "-p", &p.pid.to_string()]).output() else { return false };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(elapsed) = parse_etime(text.trim()) else { return false };
    let started_ms = Timestamp::now().0.saturating_sub(elapsed * 1000);
    started_ms <= p.start_time.0 + 2_000
}

/// `ps -o etime` format: `[[dd-]hh:]mm:ss`.
fn parse_etime(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let (h, m, sec) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    Some(days * 86_400 + h * 3600 + m * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etime_parses_every_ps_shape() {
        assert_eq!(parse_etime("05:07"), Some(307));
        assert_eq!(parse_etime("1:00:00"), Some(3600));
        assert_eq!(parse_etime("2-00:00:01"), Some(172_801));
        assert_eq!(parse_etime("x"), None);
    }

    #[test]
    fn dates_render_in_utc() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        assert_eq!(when(t), "2026-09-21 14:13");
    }
}
