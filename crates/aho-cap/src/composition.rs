//! Composition: a product as a Rust value; builds Active rows and checks manifest drift.

use super::*;

// ───────────────────────────────── composition ─────────────────────────────────

pub type BuildFn = fn(&mut Resolver, &Loadout) -> Result<(), BuildError>;

/// One bundle's fragment of a built-in profile: TOML that `--profile <name>` layers at
/// `Layer::Profile(name)` below the operator's own `~/.aho/profiles/<name>.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinProfile {
    pub name: &'static str,
    /// The composition that shipped the fragment (kept through `include`).
    pub bundle: &'static str,
    pub text: &'static str,
}

/// A product is a Rust value, not a YAML stack.
#[derive(Clone)]
pub struct Composition {
    pub name: &'static str,
    rows: Vec<(&'static Manifest, BuildFn)>,
    defaults: Vec<(String, Value)>,
    discoverers: Vec<Arc<dyn Discoverer>>,
    profiles: Vec<BuiltinProfile>,
}

impl Composition {
    pub fn new(name: &'static str) -> Self {
        Composition { name, rows: vec![], defaults: vec![], discoverers: vec![], profiles: vec![] }
    }
    pub fn include(mut self, other: Composition) -> Self {
        for r in other.rows {
            if !self.rows.iter().any(|(m, _)| m.id == r.0.id) {
                self.rows.push(r);
            }
        }
        self.defaults.extend(other.defaults);
        self.discoverers.extend(other.discoverers);
        for p in other.profiles {
            if !self.profiles.iter().any(|q| q.name == p.name && q.bundle == p.bundle) {
                self.profiles.push(p);
            }
        }
        self
    }
    /// `provide` / `consume` / `surface` are the same call; the split is documentation.
    pub fn provide<C: Capability>(mut self) -> Self {
        if !self.rows.iter().any(|(m, _)| m.id == C::ID) {
            self.rows.push((C::MANIFEST, build_fn::<C>));
        }
        self
    }
    pub fn consume<C: Capability>(self) -> Self {
        self.provide::<C>()
    }
    pub fn surface<C: Capability>(self) -> Self {
        self.provide::<C>()
    }
    pub fn without<C: Capability>(mut self) -> Self {
        self.rows.retain(|(m, _)| m.id != C::ID);
        self
    }
    /// `Layer::Bundle` default, still shadowable by every operator layer.
    pub fn default(mut self, key: &str, value: Value) -> Self {
        self.defaults.retain(|(k, _)| k != key);
        self.defaults.push((key.to_string(), value));
        self
    }
    pub fn discoverer(mut self, d: Arc<dyn Discoverer>) -> Self {
        self.discoverers.push(d);
        self
    }
    /// Ships `text` (TOML) as this bundle's fragment of built-in profile `name`. Fragments of one
    /// name from included bundles layer in include order; later fragments win within the rank.
    pub fn profile(mut self, name: &'static str, text: &'static str) -> Self {
        self.profiles.retain(|p| !(p.name == name && p.bundle == self.name));
        self.profiles.push(BuiltinProfile { name, bundle: self.name, text });
        self
    }
    pub fn manifests(&self) -> Vec<&'static Manifest> {
        self.rows.iter().map(|(m, _)| *m).collect()
    }
    pub fn defaults(&self) -> &[(String, Value)] {
        &self.defaults
    }
    pub fn discoverers(&self) -> &[Arc<dyn Discoverer>] {
        &self.discoverers
    }
    /// Every built-in profile fragment, in layering order.
    pub fn profiles(&self) -> &[BuiltinProfile] {
        &self.profiles
    }
    /// Every config key any linked capability declares, plus the core keys.
    pub fn config_keys(&self) -> Vec<ConfigKey> {
        let mut keys = core_keys();
        for (m, _) in &self.rows {
            keys.extend((m.config_keys)());
        }
        keys
    }

    /// Builds Active rows in plan order. A capability whose build/register fails, or whose
    /// registration drifts from its manifest, is marked Broken; the rest still start. A row that
    /// fails for a handle a Broken provider declared carries that provider's error, so a broken
    /// world reads `missing handle FsRead: world-vm is Broken: <its own error>` on every dependent.
    pub fn start(&self, plan: &Plan, loadout: &Loadout) -> (Registry, Vec<(CapabilityId, String)>) {
        self.start_seeded(plan, loadout, Registry::default())
    }

    /// Like `start`, with handles/services the session owns (log append, operations table,
    /// approval channel) already in the resolver so tools can declare them in `Deps`.
    pub fn start_seeded(&self, plan: &Plan, loadout: &Loadout, seed: Registry) -> (Registry, Vec<(CapabilityId, String)>) {
        let mut resolver = Resolver { reg: seed, policy: plan.allowed };
        let mut broken: Vec<(CapabilityId, String)> = Vec::new();
        // The manifest behind each `broken` row, index for index.
        let mut failed: Vec<&'static Manifest> = Vec::new();
        // Build in dependency order so a provider that holds another provider's handle sees it built;
        // `plan.rows` is sorted for display. A cycle already failed `plan`, so the fallback is unreachable
        // in a planned composition.
        let order: Vec<&str> = plan::topo(&self.manifests())
            .map(|ms| ms.iter().map(|m| m.id.name).collect())
            .unwrap_or_else(|_| plan.rows.iter().map(|r| r.id.name).collect());
        let mut rows: Vec<&(&'static Manifest, BuildFn)> = self.rows.iter().collect();
        rows.sort_by_key(|(m, _)| (m.kind != Kind::Provider, order.iter().position(|n| *n == m.id.name).unwrap_or(usize::MAX)));
        for (m, build) in rows {
            let Some(row) = plan.row(m.id.name) else { continue };
            if !matches!(row.state, CapState::Active) {
                continue;
            }
            if let Err(e) = build(&mut resolver, loadout) {
                let cause = match e {
                    BuildError::Missing(kind) => failed.iter().zip(&broken).find(|(f, _)| f.provides_handles.contains(&kind)),
                    _ => None,
                };
                let err = match cause {
                    Some((f, (_, cause))) => format!("{e}: {} is Broken: {cause}", f.id.name),
                    None => e.to_string(),
                };
                failed.push(m);
                broken.push((m.id, err));
            }
        }
        (resolver.reg, broken)
    }
}

/// Keys the core itself reads (not owned by any capability).
pub fn core_keys() -> Vec<ConfigKey> {
    use aho_config::Ty;
    use serde_json::json;
    vec![
        ConfigKey::new(
            "seams",
            Ty::Table,
            None,
            "Seam bindings: `seams.<seam> = \"<provider>\"`; `seams.world` picks the execution world.",
        ),
        ConfigKey::new("caps", Ty::Table, None, "Per-capability switches: `caps.<id>.enabled`, `caps.<id>.allow`."),
        ConfigKey::new(
            "policy.preset",
            Ty::String,
            Some(json!("workspace")),
            "Lattice cut: read-only | workspace | dev | lab | full | spec.",
        ),
        ConfigKey::new(
            "policy.rules",
            Ty::Array,
            Some(json!([])),
            "Content rules: `deny bash(rm *)`, `allow bash(git *)`, `ask write(src/**)`.",
        ),
        ConfigKey::new("policy.ask", Ty::String, Some(json!("ask")), "Approval policy: ask | never."),
        ConfigKey::new("model.provider", Ty::String, Some(json!("openai")), "Default model provider (an llm adapter route)."),
        ConfigKey::new("model.name", Ty::String, Some(json!("gpt-4.1-mini")), "Default model id."),
        ConfigKey::new(
            "model.max_tokens",
            Ty::Int,
            Some(json!(8192)),
            "Max output tokens per request. Unset: the model catalog's output limit, capped at 64000 and at the window above `compaction.threshold`; this default for uncatalogued models.",
        ),
        ConfigKey::new(
            "model.thinking",
            Ty::Bool,
            Some(json!(true)),
            "Ask catalogued reasoning models to think; the TUI shows it by default and `/thinking` hides it locally.",
        ),
        ConfigKey::new(
            "model.thinking_budget",
            Ty::Int,
            Some(json!(4096)),
            "Provider reasoning-token budget, kept below `model.max_tokens`. Unset: min(16384, max_tokens / 2) for catalogued reasoning models.",
        ),
        ConfigKey::new(
            "model.context_window",
            Ty::Int,
            Some(json!(128000)),
            "Context budget in tokens: compaction and the TUI's context gauge measure against it. Unset: the model catalog's window (anthropic: 200000, no 1M beta); this default for uncatalogued models.",
        ),
        ConfigKey::new("session.dir", Ty::String, Some(json!(".aho/sessions")), "Session log directory."),
        ConfigKey::new("session.max_turns", Ty::Int, Some(json!(200)), "Hard cap on steps per turn."),
        ConfigKey::new("tools.grace_ms", Ty::Int, Some(json!(1000)), "Async tool grace window before the next turn."),
        ConfigKey::new("tools.heartbeat_s", Ty::Int, Some(json!(600)), "Heartbeat when only tool calls are pending."),
        ConfigKey::new("tools.max_parallel", Ty::Int, Some(json!(10)), "Bounded pool for concurrency-safe tool calls."),
        ConfigKey::new("tools.spill_budget", Ty::Int, Some(json!(200000)), "Per-message aggregate result budget (chars)."),
        ConfigKey::new("compaction.threshold", Ty::Float, Some(json!(0.8)), "Compact when context exceeds this ratio of the window."),
        ConfigKey::new("compaction.retain", Ty::Float, Some(json!(0.16)), "Verbatim tail retained after compaction (ratio of window)."),
        ConfigKey::new("provider", Ty::Table, None, "Provider credentials and endpoints: `provider.<name>.key`, `.base_url`."),
        ConfigKey::new("credentials", Ty::Table, None, "Named credentials referenced by `cred:<name>`."),
    ]
}

fn build_fn<C: Capability>(r: &mut Resolver, loadout: &Loadout) -> Result<(), BuildError> {
    let deps = C::Deps::resolve(r).map_err(|MissingDep(k)| BuildError::Missing(k))?;
    let cfg = C::Config::from_loadout(loadout).map_err(BuildError::Config)?;
    let cap = C::build(deps, cfg)?;
    let grant = Grant { cap: C::ID };
    let (tools, handles) = {
        let mut reg = Registrar { grant: &grant, reg: &mut r.reg, recorded_tools: vec![], recorded_handles: vec![] };
        cap.register(&mut reg);
        (reg.recorded_tools, reg.recorded_handles)
    };
    // manifest drift: what was registered must equal what was declared
    let declared: BTreeSet<&str> = C::MANIFEST.tools.iter().map(|t| t.name).collect();
    let actual: BTreeSet<&str> = tools.iter().copied().collect();
    if declared != actual {
        return Err(BuildError::Probe(format!("ManifestDrift: declared tools {declared:?}, registered {actual:?}")));
    }
    let declared_h: BTreeSet<HandleKind> = C::MANIFEST.provides_handles.iter().copied().collect();
    let actual_h: BTreeSet<HandleKind> = handles.into_iter().collect();
    if !actual_h.is_subset(&declared_h) {
        return Err(BuildError::Probe(format!("ManifestDrift: provided handles {actual_h:?} not declared in {declared_h:?}")));
    }
    Ok(())
}

/// Resolves `Deps` against handles provided by earlier (provider) rows.
pub struct Resolver {
    reg: Registry,
    pub policy: ConsequenceSet,
}

impl Resolver {
    pub fn handle<H: Handle>(&self) -> Result<H, MissingDep> {
        self.reg.handle::<H>().ok_or(MissingDep(H::KIND))
    }
    pub fn service<T: Any + Send + Sync + Clone>(&self, name: &str) -> Option<T> {
        self.reg.service(name)
    }
}

/// Convenience: the origin text for an operator-facing line.
pub fn render_origin(o: &Origin) -> String {
    o.render()
}
