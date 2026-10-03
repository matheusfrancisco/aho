//! Engineer side (a `Composition` value) meets operator side (layers) in one `load`.
//! The result is the loadout + plan that become log entries `0..k` and `PlanSealed`.
//!
//! I/O lives at the edges: [`Inputs::from_process`] / [`Inputs::capture_files`] read the
//! environment and the config files; [`load`] is pure over those captured inputs except for
//! probing `PATH` for the binaries the linked manifests name and running the composition's
//! discoverers. Lock files, saved loadouts, and `apply` are explicit file verbs.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

mod boot;
mod credentials;
mod diff;
mod inputs;
pub mod loadouts;
mod lock;

use std::collections::BTreeMap;
use std::path::PathBuf;

use aho_cap::{Composition, Discovered, Env, HandleKind, Manifest, Os, Plan, PlanError, PolicyInput};
use aho_config::{FileSnapshot, Layer, Loadout, ResolveError, ResolveInputs, Schema};
use aho_log::EntryKind;
use aho_types::*;

pub use boot::{WorldRefusal, with_boot, world_refusal};
pub use credentials::{CredentialOrigin, credential_name, credentials};
pub use diff::{CapFlip, ConfigChange, PlanDiff, plan_diff, render_plan_diff};
pub use inputs::{CRED_ENV_PREFIX, Inputs, apply, apply_target};
pub use lock::{LOCK_FILE, LockFile};

/// Preset used when no layer sets `policy.preset` (the core key's default says the same).
pub const DEFAULT_PRESET: &str = "workspace";

/// Everything `aho plan` prints and `aho run` seals into the log.
#[derive(Clone, Debug)]
pub struct Planned {
    pub loadout: Loadout,
    pub plan: Plan,
    pub env: Env,
    pub schema: Schema,
    /// The linked manifests the plan was computed from (activation entries need their hashes).
    pub manifests: Vec<&'static Manifest>,
}

#[derive(Debug)]
pub enum ComposeError {
    Resolve(ResolveError),
    Plan(PlanError),
    UnknownPreset {
        name: String,
        origin: String,
    },
    ProfileNotFound {
        name: String,
        path: PathBuf,
        /// The composition's built-in profile names, so the message can offer them.
        builtin: Vec<&'static str>,
    },
    BadName {
        what: &'static str,
        name: String,
    },
    NoHome,
    Io {
        path: PathBuf,
        err: String,
    },
    /// The resolved plan is not the one the operator pinned (`--plan <hash>` / `aho.lock`).
    Drift {
        expected: Hash,
        actual: Hash,
        causes: Vec<String>,
    },
    Lock {
        path: PathBuf,
        err: String,
    },
    Loadout {
        path: PathBuf,
        err: String,
    },
    Apply {
        path: PathBuf,
        err: String,
    },
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComposeError::Resolve(ResolveError::Parse { path, msg }) => {
                write!(f, "could not parse {path}: {msg}. Nothing was loaded; fix the file or move it aside.")
            }
            ComposeError::Resolve(ResolveError::Strict(ds)) => {
                let list: Vec<String> = ds.iter().map(|d| format!("{} ({})", d.detail, d.key)).collect();
                write!(
                    f,
                    "--strict refused {} diagnostic(s): {}. Nothing was started; fix them or drop --strict.",
                    ds.len(),
                    list.join("; ")
                )
            }
            ComposeError::Plan(e) => write!(f, "the capability plan is inconsistent: {e}. Nothing was started."),
            ComposeError::UnknownPreset { name, origin } => write!(
                f,
                "`policy.preset = {name}` ({origin}) names no preset. Nothing was started; use one of read-only, workspace, dev, lab, full, spec."
            ),
            ComposeError::ProfileNotFound { name, path, builtin } => {
                let offer = if builtin.is_empty() { "none".to_string() } else { builtin.join(", ") };
                write!(
                    f,
                    "profile `{name}` is not built in (built-in: {offer}) and does not exist at {}. Nothing was loaded; create the file or drop --profile.",
                    path.display()
                )
            }
            ComposeError::BadName { what, name } => {
                write!(f, "`{name}` is not a valid {what} name (letters, digits, `-` and `_` only). Nothing was read or written.")
            }
            ComposeError::NoHome => write!(f, "HOME is not set, so ~/.aho cannot be found. Nothing was loaded; set HOME."),
            ComposeError::Io { path, err } => write!(f, "could not read {}: {err}. Nothing was loaded.", path.display()),
            ComposeError::Drift { expected, actual, causes } => {
                write!(f, "the resolved plan {} differs from the pinned plan {}", actual.short(), expected.short())?;
                if !causes.is_empty() {
                    write!(f, " ({})", causes.join("; "))?;
                }
                write!(f, ". The session was not started; run `aho plan --lock` to accept the new plan, or restore the inputs.")
            }
            ComposeError::Lock { path, err } => {
                write!(f, "{}: {err}. Nothing was started; run `aho plan --lock` to write a fresh lock.", path.display())
            }
            ComposeError::Loadout { path, err } => write!(f, "{}: {err}. Nothing was changed.", path.display()),
            ComposeError::Apply { path, err } => write!(f, "could not write {}: {err}. The file was left unchanged.", path.display()),
        }
    }
}

impl std::error::Error for ComposeError {}

/// Assembles the `Schema` from every linked capability's `ConfigKey` rows plus the core keys.
pub fn schema_of(comp: &Composition) -> Schema {
    let mut s = Schema::default();
    for k in comp.config_keys() {
        s.add(k);
    }
    s
}

/// The layer stack handed to `aho_config::resolve`. Credential env vars (`AHO_CRED_*`) are not
/// config keys: they only mark a credential present, so they never reach the resolver.
/// With `--profile <name>`, the composition's built-in fragments of `<name>` go first at
/// `Layer::Profile(name)`, so the operator's `~/.aho/profiles/<name>.toml` (same rank, later)
/// wins key by key; `disable` lists from every layer still add up.
pub fn resolve_inputs(comp: &Composition, inputs: &Inputs) -> ResolveInputs {
    let mut files: Vec<FileSnapshot> = match &inputs.profile {
        Some(name) => comp
            .profiles()
            .iter()
            .filter(|p| p.name == name)
            .map(|p| FileSnapshot {
                path: format!("builtin:{}/profiles/{}.toml", p.bundle, p.name),
                layer: Layer::Profile(name.clone()),
                text: p.text.to_string(),
            })
            .collect(),
        None => Vec::new(),
    };
    files.extend(inputs.files.iter().cloned());
    ResolveInputs {
        sets: inputs.sets.clone(),
        env: inputs.env.iter().filter(|(k, _)| !k.starts_with(CRED_ENV_PREFIX)).map(|(k, v)| (k.clone(), v.clone())).collect(),
        files,
        bundle: comp.name.to_string(),
        bundle_defaults: comp.defaults().to_vec(),
        runtime: Vec::new(),
        disable: inputs.disable.clone(),
    }
}

/// `--profile <name>` must name a built-in profile or an existing `~/.aho/profiles/<name>.toml`.
fn check_profile(comp: &Composition, inputs: &Inputs) -> Result<(), ComposeError> {
    let Some(name) = &inputs.profile else { return Ok(()) };
    let builtin = comp.profiles().iter().any(|p| p.name == name);
    let file = inputs.files.iter().any(|f| matches!(&f.layer, Layer::Profile(n) if n == name));
    if builtin || file {
        return Ok(());
    }
    let mut names: Vec<&'static str> = comp.profiles().iter().map(|p| p.name).collect();
    names.sort_unstable();
    names.dedup();
    Err(ComposeError::ProfileNotFound { name: name.clone(), path: inputs.profile_path(name), builtin: names })
}

/// Resolve the layers, probe the environment, and plan twice: first with every class allowed to
/// learn which world binds, then with the preset's cut for that world.
pub fn load(comp: &Composition, inputs: &Inputs) -> Result<Planned, ComposeError> {
    check_profile(comp, inputs)?;
    let schema = schema_of(comp);
    let mut loadout = aho_config::resolve(&resolve_inputs(comp, inputs), &schema, inputs.strict).map_err(ComposeError::Resolve)?;
    credentials::redact(&mut loadout);
    // The model's limits from the catalog, ranked under every operator layer (`aho explain`).
    aho_llm::models::apply_catalog(&mut loadout);

    let manifests = comp.manifests();
    let needed = aho_cap::binaries_needed(&manifests);
    let env = Env {
        home: inputs.home.display().to_string(),
        project: inputs.project.display().to_string(),
        os: Os::current(),
        binaries: inputs::probe_binaries(&needed, inputs.env.get("PATH").map(String::as_str)),
        credentials: credentials(inputs, &loadout).into_keys().collect(),
        features: inputs.features.clone(),
    };
    let discovered: Vec<Discovered> = comp.discoverers().iter().flat_map(|d| d.discover(&env)).collect();

    let preset_name = match loadout.get("policy.preset") {
        None => DEFAULT_PRESET.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(ComposeError::UnknownPreset { name: other.to_string(), origin: preset_origin(&loadout) });
        }
    };
    let preset = aho_policy::preset(&preset_name)
        .ok_or_else(|| ComposeError::UnknownPreset { name: preset_name.clone(), origin: preset_origin(&loadout) })?;
    let name = preset.name().to_string();

    let probe =
        aho_cap::plan(&manifests, &loadout, &env, &PolicyInput { preset: name.clone(), allowed: ConsequenceSet::ALL }, discovered.clone())
            .map_err(ComposeError::Plan)?;
    let allowed = preset.cut(&probe.world);
    let plan = aho_cap::plan(&manifests, &loadout, &env, &PolicyInput { preset: name, allowed }, discovered).map_err(ComposeError::Plan)?;
    Ok(Planned { loadout, plan, env, schema, manifests })
}

fn preset_origin(l: &Loadout) -> String {
    l.config.get("policy.preset").map(|s| s.winner.render()).unwrap_or_else(|| "default".into())
}

/// Entries `0..k` of a new session: config, diagnostics, activations, the first policy epoch,
/// and the seal. Secret values are already digests in the loadout.
pub fn bootstrap_entries(p: &Planned) -> Vec<EntryKind> {
    let mut out = Vec::new();
    for (key, sourced) in &p.loadout.config {
        out.push(EntryKind::ConfigResolved { key: key.clone(), sourced: sourced.clone() });
    }
    for d in &p.loadout.diagnostics {
        out.push(EntryKind::LoadoutDiagnostic {
            lint: serde_name(&d.lint),
            severity: serde_name(&d.severity),
            key: d.key.clone(),
            detail: d.detail.clone(),
        });
    }
    for row in p.plan.active() {
        let Some(m) = p.manifests.iter().find(|m| m.id == row.id) else { continue };
        out.push(EntryKind::CapActivated {
            id: row.id.name.to_string(),
            manifest_hash: m.hash(),
            deps: m.handles.iter().map(|h| handle_name(*h)).collect(),
            config_hash: cap_config_hash(m, &p.loadout),
        });
    }
    // Discovered capabilities (skills, …) that are Active from the start enter the fold too, so a
    // later mutation diffs against the truth instead of reporting them freshly activated.
    for (id, state) in &p.plan.discovered_states {
        if matches!(state, aho_log::CapState::Active) {
            out.push(EntryKind::CapDelta {
                id: id.clone(),
                op: aho_log::CapDeltaOp::Activated,
                reason: "discovered, active at start".into(),
            });
        }
    }
    out.push(EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: p.plan.allowed, preset: p.plan.preset.clone(), approval_ref: None });
    out.push(EntryKind::PlanSealed { plan_hash: p.plan.hash, fingerprint: p.plan.fingerprint, schema_hash: p.schema.hash() });
    out
}

/// `aho run --plan <hash>`: refuse to start when the resolved plan is not the pinned one.
pub fn verify_against(expected: Hash, p: &Planned) -> Result<(), ComposeError> {
    if p.plan.hash == expected { Ok(()) } else { Err(ComposeError::Drift { expected, actual: p.plan.hash, causes: Vec::new() }) }
}

/// Handle kinds as they appear in `CapActivated.deps` (`FsRead`, `Egress(Sandbox)`, `sandbox`).
pub fn handle_name(h: HandleKind) -> String {
    match h {
        HandleKind::Custom(name) => name.to_string(),
        other => format!("{other:?}"),
    }
}

/// Digest of the values a capability's own keys (and its `caps.<id>.*` switches) resolved to.
pub fn cap_config_hash(m: &Manifest, loadout: &Loadout) -> Hash {
    let prefixes: Vec<String> =
        (m.config_keys)().into_iter().map(|k| k.path).chain(std::iter::once(format!("caps.{}", m.id.name))).collect();
    let values: BTreeMap<&str, &Value> = loadout
        .config
        .iter()
        .filter(|(k, _)| prefixes.iter().any(|p| *k == p || k.strip_prefix(p.as_str()).is_some_and(|rest| rest.starts_with('.'))))
        .map(|(k, s)| (k.as_str(), &s.value))
        .collect();
    Hash::of_json(&values)
}

fn serde_name<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests;
