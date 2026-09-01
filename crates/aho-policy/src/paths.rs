//! Lexical path handling shared by the shell classifier and the fs protected-path check (12).
//! Nothing here touches the filesystem: `~`, `$HOME` and `..` are resolved as text.

/// Where the call runs: the workspace root and the operator's home, both normalized absolute.
#[derive(Clone, Debug)]
pub(crate) struct Places {
    pub workspace: Option<String>,
    pub home: Option<String>,
}

impl Places {
    pub fn new(workspace: &str, home: &str) -> Places {
        let norm = |s: &str| if s.starts_with('/') { Some(normalize(s)) } else { None };
        Places { workspace: norm(workspace).filter(|w| w != "/"), home: norm(home).filter(|h| h != "/") }
    }
}

/// Collapses `//`, `.` and `..` of an absolute path; `..` at the root stays at the root.
pub(crate) fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    if out.is_empty() { "/".to_string() } else { format!("/{}", out.join("/")) }
}

/// `path` is `dir` or below it (both normalized).
pub(crate) fn within(path: &str, dir: &str) -> bool {
    dir == "/" || path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

pub(crate) fn strictly_within(path: &str, dir: &str) -> bool {
    path != dir && within(path, dir)
}

/// Joins a relative path onto `base` and normalizes. Absolute `rel` wins.
pub(crate) fn join(base: &str, rel: &str) -> String {
    if rel.starts_with('/') { normalize(rel) } else { normalize(&format!("{base}/{rel}")) }
}

/// Expands a leading `~`, `~/…`, `~user/…`, `~+`. `Err` when the home it names is unknown.
pub(crate) fn expand_tilde(word: &str, home: Option<&str>, cwd: Option<&str>) -> Result<String, &'static str> {
    let Some(rest) = word.strip_prefix('~') else { return Ok(word.to_string()) };
    let (name, tail) = match rest.find('/') {
        Some(k) => (&rest[..k], &rest[k..]),
        None => (rest, ""),
    };
    match name {
        "" => home.map(|h| format!("{h}{tail}")).ok_or("the home directory is not known"),
        "+" => cwd.map(|c| format!("{c}{tail}")).ok_or("the working directory is not known"),
        "-" => Err("the previous directory is not known"),
        user => {
            let base = home.and_then(|h| h.rsplit_once('/').map(|(p, _)| p)).filter(|p| *p == "/Users" || *p == "/home").unwrap_or("/home");
            Ok(format!("{base}/{user}{tail}"))
        }
    }
}

const SYSTEM_ROOTS: &[&str] =
    &["etc", "usr", "bin", "sbin", "lib", "lib32", "lib64", "System", "Library", "var", "boot", "opt", "proc", "sys"];
const CREDENTIAL_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", ".kube", ".docker", ".config/gcloud"];
const SINKS: &[&str] = &["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/stdin", "/dev/tty"];
const TEMP_ROOTS: &[&str] = &["/tmp", "/var/tmp", "/var/folders"];
const PROTECTED_COMPONENTS: &[&str] = &[".git", ".aho"];

/// What a (lexically resolved) path is, from most to least dangerous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Zone {
    Root,
    Device,
    Credential,
    Home,
    System,
    /// `/dev/null` and friends: writing there destroys nothing.
    Sink,
    Temp,
    TempRoot,
    WorkspaceRoot,
    Workspace,
    /// `.git/` or `.aho/` internals.
    Protected,
    Outside,
}

impl Zone {
    pub fn describe(self) -> &'static str {
        match self {
            Zone::Root => "the filesystem root",
            Zone::Device => "a device node",
            Zone::Credential => "a credential directory",
            Zone::Home => "a home directory",
            Zone::System => "a system directory",
            Zone::Sink => "a null device",
            Zone::Temp => "a temp directory",
            Zone::TempRoot => "the temp directory itself",
            Zone::WorkspaceRoot => "the whole workspace",
            Zone::Workspace => "inside the workspace",
            Zone::Protected => "repository or harness internals (.git/ or .aho/)",
            Zone::Outside => "outside the workspace",
        }
    }
}

/// macOS keeps `/etc`, `/var`, `/tmp` under `/private`; classify them as the short form.
fn unprivate(path: &str) -> &str {
    for short in ["/etc", "/var", "/tmp"] {
        if let Some(rest) = path.strip_prefix("/private")
            && within(rest, short)
        {
            return rest;
        }
    }
    path
}

fn is_home_like(path: &str, places: &Places) -> bool {
    if places.home.as_deref() == Some(path) {
        return true;
    }
    if matches!(path, "/root" | "/var/root" | "/Users" | "/home") {
        return true;
    }
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    comps.len() == 2 && (comps[0] == "Users" || comps[0] == "home")
}

fn is_credential(path: &str, places: &Places) -> bool {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let mut homes: Vec<String> = vec!["/root".into(), "/var/root".into()];
    if let Some(h) = &places.home {
        homes.push(h.clone());
    }
    if comps.len() >= 2 && (comps[0] == "Users" || comps[0] == "home") {
        homes.push(format!("/{}/{}", comps[0], comps[1]));
    }
    homes.iter().any(|h| CREDENTIAL_DIRS.iter().any(|c| within(path, &format!("{h}/{c}"))))
}

fn has_protected_component(path: &str) -> bool {
    path.split('/').any(|c| PROTECTED_COMPONENTS.contains(&c))
}

/// Classifies a normalized absolute path. `contents` = the target is a glob over the
/// directory's children (`/*`, `~/*`, `./*`), which is as dangerous as the parent for
/// protected parents and harmless-as-a-child for the workspace and temp roots.
pub(crate) fn zone(path: &str, contents: bool, places: &Places) -> Zone {
    let p = unprivate(path);
    if p == "/" {
        return Zone::Root;
    }
    if within(p, "/dev") {
        if !contents && (SINKS.contains(&p) || strictly_within(p, "/dev/fd")) {
            return Zone::Sink;
        }
        return Zone::Device;
    }
    if is_credential(p, places) {
        return Zone::Credential;
    }
    if is_home_like(p, places) {
        return Zone::Home;
    }
    if let Some(ws) = &places.workspace {
        if path == ws {
            return if contents { Zone::Workspace } else { Zone::WorkspaceRoot };
        }
        if strictly_within(path, ws) {
            return if has_protected_component(&path[ws.len()..]) { Zone::Protected } else { Zone::Workspace };
        }
    }
    if let Some(t) = TEMP_ROOTS.iter().find(|t| within(p, t)) {
        return if p == *t && !contents { Zone::TempRoot } else { Zone::Temp };
    }
    if has_protected_component(p) {
        return Zone::Protected;
    }
    let first = p.split('/').find(|c| !c.is_empty()).unwrap_or("");
    if SYSTEM_ROOTS.contains(&first) {
        return Zone::System;
    }
    Zone::Outside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> Places {
        Places::new("/Users/me/proj", "/Users/me")
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(normalize("/a/b/../c/./d/"), "/a/c/d");
        assert_eq!(normalize("/../.."), "/");
        assert_eq!(join("/ws", "spec/../src/main.rs"), "/ws/src/main.rs");
        assert_eq!(join("/ws", "/etc"), "/etc");
    }

    #[test]
    fn tilde_forms() {
        assert_eq!(expand_tilde("~/.ssh", Some("/Users/me"), None), Ok("/Users/me/.ssh".into()));
        assert_eq!(expand_tilde("~bob/x", Some("/Users/me"), None), Ok("/Users/bob/x".into()));
        assert_eq!(expand_tilde("~bob", Some("/root"), None), Ok("/home/bob".into()));
        assert!(expand_tilde("~", None, None).is_err());
        assert_eq!(expand_tilde("a~b", None, None), Ok("a~b".into()));
    }

    #[test]
    fn zones() {
        let p = places();
        assert_eq!(zone("/", false, &p), Zone::Root);
        assert_eq!(zone("/dev/sda", false, &p), Zone::Device);
        assert_eq!(zone("/dev/null", false, &p), Zone::Sink);
        assert_eq!(zone("/Users/me", false, &p), Zone::Home);
        assert_eq!(zone("/Users/me", true, &p), Zone::Home);
        assert_eq!(zone("/Users/other", false, &p), Zone::Home);
        assert_eq!(zone("/Users/me/.ssh/id_ed25519", false, &p), Zone::Credential);
        assert_eq!(zone("/Users/me/.config/gcloud", false, &p), Zone::Credential);
        assert_eq!(zone("/etc/hosts", false, &p), Zone::System);
        assert_eq!(zone("/private/etc/hosts", false, &p), Zone::System);
        assert_eq!(zone("/tmp/x", false, &p), Zone::Temp);
        assert_eq!(zone("/private/var/folders/ab/T/x", false, &p), Zone::Temp);
        assert_eq!(zone("/tmp", false, &p), Zone::TempRoot);
        assert_eq!(zone("/tmp", true, &p), Zone::Temp);
        assert_eq!(zone("/Users/me/proj", false, &p), Zone::WorkspaceRoot);
        assert_eq!(zone("/Users/me/proj/target", false, &p), Zone::Workspace);
        assert_eq!(zone("/Users/me/proj/.git/hooks/pre-commit", false, &p), Zone::Protected);
        assert_eq!(zone("/Users/me/other", false, &p), Zone::Outside);
        assert_eq!(zone("/some/other", false, &p), Zone::Outside);
    }

    #[test]
    fn workspace_under_a_system_root_is_still_the_workspace() {
        let p = Places::new("/var/folders/x/T/ws", "/Users/me");
        assert_eq!(zone("/var/folders/x/T/ws/a", false, &p), Zone::Workspace);
        let p = Places::new("/opt/proj", "/Users/me");
        assert_eq!(zone("/opt/proj/build", false, &p), Zone::Workspace);
        assert_eq!(zone("/opt/other", false, &p), Zone::System);
    }
}
