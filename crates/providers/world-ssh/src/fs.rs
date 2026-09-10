//! `fs-ssh`: the Fs seam on the remote host, fenced to the remote workspace root.
//!
//! Same contract as `fs-host`: paths resolve lexically against the root first
//! ([`aho_fs::lexical_rel`], locally), then the remote walks them component by component in POSIX
//! sh (`aho_walk` below): a symlink is followed only if its canonical target stays inside the
//! root, a dangling one is refused. Writes are temp file + `mv` in the same directory, guarded by
//! the `cksum` the preceding read saw so a change in between is caught instead of overwritten.
//! File bytes travel raw over the ssh channel (`cat` on stdout / from stdin).
//!
//! `glob`/`grep` list files remotely (`find`, `.git` pruned) together with every `.gitignore` /
//! `.ignore` in play, apply the ignore rules and the glob/regex here with the same crates
//! `fs-host` uses, and fetch only the surviving candidates' bytes for `grep`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_fs::{AppliedDiff, DIFF_CONTEXT, FileVersion, Fs, FsError, GrepHit, MAX_HITS, ReadResult, Stat, lexical_rel, unified_hunks};
use aho_types::BoxFuture;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::ssh::{Out, Ssh, nonce};

/// Longest line text returned in a grep hit (chars).
const GREP_LINE_CHARS: usize = 500;
/// Files larger than this are skipped by grep (the remote `find` marks them `b`).
const GREP_MAX_FILE: usize = 16 << 20;
/// Candidate files fetched per round trip while grepping.
const FETCH_BATCH: usize = 256;
/// Attempts at a guarded write when the file keeps changing underneath.
const WRITE_ATTEMPTS: usize = 3;

/// Prelude of every fs script: `$1` is the canonical root. Exit codes: 10 not found, 11 outside
/// the workspace, 12 dangling symlink, 13 is a directory, 14 permission denied, 15 changed since
/// read, 16 not a directory, 17 not a regular file, 20/21 I/O.
pub(crate) const PRELUDE: &str = r#"unset CDPATH
ROOT=$1; shift
aho_inroot() { case $1 in "$ROOT"|"$ROOT"/*) return 0 ;; esac; return 1; }
aho_canon() {
  _p=$1; _n=0
  while [ -L "$_p" ]; do
    _n=$((_n+1)); [ "$_n" -gt 40 ] && return 1
    _t=$(readlink -- "$_p") || return 1
    case $_t in /*) _p=$_t ;; *) _p=${_p%/*}/$_t ;; esac
  done
  if [ -d "$_p" ]; then (cd -P -- "$_p" && pwd -P); return; fi
  _d=${_p%/*}; [ -n "$_d" ] || _d=/
  _d=$(cd -P -- "$_d" && pwd -P) || return 1
  printf '%s/%s\n' "${_d%/}" "${_p##*/}"
}
aho_walk() {
  REAL=$ROOT
  [ "$1" = . ] && return 0
  _rest=$1
  while [ -n "$_rest" ]; do
    case $_rest in */*) _c=${_rest%%/*}; _rest=${_rest#*/} ;; *) _c=$_rest; _rest= ;; esac
    REAL=$REAL/$_c
    if [ -L "$REAL" ]; then
      if [ -e "$REAL" ]; then
        _r=$(aho_canon "$REAL") || exit 20
        aho_inroot "$_r" || exit 11
        REAL=$_r
      else
        _t=$(readlink -- "$REAL") || exit 20
        case $_t in /*) aho_inroot "$_t" || exit 11 ;; esac
        exit 12
      fi
    elif [ ! -e "$REAL" ]; then
      [ -z "$_rest" ] || REAL=$REAL/$_rest
      return 0
    fi
  done
}
"#;

const READ: &str = r#"aho_walk "$1"
[ -e "$REAL" ] || exit 10
[ -d "$REAL" ] && exit 13
[ -f "$REAL" ] || exit 17
[ -r "$REAL" ] || exit 14
exec cat -- "$REAL"
"#;

const STAT: &str = r#"aho_walk "$1"
[ -e "$REAL" ] || exit 10
if [ -d "$REAL" ]; then echo D; exit 0; fi
[ -f "$REAL" ] || exit 17
[ -r "$REAL" ] || exit 14
echo F
exec cat -- "$REAL"
"#;

/// `N` (absent) or `F <cksum>` then the bytes.
const PREP: &str = r#"aho_walk "$1"
[ -d "$REAL" ] && exit 13
if [ ! -e "$REAL" ]; then echo N; exit 0; fi
[ -f "$REAL" ] || exit 17
[ -r "$REAL" ] || exit 14
_k=$(cksum < "$REAL") || exit 20
printf 'F %s\n' "$_k"
exec cat -- "$REAL"
"#;

/// `$1` rel, `$2` guard from PREP, `$3` temp nonce, `$4` byte count; stdin = new bytes. `cp -p`
/// first so the temp file carries the old mode. The byte count is checked before the rename: if
/// the connection dies mid-transfer, `cat` sees an early EOF and the partial file must not land.
const COMMIT: &str = r#"aho_walk "$1"
[ -d "$REAL" ] && exit 13
if [ "$2" = N ]; then
  { [ -e "$REAL" ] || [ -L "$REAL" ]; } && exit 15
else
  [ -f "$REAL" ] || exit 15
  _k=$(cksum < "$REAL") || exit 20
  [ "F $_k" = "$2" ] || exit 15
fi
_dir=${REAL%/*}
mkdir -p -- "$_dir" || exit 21
_tmp=$_dir/.${REAL##*/}.aho-tmp.$3
if [ -f "$REAL" ]; then cp -p -- "$REAL" "$_tmp" || { rm -f -- "$_tmp"; exit 21; }; fi
if cat > "$_tmp" && [ "$(wc -c < "$_tmp" | tr -d ' ')" = "$4" ] && mv -f -- "$_tmp" "$REAL"; then exit 0; fi
rm -f -- "$_tmp"
exit 21
"#;

/// Records `d\0name\0`, `f\0name\0size\0`, `o\0name\0` (dangling link, fifo, …).
const LIST: &str = r#"aho_walk "$1"
[ -e "$REAL" ] || exit 10
[ -d "$REAL" ] || exit 16
cd -P -- "$REAL" || exit 20
for _f in * .[!.]* ..?*; do
  { [ -e "$_f" ] || [ -L "$_f" ]; } || continue
  if [ -d "$_f" ]; then printf 'd\0%s\0' "$_f"
  elif [ -f "$_f" ] && [ -r "$_f" ]; then printf 'f\0%s\0%s\0' "$_f" "$(wc -c < "$_f" | tr -d ' ')"
  else printf 'o\0%s\0' "$_f"
  fi
done
"#;

/// The last component is not followed: removing a symlink removes the link.
const REMOVE: &str = r#"case $1 in
  */*) aho_walk "${1%/*}"; _t=$REAL/${1##*/} ;;
  *) _t=$ROOT/$1 ;;
esac
{ [ -e "$_t" ] || [ -L "$_t" ]; } || exit 10
if [ -d "$_t" ] && [ ! -L "$_t" ]; then rm -rf -- "$_t" || exit 21; else rm -f -- "$_t" || exit 21; fi
"#;

/// `$1` base, `$2` boundary. Output: `<canonical base rel>\0` then `F\0` (a file) or `D\0` and
/// records `f\0path\0` (≤ 16 MiB), `b\0path\0` (bigger), `l\0path\0` (link to an in-root file),
/// then ignore files as `<boundary>g\0<root-rel path>\0<bytes>` / `<boundary>x\0\0<bytes>`
/// (`.git/info/exclude`), then a final boundary.
const WALK: &str = r#"_b=$2
aho_walk "$1"
[ -e "$REAL" ] || exit 10
_c=${REAL#"$ROOT"}; _c=${_c#/}
printf '%s\0' "${_c:-.}"
if [ ! -d "$REAL" ]; then printf 'F\0'; exit 0; fi
printf 'D\0'
cd -P -- "$REAL" || exit 20
find . -name .git -prune -o -type f -size -16777217c -exec printf 'f\0%s\0' {} + -o -type f -exec printf 'b\0%s\0' {} +
find . -name .git -prune -o -type l -print | while IFS= read -r _l; do
  _t=$(aho_canon "$REAL/${_l#./}") || continue
  [ -f "$_t" ] && aho_inroot "$_t" && printf 'l\0%s\0' "$_l"
done
find . -name .git -prune -o \( -name .gitignore -o -name .ignore \) -type f -print | while IFS= read -r _g; do
  printf '%sg\0%s\0' "$_b" "$_c/${_g#./}"; cat -- "$_g"
done
_a=$REAL
while [ "$_a" != "$ROOT" ] && [ -n "$_a" ]; do
  _a=${_a%/*}
  for _n in .gitignore .ignore; do
    [ -f "$_a/$_n" ] || continue
    _r=${_a#"$ROOT"}; printf '%sg\0%s\0' "$_b" "${_r#/}/$_n"; cat -- "$_a/$_n"
  done
done
if [ -f "$ROOT/.git/info/exclude" ]; then printf '%sx\0\0' "$_b"; cat -- "$ROOT/.git/info/exclude"; fi
printf '%s' "$_b"
"#;

/// `$1` boundary; stdin = root-relative paths, one per line. Output: `<boundary>path\0<bytes>`
/// per readable in-root regular file, then a final boundary.
const FETCH: &str = r#"_b=$1
while IFS= read -r _p; do
  _t=$(aho_canon "$ROOT/$_p") || continue
  aho_inroot "$_t" || continue
  { [ -f "$_t" ] && [ -r "$_t" ]; } || continue
  printf '%s%s\0' "$_b" "$_p"
  cat -- "$_t"
done
printf '%s' "$_b"
"#;

#[derive(Clone)]
pub struct SshFs {
    core: Arc<Core>,
}

struct Core {
    ssh: Arc<Ssh>,
    /// Canonical remote root (`pwd -P`).
    root: String,
    /// The root as configured, when it differs from the canonical form.
    alias: Option<String>,
}

enum Commit {
    Done,
    /// The file changed between the read and the write; nothing was written.
    Raced,
}

impl SshFs {
    /// `root` is the canonical remote workspace root; `alias` its logical spelling, if different.
    pub fn new(ssh: Arc<Ssh>, root: String, alias: Option<String>) -> SshFs {
        let alias = alias.filter(|a| a.starts_with('/') && a.trim_end_matches('/') != root);
        SshFs { core: Arc::new(Core { ssh, root, alias }) }
    }
}

impl Fs for SshFs {
    fn root(&self) -> String {
        self.core.root.clone()
    }
    fn read(&self, path: &str, range: Option<(u32, u32)>) -> BoxFuture<'_, Result<ReadResult, FsError>> {
        let path = path.to_string();
        Box::pin(async move { self.core.read(&path, range).await })
    }
    fn read_bytes(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        let path = path.to_string();
        Box::pin(async move { self.core.read_bytes(&path).await.map(|(_, b)| b) })
    }
    fn stat(&self, path: &str) -> BoxFuture<'_, Result<Stat, FsError>> {
        let path = path.to_string();
        Box::pin(async move { self.core.stat(&path).await })
    }
    fn write(&self, path: &str, text: &str, expect: Option<FileVersion>) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        let (path, text) = (path.to_string(), text.to_string());
        Box::pin(async move { self.core.write(&path, &text, expect).await })
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
        Box::pin(async move { self.core.edit(&path, &old, &new, replace_all, expect).await })
    }
    fn glob(&self, pattern: &str, base: Option<&str>) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        let (pattern, base) = (pattern.to_string(), base.map(str::to_string));
        Box::pin(async move { self.core.glob(&pattern, base.as_deref()).await })
    }
    fn grep(&self, pattern: &str, base: Option<&str>, include: Option<&str>) -> BoxFuture<'_, Result<Vec<GrepHit>, FsError>> {
        let (pattern, base, include) = (pattern.to_string(), base.map(str::to_string), include.map(str::to_string));
        Box::pin(async move { self.core.grep(&pattern, base.as_deref(), include.as_deref()).await })
    }
    fn list(&self, dir: &str) -> BoxFuture<'_, Result<Vec<Stat>, FsError>> {
        let dir = dir.to_string();
        Box::pin(async move { self.core.list(&dir).await })
    }
    fn remove(&self, path: &str) -> BoxFuture<'_, Result<(), FsError>> {
        let path = path.to_string();
        Box::pin(async move { self.core.remove(&path).await })
    }
}

fn join_rel(base: &str, name: &str) -> String {
    if base == "." || base.is_empty() { name.to_string() } else { format!("{base}/{name}") }
}

/// Splits `line\n rest` off the front of `bytes`.
fn first_line(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().position(|b| *b == b'\n') {
        Some(i) => (&bytes[..i], &bytes[i + 1..]),
        None => (bytes, &[]),
    }
}

impl Core {
    fn host(&self) -> String {
        self.ssh.target().display()
    }

    fn rel(&self, path: &str) -> Result<String, FsError> {
        match lexical_rel(&self.root, path) {
            Err(FsError::OutsideWorkspace(_)) if path.starts_with('/') => match &self.alias {
                Some(a) => lexical_rel(a, path),
                None => Err(FsError::OutsideWorkspace(path.to_string())),
            },
            r => r,
        }
    }

    async fn script(&self, body: &str, args: &[&str], input: Option<Vec<u8>>) -> Result<Out, FsError> {
        let script = format!("{PRELUDE}{body}");
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(self.root.as_str());
        all.extend_from_slice(args);
        self.ssh.run(&script, &all, input).await.map_err(|e| FsError::Io(e.to_string()))
    }

    /// Maps a script's exit code to the seam's error (`shown` = the path as given).
    fn fail(&self, out: &Out, shown: &str, rel: &str, what: &str) -> FsError {
        let host = self.host();
        match out.code {
            10 => FsError::NotFound(rel.to_string()),
            11 => FsError::OutsideWorkspace(shown.to_string()),
            12 => FsError::DanglingSymlink(shown.to_string()),
            13 => FsError::Invalid(format!("`{rel}` is a directory; nothing was {what}. Name a file, or list/glob inside it.")),
            17 => FsError::Invalid(format!("`{rel}` is not a regular file (a fifo, socket or device on {host}); nothing was {what}.")),
            14 => FsError::Denied(format!(
                "{host} refused access to `{rel}` (permission denied); nothing was {what}. Check its permissions there."
            )),
            _ if out.stderr.contains("Permission denied") => FsError::Denied(format!(
                "{host} refused access to `{rel}` ({}); nothing was {what}. Check its permissions there.",
                out.stderr.trim()
            )),
            16 => FsError::Io(format!("could not list `{rel}` on {host}: it is not a directory; nothing was listed.")),
            code => FsError::Io(format!("could not handle `{rel}` on {host} (exit {code}: {}); nothing was {what}.", out.stderr.trim())),
        }
    }

    async fn read_bytes(&self, path: &str) -> Result<(String, Vec<u8>), FsError> {
        let rel = self.rel(path)?;
        let out = self.script(READ, &[&rel], None).await?;
        if out.code != 0 {
            return Err(self.fail(&out, path, &rel, "read"));
        }
        Ok((rel, out.stdout))
    }

    async fn read(&self, path: &str, range: Option<(u32, u32)>) -> Result<ReadResult, FsError> {
        let (rel, bytes) = self.read_bytes(path).await?;
        slice_text(&rel, bytes, range)
    }

    async fn stat(&self, path: &str) -> Result<Stat, FsError> {
        let rel = self.rel(path)?;
        let out = self.script(STAT, &[&rel], None).await?;
        if out.code != 0 {
            return Err(self.fail(&out, path, &rel, "inspected"));
        }
        let (kind, body) = first_line(&out.stdout);
        Ok(match kind {
            b"D" => Stat { path: rel, is_dir: true, bytes: 0, version: None },
            _ => Stat { path: rel, is_dir: false, bytes: body.len() as u64, version: Some(FileVersion::of(body)) },
        })
    }

    /// `(guard, current bytes)`: the guard is what COMMIT re-checks before renaming.
    async fn prep(&self, path: &str, rel: &str) -> Result<(String, Option<Vec<u8>>), FsError> {
        let out = self.script(PREP, &[rel], None).await?;
        if out.code != 0 {
            return Err(self.fail(&out, path, rel, "written"));
        }
        let (guard, body) = first_line(&out.stdout);
        let guard = String::from_utf8_lossy(guard).into_owned();
        let bytes = guard.starts_with("F ").then(|| body.to_vec());
        Ok((guard, bytes))
    }

    async fn commit(&self, path: &str, rel: &str, guard: &str, bytes: &[u8]) -> Result<Commit, FsError> {
        let len = bytes.len().to_string();
        let out = self.script(COMMIT, &[rel, guard, &nonce()[..16], &len], Some(bytes.to_vec())).await?;
        match out.code {
            0 => Ok(Commit::Done),
            15 => Ok(Commit::Raced),
            _ => Err(self.fail(&out, path, rel, "written")),
        }
    }

    async fn write(&self, path: &str, text: &str, expect: Option<FileVersion>) -> Result<AppliedDiff, FsError> {
        let rel = self.rel(path)?;
        if rel == "." {
            return Err(FsError::Invalid("the workspace root is a directory; nothing was written. Name a file.".into()));
        }
        for _ in 0..WRITE_ATTEMPTS {
            let (guard, old) = self.prep(path, &rel).await?;
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
            if let Commit::Done = self.commit(path, &rel, &guard, text.as_bytes()).await? {
                let old_text = old.as_deref().map(String::from_utf8_lossy).unwrap_or_default();
                return Ok(AppliedDiff {
                    path: rel,
                    created: old.is_none(),
                    hunks: unified_hunks(&old_text, text, DIFF_CONTEXT),
                    new_version: FileVersion::of(text.as_bytes()),
                });
            }
        }
        Err(self.kept_changing(&rel))
    }

    fn kept_changing(&self, rel: &str) -> FsError {
        FsError::Io(format!(
            "`{rel}` kept changing on {} while it was being written ({WRITE_ATTEMPTS} attempts); nothing was written. Read it again and retry.",
            self.host()
        ))
    }

    async fn edit(&self, path: &str, old: &str, new: &str, replace_all: bool, expect: FileVersion) -> Result<AppliedDiff, FsError> {
        if old.is_empty() {
            return Err(FsError::Invalid(
                "the text to replace is empty; nothing was changed. Use write to create or overwrite a file.".into(),
            ));
        }
        if old == new {
            return Err(FsError::Invalid("the replacement is identical to the text it replaces; nothing was changed.".into()));
        }
        let rel = self.rel(path)?;
        for _ in 0..WRITE_ATTEMPTS {
            let (guard, bytes) = self.prep(path, &rel).await?;
            let bytes = bytes.ok_or_else(|| FsError::NotFound(rel.clone()))?;
            let actual = FileVersion::of(&bytes);
            if actual != expect {
                return Err(FsError::StaleVersion { path: rel, expected: expect, actual });
            }
            let text =
                String::from_utf8(bytes).map_err(|_| FsError::Invalid(format!("`{rel}` is not UTF-8 text; nothing was changed.")))?;
            let count = text.matches(old).count();
            if count == 0 {
                return Err(FsError::NoMatch(rel));
            }
            if count > 1 && !replace_all {
                return Err(FsError::NotUnique { path: rel, count });
            }
            let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
            if let Commit::Done = self.commit(path, &rel, &guard, updated.as_bytes()).await? {
                return Ok(AppliedDiff {
                    path: rel,
                    created: false,
                    hunks: unified_hunks(&text, &updated, DIFF_CONTEXT),
                    new_version: FileVersion::of(updated.as_bytes()),
                });
            }
        }
        Err(self.kept_changing(&rel))
    }

    async fn walk(&self, path: &str) -> Result<Listing, FsError> {
        let rel = self.rel(path)?;
        let boundary = format!("--aho-{}", nonce());
        let out = self.script(WALK, &[&rel, &boundary], None).await?;
        if out.code != 0 {
            return Err(self.fail(&out, path, &rel, "searched"));
        }
        Listing::parse(&out.stdout, boundary.as_bytes()).ok_or_else(|| {
            FsError::Io(format!("{} sent a file listing for `{rel}` that could not be parsed; nothing was searched.", self.host()))
        })
    }

    /// Bytes of `paths` (root-relative), in the order the remote sent them.
    async fn fetch(&self, paths: &[String]) -> Result<Vec<(String, Vec<u8>)>, FsError> {
        let boundary = format!("--aho-{}", nonce());
        let mut input = Vec::new();
        for p in paths {
            input.extend_from_slice(p.as_bytes());
            input.push(b'\n');
        }
        let out = self.script(FETCH, &[&boundary], Some(input)).await?;
        if out.code != 0 {
            return Err(self.fail(&out, ".", ".", "searched"));
        }
        let mut files = Vec::new();
        for part in split_on(&out.stdout, boundary.as_bytes()).into_iter().skip(1) {
            if let Some(nul) = part.iter().position(|b| *b == 0) {
                files.push((String::from_utf8_lossy(&part[..nul]).into_owned(), part[nul + 1..].to_vec()));
            }
        }
        Ok(files)
    }

    async fn glob(&self, pattern: &str, base: Option<&str>) -> Result<Vec<String>, FsError> {
        let pattern = pattern.trim_start_matches("./");
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| FsError::Invalid(format!("`{pattern}` is not a valid glob ({e}); nothing was searched.")))?
            .compile_matcher();
        let listing = self.walk(base.unwrap_or(".")).await?;
        if !listing.is_dir {
            return Err(FsError::Invalid("glob needs a directory as its base; nothing was searched.".into()));
        }
        let ignores = Ignores::build(&listing.ignores);
        let mut out: Vec<String> = listing
            .walked(&ignores)
            .into_iter()
            .filter(|(_, under)| matcher.is_match(under))
            .map(|(_, under)| join_rel(&listing.base, &under))
            .collect();
        out.truncate(MAX_HITS);
        out.sort();
        Ok(out)
    }

    async fn grep(&self, pattern: &str, base: Option<&str>, include: Option<&str>) -> Result<Vec<GrepHit>, FsError> {
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
        let shown_base = self.rel(base.unwrap_or("."))?;
        let listing = self.walk(base.unwrap_or(".")).await?;
        let mut hits = Vec::new();
        if !listing.is_dir {
            for (_, bytes) in self.fetch(std::slice::from_ref(&listing.base)).await? {
                scan(&re, &shown_base, &bytes, &mut hits);
            }
            return Ok(hits);
        }
        let ignores = Ignores::build(&listing.ignores);
        // A name with a newline cannot travel in FETCH's line-per-path input; such files are not grepped.
        let candidates: Vec<String> = listing
            .walked(&ignores)
            .into_iter()
            .filter(|(kind, _)| *kind != b'b')
            .filter(|(_, under)| match &include {
                Some((m, true)) => m.is_match(under),
                Some((m, false)) => m.is_match(under.rsplit('/').next().unwrap_or(under)),
                None => true,
            })
            .map(|(_, under)| join_rel(&listing.base, &under))
            .filter(|p| !p.contains('\n'))
            .collect();
        for batch in candidates.chunks(FETCH_BATCH) {
            let files: BTreeMap<String, Vec<u8>> = self.fetch(batch).await?.into_iter().collect();
            for p in batch {
                if hits.len() >= MAX_HITS {
                    break;
                }
                if let Some(bytes) = files.get(p) {
                    scan(&re, p, bytes, &mut hits);
                }
            }
            if hits.len() >= MAX_HITS {
                break;
            }
        }
        hits.truncate(MAX_HITS);
        hits.sort();
        Ok(hits)
    }

    async fn list(&self, dir: &str) -> Result<Vec<Stat>, FsError> {
        let rel = self.rel(dir)?;
        let out = self.script(LIST, &[&rel], None).await?;
        if out.code != 0 {
            return Err(self.fail(&out, dir, &rel, "listed"));
        }
        let mut fields = out.stdout.split(|b| *b == 0);
        let mut stats = Vec::new();
        while let (Some(kind), Some(name)) = (fields.next(), fields.next()) {
            let path = join_rel(&rel, &String::from_utf8_lossy(name));
            match kind {
                b"d" => stats.push(Stat { path, is_dir: true, bytes: 0, version: None }),
                b"f" => {
                    let bytes = fields.next().and_then(|s| std::str::from_utf8(s).ok()).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                    stats.push(Stat { path, is_dir: false, bytes, version: None });
                }
                _ => stats.push(Stat { path, is_dir: false, bytes: 0, version: None }),
            }
        }
        stats.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(stats)
    }

    async fn remove(&self, path: &str) -> Result<(), FsError> {
        let rel = self.rel(path)?;
        if rel == "." {
            return Err(FsError::Invalid("refusing to remove the workspace root; nothing was removed.".into()));
        }
        let out = self.script(REMOVE, &[&rel], None).await?;
        if out.code != 0 {
            return Err(self.fail(&out, path, &rel, "removed"));
        }
        Ok(())
    }
}

/// `fs-host`'s line slicing, shared so both worlds answer a ranged read identically.
fn slice_text(rel: &str, bytes: Vec<u8>, range: Option<(u32, u32)>) -> Result<ReadResult, FsError> {
    let version = FileVersion::of(&bytes);
    let text = String::from_utf8(bytes).map_err(|e| {
        FsError::Invalid(format!("`{rel}` is not UTF-8 text ({} bytes); nothing was read. Use read_image for images.", e.as_bytes().len()))
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

/// `fs-host`'s grep scan: skip big, binary and non-UTF-8 files; one hit per matching line.
fn scan(re: &regex::Regex, rel: &str, bytes: &[u8], hits: &mut Vec<GrepHit>) {
    if bytes.len() > GREP_MAX_FILE || bytes.iter().take(8000).any(|b| *b == 0) {
        return;
    }
    let Ok(text) = std::str::from_utf8(bytes) else { return };
    for (i, line) in text.lines().enumerate() {
        if hits.len() >= MAX_HITS {
            return;
        }
        if re.is_match(line) {
            hits.push(GrepHit { path: rel.to_string(), line: i as u32 + 1, text: line.chars().take(GREP_LINE_CHARS).collect() });
        }
    }
}

fn split_on<'a>(hay: &'a [u8], sep: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + sep.len() <= hay.len() {
        if hay[i] == sep[0] && hay[i..].starts_with(sep) {
            parts.push(&hay[start..i]);
            i += sep.len();
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(&hay[start..]);
    parts
}

/// One ignore file the remote sent: its root-relative directory (`""` = root) and rules.
#[derive(Debug, Clone, PartialEq)]
struct IgnoreFile {
    dir: String,
    kind: IgnoreKind,
    text: String,
}

/// Precedence within one directory, highest first (as the `ignore` crate's walker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum IgnoreKind {
    Ignore,
    GitIgnore,
    /// `.git/info/exclude` at the root, below everything else.
    Exclude,
}

#[derive(Debug, Default)]
struct Listing {
    /// Canonical base, root-relative (`.` for the root).
    base: String,
    is_dir: bool,
    /// `(kind, path under base)`: `f` file ≤ 16 MiB, `b` bigger, `l` link to an in-root file.
    files: Vec<(u8, String)>,
    ignores: Vec<IgnoreFile>,
}

impl Listing {
    fn parse(bytes: &[u8], boundary: &[u8]) -> Option<Listing> {
        let parts = split_on(bytes, boundary);
        let mut fields = parts.first()?.split(|b| *b == 0);
        let base = String::from_utf8_lossy(fields.next()?).into_owned();
        let is_dir = match fields.next()? {
            b"D" => true,
            b"F" => false,
            _ => return None,
        };
        let mut files = Vec::new();
        while let (Some(kind), Some(path)) = (fields.next(), fields.next()) {
            let [k] = kind else { continue };
            let path = String::from_utf8_lossy(path);
            files.push((*k, path.trim_start_matches("./").to_string()));
        }
        let mut ignores = Vec::new();
        for part in parts.iter().skip(1) {
            let mut it = part.splitn(3, |b| *b == 0);
            let (Some(kind), Some(path), Some(text)) = (it.next(), it.next(), it.next()) else { continue };
            let text = String::from_utf8_lossy(text).into_owned();
            match kind {
                b"x" => ignores.push(IgnoreFile { dir: String::new(), kind: IgnoreKind::Exclude, text }),
                b"g" => {
                    let path = String::from_utf8_lossy(path);
                    let path = path.trim_start_matches('/');
                    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
                    let dir = if dir == "." { "" } else { dir.trim_start_matches("./") };
                    let kind = if name == ".ignore" { IgnoreKind::Ignore } else { IgnoreKind::GitIgnore };
                    ignores.push(IgnoreFile { dir: dir.to_string(), kind, text });
                }
                _ => {}
            }
        }
        Some(Listing { base: if base.is_empty() { ".".into() } else { base }, is_dir, files, ignores })
    }

    /// Files that survive the ignore rules, in `fs-host`'s walk order (sorted per directory),
    /// as `(kind, path under base)`.
    fn walked(&self, ignores: &Ignores) -> Vec<(u8, String)> {
        let mut kept: Vec<(u8, String)> = self.files.iter().filter(|(_, under)| !ignores.skips(&self.base, under)).cloned().collect();
        kept.sort_by(|a, b| a.1.split('/').cmp(b.1.split('/')));
        kept
    }
}

/// The ignore rules of one walk, matched the way the `ignore` crate's walker does: deeper
/// directories override shallower ones, `.ignore` overrides `.gitignore`, and nothing under an
/// ignored directory is visited.
struct Ignores {
    by_dir: BTreeMap<String, Vec<(IgnoreKind, Gitignore)>>,
}

impl Ignores {
    fn build(files: &[IgnoreFile]) -> Ignores {
        let mut by_dir: BTreeMap<String, Vec<(IgnoreKind, Gitignore)>> = BTreeMap::new();
        for f in files {
            let mut b = GitignoreBuilder::new(virtual_path(&f.dir));
            for line in f.text.lines() {
                if b.add_line(None, line).is_err() {
                    // git skips a pattern it cannot parse; so does the walker.
                    continue;
                }
            }
            if let Ok(g) = b.build() {
                by_dir.entry(f.dir.clone()).or_default().push((f.kind, g));
            }
        }
        for v in by_dir.values_mut() {
            v.sort_by_key(|(k, _)| *k);
        }
        Ignores { by_dir }
    }

    /// `rel` (root-relative) is ignored by the rules of its ancestors.
    fn matched(&self, rel: &str, is_dir: bool) -> bool {
        let abs = virtual_path(rel);
        let comps: Vec<&str> = rel.split('/').collect();
        for depth in (0..comps.len()).rev() {
            let dir = comps[..depth].join("/");
            for (_, g) in self.by_dir.get(&dir).map(Vec::as_slice).unwrap_or(&[]) {
                match g.matched(&abs, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
        }
        false
    }

    /// `under` (relative to `base`) is skipped: it, or a directory between `base` and it, is ignored.
    fn skips(&self, base: &str, under: &str) -> bool {
        let comps: Vec<&str> = under.split('/').collect();
        for depth in 1..comps.len() {
            if self.matched(&join_rel(base, &comps[..depth].join("/")), true) {
                return true;
            }
        }
        self.matched(&join_rel(base, under), false)
    }
}

fn virtual_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from("/");
    if !rel.is_empty() && rel != "." {
        p.push(Path::new(rel));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(dir: &str, kind: IgnoreKind, text: &str) -> IgnoreFile {
        IgnoreFile { dir: dir.into(), kind, text: text.into() }
    }

    #[test]
    fn deeper_rules_and_dot_ignore_win_and_ignored_directories_are_not_entered() {
        let rules = Ignores::build(&[
            ig("", IgnoreKind::GitIgnore, "*.log\ntarget/\n"),
            ig("", IgnoreKind::Exclude, "secret.txt\n"),
            ig("sub", IgnoreKind::GitIgnore, "!keep.log\n"),
            ig("sub", IgnoreKind::Ignore, "keep.log\n"),
            ig("other", IgnoreKind::GitIgnore, "!keep.log\n"),
        ]);
        assert!(rules.skips(".", "a.log"));
        assert!(rules.skips(".", "target/debug/x.rs"), "nothing under an ignored directory");
        assert!(rules.skips(".", "secret.txt"));
        assert!(rules.skips(".", "sub/keep.log"), ".ignore beats .gitignore in the same directory");
        assert!(!rules.skips(".", "other/keep.log"), "a deeper whitelist beats the root rule");
        assert!(!rules.skips(".", "src/main.rs"));
        assert!(rules.skips("sub", "x.log"), "a base below the root still sees the root's rules");
    }

    #[test]
    fn a_walk_listing_parses_records_and_ignore_blocks() {
        let b = b"--aho-B";
        let mut raw = b"sub\0D\0f\0./a.txt\0b\0./big.bin\0l\0./link\0".to_vec();
        raw.extend_from_slice(b);
        raw.extend_from_slice(b"g\0sub/.gitignore\0*.log\n");
        raw.extend_from_slice(b);
        raw.extend_from_slice(b"g\0/.ignore\0x\n");
        raw.extend_from_slice(b);
        raw.extend_from_slice(b"x\0\0secret\n");
        raw.extend_from_slice(b);
        let l = Listing::parse(&raw, b).unwrap();
        assert_eq!(l.base, "sub");
        assert!(l.is_dir);
        assert_eq!(l.files, vec![(b'f', "a.txt".into()), (b'b', "big.bin".into()), (b'l', "link".into())]);
        assert_eq!(
            l.ignores,
            vec![ig("sub", IgnoreKind::GitIgnore, "*.log\n"), ig("", IgnoreKind::Ignore, "x\n"), ig("", IgnoreKind::Exclude, "secret\n"),]
        );
        assert!(Listing::parse(b"only-garbage", b).is_none());
    }

    #[test]
    fn walk_order_matches_a_sorted_directory_walk() {
        let l = Listing { base: ".".into(), is_dir: true, files: vec![(b'f', "a.txt".into()), (b'f', "a/b".into())], ignores: vec![] };
        let order: Vec<String> = l.walked(&Ignores::build(&[])).into_iter().map(|(_, p)| p).collect();
        assert_eq!(order, vec!["a/b".to_string(), "a.txt".to_string()]);
    }

    #[test]
    fn boundary_split_keeps_every_byte_between_boundaries() {
        assert_eq!(split_on(b"XXa\0bXXc\0\0dXX", b"XX"), vec![&b""[..], b"a\0b", b"c\0\0d", b""]);
        assert_eq!(split_on(b"no boundary", b"XX"), vec![&b"no boundary"[..]]);
    }

    /// Runs a remote fs script with the local `/bin/sh`, the way the remote runs it.
    fn sh_script(body: &str, root: &Path, args: &[&str], stdin: &[u8]) -> i32 {
        use std::io::Write;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{PRELUDE}{body}"))
            .arg("aho-ssh")
            .arg(root)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        child.wait().unwrap().code().unwrap()
    }

    #[test]
    fn a_transfer_cut_short_never_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("f.txt"), "original\n").unwrap();
        // The guard PREP would have produced for the current content.
        let out = std::process::Command::new("/bin/sh").arg("-c").arg("cksum < f.txt").current_dir(&root).output().unwrap();
        let guard = format!("F {}", String::from_utf8_lossy(&out.stdout).trim_end());
        assert_eq!(sh_script(COMMIT, &root, &["f.txt", &guard, "n1", "13"], b"repla"), 21, "5 of 13 bytes arrived");
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "original\n");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "the temp file is gone");
        assert_eq!(sh_script(COMMIT, &root, &["f.txt", &guard, "n2", "13"], b"replacement\n\n"), 0);
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "replacement\n\n");
    }

    #[test]
    fn ranged_reads_slice_like_the_host_world() {
        let r = slice_text("f", b"a\nb\nc\n".to_vec(), Some((2, 9))).unwrap();
        assert_eq!((r.text.as_str(), r.total_lines, r.partial), ("b\nc\n", 3, true));
        assert!(matches!(slice_text("f", b"a\n".to_vec(), Some((3, 4))), Err(FsError::Invalid(_))));
        assert!(matches!(slice_text("f", vec![0xff, 0xfe], None), Err(FsError::Invalid(_))));
    }
}
