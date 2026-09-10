//! `fs-host`: the Fs seam on the host disk, fenced to the workspace root.
//!
//! Every path is resolved lexically against the root (a `..` that climbs out is refused), then
//! walked component by component: a symlink is followed only if its target stays inside the
//! root, and a dangling one is refused. Writes are atomic (temp file + rename in the same dir).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_fs::{
    AppliedDiff, DIFF_CONTEXT, FileVersion, Fs, FsError, GrepHit, MAX_HITS, ReadResult, ScratchDir, Stat, lexical_rel, unified_hunks,
};
use aho_types::BoxFuture;

/// Longest line text returned in a grep hit (chars).
const GREP_LINE_CHARS: usize = 500;
/// Files larger than this are skipped by grep.
const GREP_MAX_FILE: u64 = 16 << 20;

#[derive(Clone)]
pub struct HostFs {
    core: Arc<FsCore>,
}

struct FsCore {
    /// Canonical root.
    root: PathBuf,
    root_str: String,
    /// The root as configured, when it differs from the canonical form (macOS `/var` → `/private/var`).
    alias: Option<String>,
}

impl HostFs {
    /// `root` must exist and be a directory; it is canonicalized once here.
    pub fn new(root: &Path) -> Result<HostFs, String> {
        let canon = std::fs::canonicalize(root).map_err(|e| format!("workspace root `{}` cannot be opened: {e}", root.display()))?;
        if !canon.is_dir() {
            return Err(format!("workspace root `{}` is not a directory", root.display()));
        }
        let root_str = canon.to_string_lossy().into_owned();
        let given = root.to_string_lossy().trim_end_matches('/').to_string();
        let alias = (given != root_str && given.starts_with('/')).then_some(given);
        Ok(HostFs { core: Arc::new(FsCore { root: canon, root_str, alias }) })
    }

    pub fn root_path(&self) -> &Path {
        &self.core.root
    }
}

struct HostScratch {
    // TempDir owns cleanup on drop, including when the allocation future is cancelled.
    dir: Option<tempfile::TempDir>,
    path: String,
}

impl ScratchDir for HostScratch {
    fn path(&self) -> &str {
        &self.path
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), FsError>> {
        Box::pin(async move {
            let Some(dir) = &self.dir else { return Ok(()) };
            let path = dir.path().to_path_buf();
            blocking(move || match std::fs::remove_dir_all(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io(&path.to_string_lossy(), "remove scratch directory", e)),
            })
            .await?;
            // Keep the lease armed until removal succeeds, even if this future is cancelled.
            self.dir.take();
            Ok(())
        })
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, FsError> + Send + 'static) -> Result<T, FsError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(FsError::Io(format!("the filesystem worker stopped unexpectedly ({e}); nothing was done. Retry the call."))),
    }
}

macro_rules! off_thread {
    ($self:ident, |$core:ident| $body:expr) => {{
        let $core = $self.core.clone();
        Box::pin(blocking(move || $body))
    }};
}

impl Fs for HostFs {
    fn root(&self) -> String {
        self.core.root_str.clone()
    }
    fn read(&self, path: &str, range: Option<(u32, u32)>) -> BoxFuture<'_, Result<ReadResult, FsError>> {
        let path = path.to_string();
        off_thread!(self, |core| core.read(&path, range))
    }
    fn read_bytes(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        let path = path.to_string();
        off_thread!(self, |core| core.read_bytes(&path).map(|(_, b)| b))
    }
    fn stat(&self, path: &str) -> BoxFuture<'_, Result<Stat, FsError>> {
        let path = path.to_string();
        off_thread!(self, |core| core.stat(&path))
    }
    fn write(&self, path: &str, text: &str, expect: Option<FileVersion>) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        let (path, text) = (path.to_string(), text.to_string());
        off_thread!(self, |core| core.write(&path, &text, expect))
    }
    fn edit(
        &self,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
        expect: FileVersion,
    ) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        let (path, old, new) = (path.to_string(), old.to_string(), new.to_string());
        off_thread!(self, |core| core.edit(&path, &old, &new, replace_all, expect))
    }
    fn glob(&self, pattern: &str, base: Option<&str>) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        let (pattern, base) = (pattern.to_string(), base.map(str::to_string));
        off_thread!(self, |core| core.glob(&pattern, base.as_deref()))
    }
    fn grep(&self, pattern: &str, base: Option<&str>, include: Option<&str>) -> BoxFuture<'_, Result<Vec<GrepHit>, FsError>> {
        let (pattern, base, include) = (pattern.to_string(), base.map(str::to_string), include.map(str::to_string));
        off_thread!(self, |core| core.grep(&pattern, base.as_deref(), include.as_deref()))
    }
    fn list(&self, dir: &str) -> BoxFuture<'_, Result<Vec<Stat>, FsError>> {
        let dir = dir.to_string();
        off_thread!(self, |core| core.list(&dir))
    }
    fn remove(&self, path: &str) -> BoxFuture<'_, Result<(), FsError>> {
        let path = path.to_string();
        off_thread!(self, |core| core.remove(&path))
    }
    fn create_dir(&self, dir: &str) -> BoxFuture<'_, Result<(), FsError>> {
        let dir = dir.to_string();
        off_thread!(self, |core| {
            let (_, path) = core.resolve(&dir)?;
            std::fs::create_dir_all(path).map_err(|e| io(&dir, "create directory", e))
        })
    }
    fn scratch_dir(&self, parent: &str, prefix: &str) -> BoxFuture<'_, Result<Box<dyn ScratchDir>, FsError>> {
        let (parent, prefix) = (parent.to_string(), prefix.to_string());
        off_thread!(self, |core| {
            if prefix.is_empty() || prefix.contains(['/', '\\', '\0']) || prefix == "." || prefix == ".." {
                return Err(FsError::Invalid("a scratch prefix must be a nonempty single path component".into()));
            }
            let (_, dir) = core.resolve(&parent)?;
            let scratch =
                tempfile::Builder::new().prefix(&prefix).tempdir_in(dir).map_err(|e| io(&parent, "create scratch directory", e))?;
            let path = scratch.path().to_string_lossy().into_owned();
            Ok(Box::new(HostScratch { dir: Some(scratch), path }) as Box<dyn ScratchDir>)
        })
    }
}

fn io(rel: &str, what: &str, e: std::io::Error) -> FsError {
    if e.kind() == std::io::ErrorKind::NotFound {
        FsError::NotFound(rel.to_string())
    } else if e.kind() == std::io::ErrorKind::PermissionDenied {
        FsError::Denied(format!("the host refused to {what} `{rel}` ({e}); nothing was changed. Check its permissions."))
    } else {
        FsError::Io(format!("could not {what} `{rel}`: {e}; nothing was changed."))
    }
}

fn join_rel(base: &str, name: &str) -> String {
    if base == "." { name.to_string() } else { format!("{base}/{name}") }
}

impl FsCore {
    fn rel(&self, path: &str) -> Result<String, FsError> {
        match lexical_rel(&self.root_str, path) {
            Err(FsError::OutsideWorkspace(_)) if path.starts_with('/') => match &self.alias {
                Some(a) => lexical_rel(a, path),
                None => Err(FsError::OutsideWorkspace(path.to_string())),
            },
            r => r,
        }
    }

    /// `(workspace-relative path, real path)`; symlinks resolved and fenced.
    fn resolve(&self, path: &str) -> Result<(String, PathBuf), FsError> {
        let rel = self.rel(path)?;
        let real = self.walk(path, &rel)?;
        Ok((rel, real))
    }

    /// Like `resolve`, but the final component is not followed if it is a symlink (for `remove`).
    fn resolve_parent(&self, path: &str) -> Result<(String, PathBuf), FsError> {
        let rel = self.rel(path)?;
        if rel == "." {
            return Ok((rel, self.root.clone()));
        }
        let (parent, name) = rel.rsplit_once('/').unwrap_or((".", rel.as_str()));
        let real = self.walk(path, parent)?;
        Ok((rel.clone(), real.join(name)))
    }

    /// Real path of `rel` (already lexically fenced), following every symlink on the way.
    fn walk(&self, shown: &str, rel: &str) -> Result<PathBuf, FsError> {
        let mut cur = self.root.clone();
        if rel == "." {
            return Ok(cur);
        }
        let comps: Vec<&str> = rel.split('/').collect();
        for (i, comp) in comps.iter().enumerate() {
            cur.push(comp);
            match std::fs::symlink_metadata(&cur) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // The rest does not exist yet; `cur`'s parent is a real directory inside the root.
                    for rest in &comps[i + 1..] {
                        cur.push(rest);
                    }
                    return Ok(cur);
                }
                Err(e) => return Err(io(shown, "inspect", e)),
                Ok(m) if m.file_type().is_symlink() => match std::fs::canonicalize(&cur) {
                    Ok(real) if real.starts_with(&self.root) => cur = real,
                    Ok(_) => return Err(FsError::OutsideWorkspace(shown.to_string())),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Dangling: refuse whether the missing target is inside or outside.
                        return Err(match std::fs::read_link(&cur) {
                            Ok(t) if t.is_absolute() && !t.starts_with(&self.root) => FsError::OutsideWorkspace(shown.to_string()),
                            _ => FsError::DanglingSymlink(shown.to_string()),
                        });
                    }
                    Err(e) => return Err(io(shown, "resolve", e)),
                },
                Ok(_) => {}
            }
        }
        Ok(cur)
    }

    fn read_bytes(&self, path: &str) -> Result<(String, Vec<u8>), FsError> {
        let (rel, real) = self.resolve(path)?;
        let meta = std::fs::metadata(&real).map_err(|e| io(&rel, "read", e))?;
        if meta.is_dir() {
            return Err(FsError::Invalid(format!("`{rel}` is a directory; nothing was read. List it or glob inside it.")));
        }
        let bytes = std::fs::read(&real).map_err(|e| io(&rel, "read", e))?;
        Ok((rel, bytes))
    }

    fn read(&self, path: &str, range: Option<(u32, u32)>) -> Result<ReadResult, FsError> {
        let (rel, bytes) = self.read_bytes(path)?;
        let version = FileVersion::of(&bytes);
        let text = String::from_utf8(bytes).map_err(|e| {
            FsError::Invalid(format!(
                "`{rel}` is not UTF-8 text ({} bytes); nothing was read. Use read_image for images.",
                e.as_bytes().len()
            ))
        })?;
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let total = lines.len() as u32;
        let Some((first, last)) = range else {
            return Ok(ReadResult { text, version, total_lines: total, partial: false });
        };
        let first = first.max(1);
        if last < first {
            return Err(FsError::Invalid(format!("line range {first}..{last} is empty; nothing was read. Give first ≤ last.")));
        }
        if first > total.max(1) {
            return Err(FsError::Invalid(format!("`{rel}` has {total} lines, so a read starting at line {first} returns nothing.")));
        }
        let last = last.min(total);
        let slice = if total == 0 { String::new() } else { lines[(first - 1) as usize..last as usize].concat() };
        Ok(ReadResult { text: slice, version, total_lines: total, partial: first > 1 || last < total })
    }

    fn stat(&self, path: &str) -> Result<Stat, FsError> {
        let (rel, real) = self.resolve(path)?;
        let meta = std::fs::metadata(&real).map_err(|e| io(&rel, "inspect", e))?;
        let version = if meta.is_file() { Some(FileVersion::of(&std::fs::read(&real).map_err(|e| io(&rel, "read", e))?)) } else { None };
        Ok(Stat { path: rel, is_dir: meta.is_dir(), bytes: meta.len(), version })
    }

    fn existing(&self, rel: &str, real: &Path) -> Result<Option<Vec<u8>>, FsError> {
        match std::fs::metadata(real) {
            Ok(m) if m.is_dir() => Err(FsError::Invalid(format!("`{rel}` is a directory; nothing was written."))),
            Ok(_) => std::fs::read(real).map(Some).map_err(|e| io(rel, "read", e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(rel, "inspect", e)),
        }
    }

    fn write(&self, path: &str, text: &str, expect: Option<FileVersion>) -> Result<AppliedDiff, FsError> {
        let (rel, real) = self.resolve(path)?;
        if rel == "." {
            return Err(FsError::Invalid("the workspace root is a directory; nothing was written. Name a file.".into()));
        }
        let old = self.existing(&rel, &real)?;
        match (&old, expect) {
            (Some(bytes), Some(expected)) => {
                let actual = FileVersion::of(bytes);
                if actual != expected {
                    return Err(FsError::StaleVersion { path: rel, expected, actual });
                }
            }
            (None, Some(_)) => return Err(FsError::NotFound(rel)),
            _ => {}
        }
        atomic_write(&rel, &real, text.as_bytes())?;
        let old_text = old.as_deref().map(String::from_utf8_lossy).unwrap_or_default();
        Ok(AppliedDiff {
            path: rel,
            created: old.is_none(),
            hunks: unified_hunks(&old_text, text, DIFF_CONTEXT),
            new_version: FileVersion::of(text.as_bytes()),
        })
    }

    fn edit(&self, path: &str, old: &str, new: &str, replace_all: bool, expect: FileVersion) -> Result<AppliedDiff, FsError> {
        if old.is_empty() {
            return Err(FsError::Invalid(
                "the text to replace is empty; nothing was changed. Use write to create or overwrite a file.".into(),
            ));
        }
        if old == new {
            return Err(FsError::Invalid("the replacement is identical to the text it replaces; nothing was changed.".into()));
        }
        let (rel, real) = self.resolve(path)?;
        let bytes = self.existing(&rel, &real)?.ok_or_else(|| FsError::NotFound(rel.clone()))?;
        let actual = FileVersion::of(&bytes);
        if actual != expect {
            return Err(FsError::StaleVersion { path: rel, expected: expect, actual });
        }
        let text = String::from_utf8(bytes).map_err(|_| FsError::Invalid(format!("`{rel}` is not UTF-8 text; nothing was changed.")))?;
        let count = text.matches(old).count();
        if count == 0 {
            return Err(FsError::NoMatch(rel));
        }
        if count > 1 && !replace_all {
            return Err(FsError::NotUnique { path: rel, count });
        }
        let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        atomic_write(&rel, &real, updated.as_bytes())?;
        Ok(AppliedDiff {
            path: rel,
            created: false,
            hunks: unified_hunks(&text, &updated, DIFF_CONTEXT),
            new_version: FileVersion::of(updated.as_bytes()),
        })
    }

    /// Real directory to walk for glob/grep, or the one file `base` names.
    fn base(&self, base: Option<&str>) -> Result<(String, PathBuf, bool), FsError> {
        let (rel, real) = self.resolve(base.unwrap_or("."))?;
        let meta = std::fs::metadata(&real).map_err(|e| io(&rel, "open", e))?;
        Ok((rel, real, meta.is_dir()))
    }

    fn walker(&self, dir: &Path) -> ignore::Walk {
        ignore::WalkBuilder::new(dir)
            .hidden(false)
            .require_git(false)
            .git_global(false)
            .follow_links(false)
            .filter_entry(|e| e.file_name() != ".git")
            .sort_by_file_name(|a, b| a.cmp(b))
            .build()
    }

    /// Workspace-relative path of a walked file, if it is a regular file (or an in-root symlink to one).
    fn walked_file(&self, e: &ignore::DirEntry) -> Option<String> {
        let ft = e.file_type()?;
        let path = e.path();
        if ft.is_symlink() {
            let real = std::fs::canonicalize(path).ok()?;
            if !real.starts_with(&self.root) || !real.is_file() {
                return None;
            }
        } else if !ft.is_file() {
            return None;
        }
        let rel = path.strip_prefix(&self.root).ok()?;
        Some(rel.to_string_lossy().into_owned())
    }

    fn glob(&self, pattern: &str, base: Option<&str>) -> Result<Vec<String>, FsError> {
        let pattern = pattern.trim_start_matches("./");
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| FsError::Invalid(format!("`{pattern}` is not a valid glob ({e}); nothing was searched.")))?
            .compile_matcher();
        let (_, dir, is_dir) = self.base(base)?;
        if !is_dir {
            return Err(FsError::Invalid("glob needs a directory as its base; nothing was searched.".into()));
        }
        let mut out = Vec::new();
        for e in self.walker(&dir).flatten() {
            let Some(rel) = self.walked_file(&e) else { continue };
            let Ok(under) = e.path().strip_prefix(&dir) else { continue };
            if matcher.is_match(under) {
                out.push(rel);
                if out.len() >= MAX_HITS {
                    break;
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn grep(&self, pattern: &str, base: Option<&str>, include: Option<&str>) -> Result<Vec<GrepHit>, FsError> {
        let re = regex::Regex::new(pattern)
            .map_err(|e| FsError::Invalid(format!("`{pattern}` is not a valid regex ({e}); nothing was searched.")))?;
        let include = match include {
            Some(g) => Some((
                globset::GlobBuilder::new(g)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| FsError::Invalid(format!("`{g}` is not a valid include glob ({e}); nothing was searched.")))?
                    .compile_matcher(),
                g.contains('/'),
            )),
            None => None,
        };
        let (base_rel, dir, is_dir) = self.base(base)?;
        let mut hits = Vec::new();
        let scan = |rel: &str, real: &Path, hits: &mut Vec<GrepHit>| {
            if std::fs::metadata(real).map(|m| m.len() > GREP_MAX_FILE).unwrap_or(true) {
                return;
            }
            let Ok(bytes) = std::fs::read(real) else { return };
            if bytes.iter().take(8000).any(|b| *b == 0) {
                return;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else { return };
            for (i, line) in text.lines().enumerate() {
                if hits.len() >= MAX_HITS {
                    return;
                }
                if re.is_match(line) {
                    let text: String = line.chars().take(GREP_LINE_CHARS).collect();
                    hits.push(GrepHit { path: rel.to_string(), line: i as u32 + 1, text });
                }
            }
        };
        if !is_dir {
            scan(&base_rel, &dir, &mut hits);
            return Ok(hits);
        }
        for e in self.walker(&dir).flatten() {
            if hits.len() >= MAX_HITS {
                break;
            }
            let Some(rel) = self.walked_file(&e) else { continue };
            if let Some((m, by_path)) = &include {
                let target: &Path = if *by_path { e.path().strip_prefix(&dir).unwrap_or(e.path()) } else { Path::new(e.file_name()) };
                if !m.is_match(target) {
                    continue;
                }
            }
            scan(&rel, e.path(), &mut hits);
        }
        hits.sort();
        Ok(hits)
    }

    fn list(&self, dir: &str) -> Result<Vec<Stat>, FsError> {
        let (rel, real) = self.resolve(dir)?;
        let rd = std::fs::read_dir(&real).map_err(|e| io(&rel, "list", e))?;
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (is_dir, bytes) = match std::fs::metadata(entry.path()) {
                Ok(m) => (m.is_dir(), m.len()),
                Err(_) => (false, 0), // dangling symlink: listed, refused on use
            };
            out.push(Stat { path: join_rel(&rel, &name), is_dir, bytes, version: None });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        let (rel, target) = self.resolve_parent(path)?;
        if rel == "." {
            return Err(FsError::Invalid("refusing to remove the workspace root; nothing was removed.".into()));
        }
        let meta = std::fs::symlink_metadata(&target).map_err(|e| io(&rel, "remove", e))?;
        let r = if meta.is_dir() { std::fs::remove_dir_all(&target) } else { std::fs::remove_file(&target) };
        r.map_err(|e| io(&rel, "remove", e))
    }
}

/// Temp file in the same directory, fsync, carry over the old mode, rename over the target.
fn atomic_write(rel: &str, real: &Path, bytes: &[u8]) -> Result<(), FsError> {
    let parent = real.parent().ok_or_else(|| FsError::Invalid(format!("`{rel}` has no parent directory; nothing was written.")))?;
    std::fs::create_dir_all(parent).map_err(|e| io(rel, "create the parent directory of", e))?;
    let name = real.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = parent.join(format!(".{name}.aho-tmp.{}.{nanos}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Ok(m) = std::fs::metadata(real) {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        std::fs::rename(&tmp, real)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(rel, "write", e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> (tempfile::TempDir, HostFs) {
        let dir = tempfile::tempdir().unwrap();
        let fs = HostFs::new(dir.path()).unwrap();
        (dir, fs)
    }

    #[tokio::test]
    async fn fence_refuses_dotdot_escapes_in_every_form() {
        let (dir, fs) = world();
        std::fs::write(dir.path().parent().unwrap().join("aho-sibling-secret"), "s").ok();
        for p in ["../aho-sibling-secret", "a/../../aho-sibling-secret", "./../x"] {
            assert!(matches!(fs.read(p, None).await, Err(FsError::OutsideWorkspace(_))), "{p}");
            assert!(matches!(fs.write(p, "x", None).await, Err(FsError::OutsideWorkspace(_))), "{p}");
        }
        let abs_inside = format!("{}/ok.txt", dir.path().display());
        fs.write(&abs_inside, "fine\n", None).await.unwrap();
        assert_eq!(fs.read("ok.txt", None).await.unwrap().text, "fine\n", "the configured (non-canonical) root is accepted");
        let abs_outside = format!("{}/../aho-sibling-secret", dir.path().display());
        assert!(matches!(fs.read(&abs_outside, None).await, Err(FsError::OutsideWorkspace(_))));
    }

    #[tokio::test]
    async fn fence_follows_inside_symlinks_and_refuses_outside_and_dangling_ones() {
        let (dir, fs) = world();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "s").unwrap();
        std::fs::write(dir.path().join("real.txt"), "real\n").unwrap();
        std::os::unix::fs::symlink("real.txt", dir.path().join("alias.txt")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("secret-link")).unwrap();
        std::os::unix::fs::symlink("nowhere", dir.path().join("dangling")).unwrap();
        std::os::unix::fs::symlink("/definitely/not/here", dir.path().join("dangling-out")).unwrap();

        assert_eq!(fs.read("alias.txt", None).await.unwrap().text, "real\n");
        assert!(matches!(fs.read("out/secret", None).await, Err(FsError::OutsideWorkspace(_))));
        assert!(matches!(fs.read("secret-link", None).await, Err(FsError::OutsideWorkspace(_))));
        assert!(matches!(fs.write("out/new.txt", "x", None).await, Err(FsError::OutsideWorkspace(_))));
        assert!(!outside.path().join("new.txt").exists());
        assert!(matches!(fs.read("dangling", None).await, Err(FsError::DanglingSymlink(_))));
        assert!(matches!(fs.write("dangling", "x", None).await, Err(FsError::DanglingSymlink(_))));
        assert!(matches!(fs.write("dangling/child.txt", "x", None).await, Err(FsError::DanglingSymlink(_))));
        assert!(!dir.path().join("nowhere").exists(), "writing through a dangling link must not create its target");
        assert!(matches!(fs.read("dangling-out", None).await, Err(FsError::OutsideWorkspace(_))));

        fs.remove("alias.txt").await.unwrap();
        assert!(dir.path().join("real.txt").exists(), "remove deletes the link, not its target");
        fs.remove("out").await.unwrap();
        assert!(outside.path().join("secret").exists());
        assert!(matches!(fs.remove(".").await, Err(FsError::Invalid(_))));
    }

    #[tokio::test]
    async fn write_is_atomic_creates_parents_and_checks_versions() {
        let (dir, fs) = world();
        let d = fs.write("a/b/c.txt", "one\ntwo\n", None).await.unwrap();
        assert!(d.created);
        assert_eq!(d.path, "a/b/c.txt");
        assert_eq!(d.new_version, FileVersion::of(b"one\ntwo\n"));
        assert_eq!(d.hunks, vec!["@@ -0,0 +1,2 @@\n+one\n+two".to_string()]);

        let r = fs.read("a/b/c.txt", None).await.unwrap();
        assert_eq!((r.version, r.total_lines, r.partial), (d.new_version, 2, false));

        let stale = fs.write("a/b/c.txt", "x", Some(FileVersion::of(b"other"))).await;
        assert!(
            matches!(stale, Err(FsError::StaleVersion { expected, actual, .. }) if expected == FileVersion::of(b"other") && actual == d.new_version)
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(), "one\ntwo\n", "a refused write changes nothing");

        let d2 = fs.write("a/b/c.txt", "one\nTWO\n", Some(d.new_version)).await.unwrap();
        assert!(!d2.created);
        assert_eq!(d2.hunks, vec!["@@ -1,2 +1,2 @@\n one\n-two\n+TWO".to_string()]);
        let names: Vec<String> =
            std::fs::read_dir(dir.path().join("a/b")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["c.txt".to_string()], "no temp file is left behind");
        assert!(matches!(fs.write("gone.txt", "x", Some(d.new_version)).await, Err(FsError::NotFound(_))));
        assert!(matches!(fs.write("a", "x", None).await, Err(FsError::Invalid(_))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_keeps_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, fs) = world();
        let p = dir.path().join("run.sh");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let v = FileVersion::of(b"#!/bin/sh\n");
        fs.write("run.sh", "#!/bin/sh\necho hi\n", Some(v)).await.unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[tokio::test]
    async fn edit_requires_a_unique_match_and_the_current_version() {
        let (_dir, fs) = world();
        let v = fs.write("f.rs", "let a = 1;\nlet b = 1;\n", None).await.unwrap().new_version;
        assert!(matches!(fs.edit("f.rs", "= 1", "= 2", false, v).await, Err(FsError::NotUnique { count: 2, .. })));
        assert!(matches!(fs.edit("f.rs", "let a", "let z", false, FileVersion::of(b"x")).await, Err(FsError::StaleVersion { .. })));
        assert!(matches!(fs.edit("f.rs", "nope", "x", false, v).await, Err(FsError::NoMatch(_))));
        assert!(matches!(fs.edit("f.rs", "", "x", false, v).await, Err(FsError::Invalid(_))));
        assert!(matches!(fs.edit("missing.rs", "a", "b", false, v).await, Err(FsError::NotFound(_))));
        let d = fs.edit("f.rs", "let a = 1;", "let a = 2;", false, v).await.unwrap();
        assert_eq!(d.hunks, vec!["@@ -1,2 +1,2 @@\n-let a = 1;\n+let a = 2;\n let b = 1;".to_string()]);
        let d = fs.edit("f.rs", "= ", "== ", true, d.new_version).await.unwrap();
        assert_eq!(fs.read("f.rs", None).await.unwrap().text, "let a == 2;\nlet b == 1;\n");
        assert_eq!(fs.read("f.rs", None).await.unwrap().version, d.new_version);
    }

    #[tokio::test]
    async fn ranged_reads_report_partial_and_total_lines() {
        let (_dir, fs) = world();
        fs.write("n.txt", "1\n2\n3\n4\n5\n", None).await.unwrap();
        let r = fs.read("n.txt", Some((2, 3))).await.unwrap();
        assert_eq!((r.text.as_str(), r.total_lines, r.partial), ("2\n3\n", 5, true));
        let r = fs.read("n.txt", Some((1, 99))).await.unwrap();
        assert!(!r.partial);
        assert_eq!(r.version, FileVersion::of(b"1\n2\n3\n4\n5\n"), "a ranged read still versions the whole file");
        assert!(matches!(fs.read("n.txt", Some((9, 10))).await, Err(FsError::Invalid(_))));
        assert!(matches!(fs.read(".", None).await, Err(FsError::Invalid(_))));
        std::fs::write(_dir.path().join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(fs.read("bin.dat", None).await, Err(FsError::Invalid(_))));
        assert_eq!(fs.read_bytes("bin.dat").await.unwrap(), vec![0xff, 0xfe, 0x00]);
    }

    #[tokio::test]
    async fn grep_and_glob_respect_gitignore_sort_and_cap() {
        let (dir, fs) = world();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::write(dir.path().join("target/out.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("x.log"), "needle\n").unwrap();
        std::fs::write(dir.path().join(".git/config"), "needle\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "hay\nneedle here\n").unwrap();
        std::fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join(".hidden.rs"), "needle\n").unwrap();

        let hits = fs.grep("needle", None, None).await.unwrap();
        let got: Vec<(&str, u32)> = hits.iter().map(|h| (h.path.as_str(), h.line)).collect();
        assert_eq!(got, vec![(".hidden.rs", 1), ("a.rs", 1), ("b.rs", 2)]);
        let rs_only = fs.grep("needle", None, Some("*.rs")).await.unwrap();
        assert_eq!(rs_only.len(), 3);
        assert_eq!(fs.grep("needle", Some("b.rs"), None).await.unwrap()[0].text, "needle here");
        assert!(matches!(fs.grep("(", None, None).await, Err(FsError::Invalid(_))));

        assert_eq!(fs.glob("*.rs", None).await.unwrap(), vec![".hidden.rs", "a.rs", "b.rs"]);
        assert_eq!(fs.glob("**/*.rs", None).await.unwrap(), vec![".hidden.rs", "a.rs", "b.rs"], "ignored target/ stays out");

        std::fs::create_dir_all(dir.path().join("many")).unwrap();
        for i in 0..(MAX_HITS + 20) {
            std::fs::write(dir.path().join(format!("many/f{i:04}.txt")), "x").unwrap();
        }
        let many = fs.glob("many/*.txt", None).await.unwrap();
        assert_eq!(many.len(), MAX_HITS);
        assert_eq!(many.first().map(String::as_str), Some("many/f0000.txt"));
        assert!(many.windows(2).all(|w| w[0] < w[1]));
    }

    #[tokio::test]
    async fn list_and_stat_are_workspace_relative() {
        let (_dir, fs) = world();
        fs.write("d/one.txt", "1", None).await.unwrap();
        fs.write("d/sub/two.txt", "22", None).await.unwrap();
        let l = fs.list("d").await.unwrap();
        let got: Vec<(&str, bool)> = l.iter().map(|s| (s.path.as_str(), s.is_dir)).collect();
        assert_eq!(got, vec![("d/one.txt", false), ("d/sub", true)]);
        let s = fs.stat("d/sub/two.txt").await.unwrap();
        assert_eq!((s.bytes, s.version), (2, Some(FileVersion::of(b"22"))));
        assert_eq!(fs.stat("d").await.unwrap().version, None);
        assert!(matches!(fs.stat("d/nope").await, Err(FsError::NotFound(_))));
    }
}
