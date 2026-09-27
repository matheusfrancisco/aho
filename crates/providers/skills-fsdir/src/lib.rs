//! SKILL.md discovery from directories.
//!
//! Roots, lowest rank first (a lower rank wins a duplicate name; the loser is `Shadowed`):
//! 0. `<project>/.aho/skills` (workspace)
//! 1. `<project>/.agents/skills` (workspace)
//! 2. `<home>/.aho/skills` (user)
//! 3. each entry of `skills.dirs`, in order (relative entries resolve against the project;
//!    workspace origin when inside the project, user otherwise)
//!
//! A skill is `<root>/<name>/SKILL.md` (its directory's other files are its resources) or
//! `<root>/<name>.md`. Hidden entries and `README.md` are skipped; symlinks are followed.
//!
//! * [`FsSkills`] is the `aho_cap::Discoverer`: one `Discovered` row per winning skill
//!   (`skill.<name>`, `Kind::Template`, origin Workspace/UserInstalled) and one Broken row per
//!   file that failed to parse, with the parser's reason. The discoverer sees only `Env`, so it
//!   scans roots 0–2 plus the dirs it was constructed with.
//! * [`SkillsFsdir`] is the capability. It scans every root (including `skills.dirs`) and
//!   publishes the active templates as the service `"skills"` (`Arc<Vec<Template>>`) and every
//!   skill's state as `"skills.status"`. Discovered skills are `Unlocked` until the operator
//!   enables them: `skills.<name>.enabled = true` → Active, `false` or
//!   `--disable skill.<name>` → Disabled.
//!
//! Project and home default to the directory aho was started in and `$HOME`; `skills.project_dir`
//! / `skills.home_dir` override them.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_policy::Untrusted;
use aho_skills::{SkillState, SkillStatus, Template, skill_id};
use aho_types::*;
use serde_json::json;

/// Larger files are reported Broken rather than read.
pub const MAX_SKILL_BYTES: u64 = 1 << 20;
/// Resource listing bounds per skill directory.
pub const MAX_RESOURCES: usize = 200;
const MAX_RESOURCE_DEPTH: usize = 4;

#[derive(Clone, Debug, PartialEq)]
pub struct Root {
    pub dir: PathBuf,
    pub origin: CapOrigin,
    pub rank: usize,
}

/// The ordered roots for a project/home pair plus custom dirs.
pub fn roots(project: &Path, home: Option<&Path>, extra: &[String]) -> Vec<Root> {
    let mut out = vec![
        Root { dir: project.join(".aho/skills"), origin: CapOrigin::Workspace, rank: 0 },
        Root { dir: project.join(".agents/skills"), origin: CapOrigin::Workspace, rank: 1 },
    ];
    if let Some(h) = home {
        out.push(Root { dir: h.join(".aho/skills"), origin: CapOrigin::UserInstalled, rank: 2 });
    }
    for (i, d) in extra.iter().enumerate() {
        let p = PathBuf::from(d);
        let dir = if p.is_absolute() { p } else { project.join(p) };
        let origin = if dir.starts_with(project) { CapOrigin::Workspace } else { CapOrigin::UserInstalled };
        out.push(Root { dir, origin, rank: 3 + i });
    }
    out
}

#[derive(Clone, Debug)]
pub struct Found {
    pub template: Template,
    pub dropped: Vec<String>,
    pub rank: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrokenSkill {
    pub id: String,
    pub path: String,
    pub origin: CapOrigin,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shadowed {
    pub name: String,
    pub path: String,
    pub origin: CapOrigin,
    pub by: String,
}

#[derive(Clone, Debug, Default)]
pub struct Scan {
    /// Winners, in root order then name order.
    pub found: Vec<Found>,
    pub broken: Vec<BrokenSkill>,
    pub shadowed: Vec<Shadowed>,
}

fn display(p: &Path) -> String {
    p.display().to_string()
}

/// Reads every root. Missing roots are skipped; unreadable entries become Broken rows.
#[allow(clippy::disallowed_methods, reason = "authority: skills-fsdir-scan")]
pub fn scan(roots: &[Root]) -> Scan {
    let mut out = Scan::default();
    let mut winners: BTreeMap<String, String> = BTreeMap::new();
    let mut ordered: Vec<&Root> = roots.iter().collect();
    ordered.sort_by_key(|r| r.rank);
    for root in ordered {
        let entries = match std::fs::read_dir(&root.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                out.broken.push(BrokenSkill {
                    id: format!("skill-root:{}", display(&root.dir)),
                    path: display(&root.dir),
                    origin: root.origin,
                    error: format!("{}: the skills directory could not be read ({e}); its skills were not loaded", display(&root.dir)),
                });
                continue;
            }
        };
        let mut names: Vec<(String, PathBuf)> =
            entries.filter_map(Result::ok).map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect();
        names.sort();
        for (fname, path) in names {
            if fname.starts_with('.') || fname.eq_ignore_ascii_case("README.md") {
                continue;
            }
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let (file, base_dir, stem) = if meta.is_dir() {
                let f = path.join("SKILL.md");
                if !f.is_file() {
                    continue;
                }
                (f, Some(path.clone()), fname.clone())
            } else if meta.is_file() && fname.ends_with(".md") {
                (path.clone(), None, fname.trim_end_matches(".md").to_string())
            } else {
                continue;
            };
            let file_s = display(&file);
            let broken = |error: String| BrokenSkill { id: skill_id(&stem), path: file_s.clone(), origin: root.origin, error };
            let text = match std::fs::metadata(&file) {
                Ok(m) if m.len() > MAX_SKILL_BYTES => {
                    out.broken.push(broken(format!(
                        "{file_s}: {} bytes is larger than the {MAX_SKILL_BYTES}-byte limit; the skill was not loaded",
                        m.len()
                    )));
                    continue;
                }
                _ => match std::fs::read_to_string(&file) {
                    Ok(t) => t,
                    Err(e) => {
                        out.broken.push(broken(format!("{file_s}: could not be read ({e}); the skill was not loaded")));
                        continue;
                    }
                },
            };
            match aho_skills::parse(Untrusted::new(text), &file_s) {
                Err(e) => out.broken.push(broken(e.to_string())),
                Ok((mut t, dropped)) => {
                    if let Some(by) = winners.get(&t.name) {
                        out.shadowed.push(Shadowed { name: t.name.clone(), path: file_s, origin: root.origin, by: by.clone() });
                        continue;
                    }
                    t.origin = root.origin;
                    match &base_dir {
                        Some(d) => {
                            t.base_dir = display(d);
                            t.resources = resources(d);
                        }
                        None => t.base_dir = display(&root.dir),
                    }
                    winners.insert(t.name.clone(), file_s);
                    out.found.push(Found { template: t, dropped, rank: root.rank });
                }
            }
        }
    }
    out
}

/// Files under a skill directory (relative, sorted, bounded), excluding `SKILL.md` and hidden entries.
#[allow(clippy::disallowed_methods, reason = "authority: skills-fsdir-resources")]
fn resources(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let p = e.path();
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            if meta.is_dir() {
                if depth + 1 < MAX_RESOURCE_DEPTH {
                    stack.push((p, depth + 1));
                }
            } else if let Ok(rel) = p.strip_prefix(dir) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if rel != "SKILL.md" {
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    out.truncate(MAX_RESOURCES);
    out
}

fn enable_fix(name: &str) -> String {
    format!("aho enable {}", skill_id(name))
}

/// The inventory rows for a scan.
pub fn discovered(scan: &Scan) -> Vec<Discovered> {
    let mut rows = Vec::new();
    for f in &scan.found {
        let t = &f.template;
        let mut doc = t.description.clone();
        if !f.dropped.is_empty() {
            doc.push_str(&format!(" (policy keys dropped from its frontmatter: {})", f.dropped.join(", ")));
        }
        doc.push_str(&format!("; enable: `{}`", enable_fix(&t.name)));
        rows.push(Discovered {
            id: skill_id(&t.name),
            kind: Kind::Template,
            origin: t.origin,
            source_path: Some(if t.base_dir.is_empty() { t.path.clone() } else { t.base_dir.clone() }),
            classes: ConsequenceSet::of(&[Consequence::ReadOnly]),
            requires: vec![SkillsFsdir::ID_.name.to_string()],
            broken: None,
            doc,
        });
    }
    for b in &scan.broken {
        rows.push(Discovered {
            id: b.id.clone(),
            kind: Kind::Template,
            origin: b.origin,
            source_path: Some(b.path.clone()),
            classes: ConsequenceSet::of(&[Consequence::ReadOnly]),
            requires: vec![SkillsFsdir::ID_.name.to_string()],
            broken: Some(b.error.clone()),
            doc: String::new(),
        });
    }
    rows
}

/// The `Discoverer`: roots 0–2 from `Env`, plus the dirs given at construction.
#[derive(Clone, Debug, Default)]
pub struct FsSkills {
    extra: Vec<String>,
}

impl FsSkills {
    pub fn new() -> FsSkills {
        FsSkills::default()
    }
    pub fn with_dirs(dirs: Vec<String>) -> FsSkills {
        FsSkills { extra: dirs }
    }
}

impl Discoverer for FsSkills {
    fn discover(&self, env: &Env) -> Vec<Discovered> {
        let home = (!env.home.is_empty()).then(|| PathBuf::from(&env.home));
        discovered(&scan(&roots(Path::new(&env.project), home.as_deref(), &self.extra)))
    }
}

// ───────────────────────────── capability ─────────────────────────────

#[derive(Clone, Debug)]
pub struct SkillsConfig {
    pub project: PathBuf,
    pub home: Option<PathBuf>,
    pub dirs: Vec<String>,
    /// `skills.<name>.enabled`, with the origin of the winning value.
    pub switches: BTreeMap<String, (bool, String)>,
    /// `skill.<name>` ids named by `--disable` / `AHO_DISABLE` / `disable = [...]`, with origin.
    pub disabled: BTreeMap<String, String>,
}

impl Config for SkillsConfig {
    const PREFIX: &'static str = "skills";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new("skills", Ty::Table, None, "Per-skill switches: `skills.<name>.enabled`; discovered skills are opt-in."),
            ConfigKey::new(
                "skills.dirs",
                Ty::Array,
                Some(json!([])),
                "Extra skill roots, searched after `.aho/skills`, `.agents/skills`, `~/.aho/skills`.",
            ),
            ConfigKey::new(
                "skills.project_dir",
                Ty::String,
                None,
                "Project whose `.aho/skills` and `.agents/skills` are read; default: the directory aho was started in.",
            ),
            ConfigKey::new("skills.home_dir", Ty::String, None, "Home whose `.aho/skills` is read; default: `$HOME`."),
        ]
    }

    #[allow(clippy::disallowed_methods, reason = "authority: skills-fsdir-config")]
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let project = match l.str("skills.project_dir") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => std::env::current_dir().map_err(|_| ConfigError::MissingKey("skills.project_dir".into()))?,
        };
        let home = match l.str("skills.home_dir") {
            Some(h) if !h.is_empty() => Some(PathBuf::from(h)),
            _ => std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from),
        };
        let dirs = match l.get("skills.dirs") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or(ConfigError::Type { key: "skills.dirs".into(), expected: "an array of directory paths" })?,
            Some(_) => return Err(ConfigError::Type { key: "skills.dirs".into(), expected: "an array of directory paths" }),
        };
        let mut switches = BTreeMap::new();
        for (name, row) in l.table("skills") {
            let key = format!("skills.{name}.enabled");
            match row.get("enabled") {
                None => {}
                Some(Value::Bool(b)) => {
                    let origin = l.config.get(&key).map(|s| s.winner.render()).unwrap_or_else(|| key.clone());
                    switches.insert(name, (*b, origin));
                }
                Some(_) => return Err(ConfigError::Type { key, expected: "true or false" }),
            }
        }
        let disabled: BTreeMap<String, String> =
            l.disabled.iter().filter(|(id, _)| id.starts_with("skill.")).map(|(id, o)| (id.clone(), o.render())).collect();
        Ok(SkillsConfig { project, home, dirs, switches, disabled })
    }
}

/// The skill provider: publishes active templates (`"skills"`) and every skill's state (`"skills.status"`).
pub struct SkillsFsdir {
    active: Arc<Vec<Template>>,
    status: Arc<Vec<SkillStatus>>,
}

impl SkillsFsdir {
    /// State of every found / broken / shadowed skill under `cfg`'s switches.
    pub fn resolve(scan: Scan, cfg: &SkillsConfig) -> (Vec<Template>, Vec<SkillStatus>) {
        let mut active = Vec::new();
        let mut status = Vec::new();
        for f in scan.found {
            let t = f.template;
            let id = skill_id(&t.name);
            let state = if let Some(o) = cfg.disabled.get(&id) {
                SkillState::Disabled { why: o.clone() }
            } else {
                match cfg.switches.get(&t.name) {
                    Some((true, _)) => SkillState::Active,
                    Some((false, o)) => SkillState::Disabled { why: o.clone() },
                    None => SkillState::Unlocked { fix: enable_fix(&t.name) },
                }
            };
            status.push(SkillStatus {
                id,
                name: t.name.clone(),
                path: t.path.clone(),
                origin: t.origin,
                state: state.clone(),
                dropped: f.dropped,
            });
            if state == SkillState::Active {
                active.push(t);
            }
        }
        for b in scan.broken {
            let name = b.id.strip_prefix("skill.").unwrap_or(&b.id).to_string();
            status.push(SkillStatus {
                id: b.id,
                name,
                path: b.path,
                origin: b.origin,
                state: SkillState::Broken { error: b.error },
                dropped: vec![],
            });
        }
        for s in scan.shadowed {
            status.push(SkillStatus {
                id: skill_id(&s.name),
                name: s.name,
                path: s.path,
                origin: s.origin,
                state: SkillState::Shadowed { by: s.by },
                dropped: vec![],
            });
        }
        (active, status)
    }
}

impl Capability for SkillsFsdir {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = SkillsConfig;

    fn build(_: (), cfg: SkillsConfig) -> Result<Self, BuildError> {
        let found = scan(&roots(&cfg.project, cfg.home.as_deref(), &cfg.dirs));
        let (active, status) = SkillsFsdir::resolve(found, &cfg);
        Ok(SkillsFsdir { active: Arc::new(active), status: Arc::new(status) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.service(aho_skills::SERVICE, self.active.clone());
        r.service(aho_skills::STATUS_SERVICE, self.status.clone());
    }
}

declare_capability!(
    SkillsFsdir,
    "skills-fsdir",
    kind = Kind::Provider,
    provides = [aho_skills::SEAM],
    requires = [],
    tools = &[],
    doc = "SKILL.md skills from `.aho/skills`, `.agents/skills`, `~/.aho/skills` and `skills.dirs`; each skill is enabled with `skills.<name>.enabled`."
);

#[cfg(test)]
mod tests;
