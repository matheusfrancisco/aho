//! The I/O boundary for configuration: process environment, the three config files, `PATH`
//! probing, and the one write path (`apply`). Nothing else in the workspace should read these.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use aho_config::{FileSnapshot, Layer};
use aho_types::Value;

use crate::ComposeError;

/// `AHO_CRED_<NAME>` marks credential `<name>` present without making it a config key.
pub const CRED_ENV_PREFIX: &str = "AHO_CRED_";

/// Everything the operator side contributes, captured once.
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub project: PathBuf,
    pub home: PathBuf,
    pub profile: Option<String>,
    pub sets: Vec<(String, String)>,
    pub disable: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub strict: bool,
    /// Build features (`cfg!` flags) the binary was compiled with; `Requirement::Feature` checks these.
    pub features: BTreeSet<String>,
    /// Config file texts, captured by [`Inputs::capture_files`]; `load` never opens config files itself.
    pub files: Vec<FileSnapshot>,
}

impl Inputs {
    /// Empty inputs rooted at `project` with `home` as the operator's home. No I/O.
    pub fn new(project: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Inputs {
        Inputs { project: project.into(), home: home.into(), ..Inputs::default() }
    }

    /// The only place that reads the process environment: `HOME`, `PATH`, `AHO_*`, credential vars.
    #[allow(clippy::disallowed_methods, reason = "authority: compose-capture-env")]
    pub fn from_process(
        project: PathBuf,
        sets: Vec<(String, String)>,
        disable: Vec<String>,
        profile: Option<String>,
        strict: bool,
    ) -> Result<Inputs, ComposeError> {
        let env: BTreeMap<String, String> =
            std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
        let home = env.get("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).ok_or(ComposeError::NoHome)?;
        let mut inputs = Inputs { project, home, profile, sets, disable, env, strict, features: BTreeSet::new(), files: Vec::new() };
        inputs.capture_files()?;
        Ok(inputs)
    }

    pub fn home_config(&self) -> PathBuf {
        self.home.join(".aho").join("config.toml")
    }

    pub fn project_config(&self) -> PathBuf {
        self.project.join("aho.toml")
    }

    pub fn profile_path(&self, name: &str) -> PathBuf {
        self.home.join(".aho").join("profiles").join(format!("{name}.toml"))
    }

    /// Reads the profile file (if named and present), `~/.aho/config.toml`, and `./aho.toml` into
    /// `files`, replacing any earlier capture of those three layers. A named profile with neither a
    /// file nor a built-in fragment is refused by `load`, which knows the composition's built-ins.
    pub fn capture_files(&mut self) -> Result<(), ComposeError> {
        self.files.retain(|f| !matches!(f.layer, Layer::Profile(_) | Layer::FileHome | Layer::FileProject));
        if let Some(name) = self.profile.clone() {
            validate_name("profile", &name)?;
            let path = self.profile_path(&name);
            if let Some(text) = read_optional(&path)? {
                self.files.push(FileSnapshot { path: path.display().to_string(), layer: Layer::Profile(name), text });
            }
        }
        for (path, layer) in [(self.home_config(), Layer::FileHome), (self.project_config(), Layer::FileProject)] {
            if let Some(text) = read_optional(&path)? {
                self.files.push(FileSnapshot { path: path.display().to_string(), layer, text });
            }
        }
        Ok(())
    }

    /// Adds a frozen loadout (`loadouts::load`) as an extra layer.
    pub fn with_layer(mut self, layer: FileSnapshot) -> Inputs {
        self.files.push(layer);
        self
    }
}

pub(crate) fn validate_name(what: &'static str, name: &str) -> Result<(), ComposeError> {
    let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Ok(()) } else { Err(ComposeError::BadName { what, name: name.to_string() }) }
}

#[allow(clippy::disallowed_methods, reason = "authority: compose-read-config")]
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>, ComposeError> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ComposeError::Io { path: path.to_path_buf(), err: e.to_string() }),
    }
}

/// Writes `text` next to `path` and renames it into place so readers never see half a file.
#[allow(clippy::disallowed_methods, reason = "authority: compose-write-config")]
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// `which` for exactly the names the manifests mention. Empty `PATH` entries are skipped rather
/// than meaning "current directory", so the probe never depends on where aho was launched.
pub(crate) fn probe_binaries(needed: &BTreeSet<&'static str>, path_var: Option<&str>) -> BTreeSet<String> {
    let dirs: Vec<PathBuf> = path_var.map(|p| std::env::split_paths(p).filter(|d| !d.as_os_str().is_empty()).collect()).unwrap_or_default();
    needed.iter().filter(|b| find_executable(b, &dirs).is_some()).map(|b| b.to_string()).collect()
}

fn find_executable(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: compose-probe-unix")]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
#[allow(clippy::disallowed_methods, reason = "authority: compose-probe-other")]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// The most specific existing layer file: `./aho.toml` if present, else `~/.aho/config.toml`.
#[allow(clippy::disallowed_methods, reason = "authority: compose-apply-target")]
pub fn apply_target(project: &Path, home: &Path) -> PathBuf {
    let project_file = project.join("aho.toml");
    if project_file.is_file() { project_file } else { home.join(".aho").join("config.toml") }
}

/// `aho apply` / `aho set`: writes `key = value` into `file` with comments and layout kept.
/// Returns the file text before and after (empty before when the file did not exist).
pub fn apply(file: &Path, key: &str, value: Value) -> Result<(String, String), ComposeError> {
    let old = read_optional(file)?.unwrap_or_default();
    let new =
        aho_config::apply_to_file(&old, key, &value).map_err(|e| ComposeError::Apply { path: file.to_path_buf(), err: e.to_string() })?;
    write_atomic(file, &new).map_err(|err| ComposeError::Apply { path: file.to_path_buf(), err })?;
    Ok((old, new))
}
