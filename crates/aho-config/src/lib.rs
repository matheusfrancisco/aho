//! There are no bare config values. Every resolved setting is `Sourced<T>`; the complete
//! resolved set becomes the session log's first entries. `resolve` is pure: the caller captures
//! argv/env/file text into `ResolveInputs` first.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use aho_types::*;
use serde::{Deserialize, Serialize};

// ───────────────────────────── provenance ─────────────────────────────

/// Rank (explicitness). Higher rank wins.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "layer", content = "name", rename_all = "snake_case")]
pub enum Layer {
    Default,
    Bundle(String),
    /// Derived, not set: a value read from a catalog for other settings (the `model.*` limits
    /// of `<provider>/<model>`). Beats engineer defaults, loses to every operator layer.
    Catalog(String),
    Profile(String),
    FileHome,
    FileProject,
    Env,
    Arg,
    Runtime(String),
}

impl Layer {
    fn rank(&self) -> u8 {
        match self {
            Layer::Default => 0,
            Layer::Bundle(_) => 1,
            Layer::Catalog(_) => 2,
            Layer::Profile(_) => 3,
            Layer::FileHome => 4,
            Layer::FileProject => 5,
            Layer::Env => 6,
            Layer::Arg => 7,
            Layer::Runtime(_) => 8,
        }
    }
    /// Someone chose this value (profile, files, env, flags, runtime), rather than the engineer
    /// (Default, Bundle) or a catalog.
    pub fn is_operator(&self) -> bool {
        !matches!(self, Layer::Default | Layer::Bundle(_) | Layer::Catalog(_))
    }
    pub fn scope(&self) -> Scope {
        match self {
            Layer::Default | Layer::Bundle(_) | Layer::Catalog(_) | Layer::Profile(_) => Scope::Machine,
            Layer::FileHome => Scope::User,
            Layer::FileProject => Scope::Project,
            // Env vars usually come from a shell rc: user-wide, not this invocation.
            Layer::Env => Scope::User,
            Layer::Arg => Scope::Invocation,
            Layer::Runtime(_) => Scope::Session,
        }
    }
}

/// Breadth. `broader-scope-wins` fires when a winner is broader than a loser it beat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Machine,
    User,
    Project,
    Invocation,
    Session,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum Where {
    Const,
    File { path: String, line: u32 },
    Env { var: String },
    Flag { flag: String },
    Entry { id: EntryId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub layer: Layer,
    pub scope: Scope,
    pub at: Where,
}

impl Origin {
    pub fn new(layer: Layer, at: Where) -> Origin {
        let scope = layer.scope();
        Origin { layer, scope, at }
    }
    pub fn render(&self) -> String {
        let layer = match &self.layer {
            Layer::Default => "Default".to_string(),
            Layer::Bundle(n) => format!("Bundle {n}"),
            Layer::Catalog(n) => format!("Catalog {n}"),
            Layer::Profile(n) => format!("Profile {n}"),
            Layer::FileHome | Layer::FileProject => "File".to_string(),
            Layer::Env => "Env".to_string(),
            Layer::Arg => "Arg".to_string(),
            Layer::Runtime(by) => format!("Runtime ({by})"),
        };
        match &self.at {
            Where::Const => layer,
            Where::File { path, line } => format!("{layer} {path}:{line}"),
            Where::Env { var } => format!("{layer} {var}"),
            Where::Flag { flag } => format!("{layer} {flag}"),
            Where::Entry { id } => format!("{layer} entry {}", id.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sourced<T> {
    pub value: T,
    pub winner: Origin,
    /// Every loser, most specific first.
    pub shadowed: Vec<(Origin, T)>,
}

/// Plaintext that never enters a log or loadout; only its digest does.
#[derive(Clone)]
pub struct Secret {
    pub digest: Hash,
    plaintext: String,
}

impl Secret {
    pub fn new(plaintext: String) -> Self {
        Secret { digest: Hash::of(plaintext.as_bytes()), plaintext }
    }
    pub fn expose(&self) -> &str {
        &self.plaintext
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({})", self.digest.short())
    }
}

// ───────────────────────────── schema ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ty {
    String,
    Bool,
    Int,
    Float,
    Array,
    Table,
    Any,
}

impl Ty {
    fn accepts(self, v: &Value) -> bool {
        match self {
            Ty::String => v.is_string(),
            Ty::Bool => v.is_boolean(),
            Ty::Int => v.is_i64() || v.is_u64(),
            Ty::Float => v.is_number(),
            Ty::Array => v.is_array(),
            Ty::Table => v.is_object(),
            Ty::Any => true,
        }
    }
}

/// One row per settable key, contributed by a capability's `Config` (real build: `#[derive(Config)]` + `inventory`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigKey {
    pub path: String,
    pub ty: Ty,
    pub default: Option<Value>,
    pub doc: String,
    pub secret: bool,
}

impl ConfigKey {
    pub fn new(path: &str, ty: Ty, default: Option<Value>, doc: &str) -> ConfigKey {
        ConfigKey { path: path.to_string(), ty, default, doc: doc.to_string(), secret: false }
    }
    pub fn secret(mut self) -> ConfigKey {
        self.secret = true;
        self
    }
    /// `sandbox.mode` → `AHO_SANDBOX_MODE`.
    pub fn env_var(&self) -> String {
        format!("AHO_{}", self.path.replace(['.', '-'], "_").to_uppercase())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Schema {
    pub keys: BTreeMap<String, ConfigKey>,
}

impl Schema {
    pub fn add(&mut self, k: ConfigKey) {
        self.keys.insert(k.path.clone(), k);
    }
    /// Keys whose value is a table accept any sub-key (`mcp.servers.<name>.cmd`).
    fn lookup(&self, path: &str) -> Option<&ConfigKey> {
        if let Some(k) = self.keys.get(path) {
            return Some(k);
        }
        let mut prefix = path;
        while let Some((head, _)) = prefix.rsplit_once('.') {
            if let Some(k) = self.keys.get(head)
                && k.ty == Ty::Table
            {
                return Some(k);
            }
            prefix = head;
        }
        None
    }
    pub fn hash(&self) -> Hash {
        Hash::of_json(&self.keys)
    }
}

// ───────────────────────────── inputs → loadout ─────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct ResolveInputs {
    /// `--set key=value` pairs, in argv order.
    pub sets: Vec<(String, String)>,
    pub env: BTreeMap<String, String>,
    pub files: Vec<FileSnapshot>,
    /// Engineer defaults from the composition (`Layer::Bundle`).
    pub bundle: String,
    pub bundle_defaults: Vec<(String, Value)>,
    /// Keys applied at runtime by `aho apply` / UI toggles, already attributed.
    pub runtime: Vec<(String, Value, String, Option<EntryId>)>,
    /// `--disable <id>` flags; `AHO_DISABLE` is read from `env`.
    pub disable: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct FileSnapshot {
    pub path: String,
    pub layer: Layer,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lint {
    BroaderScopeWins,
    DeadKey,
    UnknownKey,
    TypeMismatch,
    ReplacedCapability,
    ConsequenceWidened,
    ShadowedRule,
    WorldCapsPreset,
    PresetUnmatched,
    HookManagedOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub lint: Lint,
    pub severity: Severity,
    pub key: String,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct Loadout {
    pub hash: Hash,
    /// Secret keys hold `{"digest": "<hex>"}` here; plaintext lives only in `secrets`.
    pub config: BTreeMap<String, Sourced<Value>>,
    pub secrets: BTreeMap<String, Secret>,
    pub disabled: Vec<(String, Origin)>,
    pub inputs: Vec<(String, Hash)>,
    pub schema_hash: Hash,
    pub diagnostics: Vec<Diagnostic>,
}

impl Loadout {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.config.get(key).map(|s| &s.value)
    }
    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| v.as_str())
    }
    pub fn is_disabled(&self, id: &str) -> bool {
        self.disabled.iter().any(|(d, _)| d == id)
    }
    /// Rebuilds a table from dotted leaves under `prefix` (`mcp.servers` → `{github: {cmd: ..}}`).
    pub fn table(&self, prefix: &str) -> serde_json::Map<String, Value> {
        let mut root = serde_json::Map::new();
        let p = format!("{prefix}.");
        for (k, s) in &self.config {
            let Some(rest) = k.strip_prefix(&p) else { continue };
            let parts: Vec<&str> = rest.split('.').collect();
            let mut cur = &mut root;
            for (i, part) in parts.iter().enumerate() {
                if i + 1 == parts.len() {
                    cur.insert(part.to_string(), s.value.clone());
                } else {
                    let e = cur.entry(part.to_string()).or_insert_with(|| Value::Object(serde_json::Map::new()));
                    if !e.is_object() {
                        *e = Value::Object(serde_json::Map::new());
                    }
                    let Value::Object(m) = e else { break };
                    cur = m;
                }
            }
        }
        if let Some(Value::Object(m)) = self.get(prefix) {
            for (k, v) in m {
                root.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        root
    }
    /// Ranks a value derived from other settings (`Layer::Catalog` named `name`) among the key's
    /// layers: it wins over Default and Bundle values and is shadowed by every operator layer.
    /// An earlier catalog value of the key is replaced, so re-deriving after the model changes
    /// never stacks; `None` only drops it.
    pub fn derive(&mut self, key: &str, value: Option<Value>, name: &str) {
        let mut layers: Vec<(Origin, Value)> = match self.config.remove(key) {
            Some(s) => std::iter::once((s.winner, s.value)).chain(s.shadowed).collect(),
            None => Vec::new(),
        };
        layers.retain(|(o, _)| !matches!(o.layer, Layer::Catalog(_)));
        if let Some(v) = value {
            let origin = Origin::new(Layer::Catalog(name.to_string()), Where::Const);
            let at = layers.iter().position(|(o, _)| o.layer.rank() < origin.layer.rank()).unwrap_or(layers.len());
            layers.insert(at, (origin, v));
        }
        let mut layers = layers.into_iter();
        if let Some((winner, value)) = layers.next() {
            self.config.insert(key.to_string(), Sourced { value, winner, shadowed: layers.collect() });
        }
        self.hash = loadout_hash(&self.config, &self.disabled, self.schema_hash);
    }
}

#[derive(Debug)]
pub enum ResolveError {
    Parse { path: String, msg: String },
    Strict(Vec<Diagnostic>),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Parse { path, msg } => write!(f, "{path}: {msg}"),
            ResolveError::Strict(d) => write!(f, "{} diagnostic(s) at error level under --strict", d.len()),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Pure. Groups `(key, value, origin)` triples by key, orders by rank, emits `Sourced<Value>` per key.
pub fn resolve(inputs: &ResolveInputs, schema: &Schema, strict: bool) -> Result<Loadout, ResolveError> {
    let mut triples: Vec<(String, Value, Origin)> = Vec::new();
    let mut diagnostics = Vec::new();
    let mut disabled: Vec<(String, Origin)> = Vec::new();

    for k in schema.keys.values() {
        if let Some(d) = &k.default {
            triples.push((k.path.clone(), d.clone(), Origin::new(Layer::Default, Where::Const)));
        }
    }
    for (key, v) in &inputs.bundle_defaults {
        triples.push((key.clone(), v.clone(), Origin::new(Layer::Bundle(inputs.bundle.clone()), Where::Const)));
    }

    let mut input_digests = Vec::new();
    let mut files = inputs.files.clone();
    files.sort_by_key(|f| f.layer.rank());
    for f in &files {
        input_digests.push((f.path.clone(), Hash::of(f.text.as_bytes())));
        let table: toml::Table =
            f.text.parse().map_err(|e: toml::de::Error| ResolveError::Parse { path: f.path.clone(), msg: e.to_string() })?;
        let lines = key_lines(&f.text);
        let mut flat = Vec::new();
        flatten("", &toml::Value::Table(table), schema, &mut flat);
        for (key, v) in flat {
            if key == "disable" {
                if let Some(arr) = v.as_array() {
                    for id in arr.iter().filter_map(|x| x.as_str()) {
                        let line = lines.get("disable").copied().unwrap_or(1);
                        disabled.push((id.to_string(), Origin::new(f.layer.clone(), Where::File { path: f.path.clone(), line })));
                    }
                }
                continue;
            }
            let line = lines.get(&key).copied().unwrap_or(1);
            triples.push((key, v, Origin::new(f.layer.clone(), Where::File { path: f.path.clone(), line })));
        }
    }

    let env_map: BTreeMap<String, &ConfigKey> = schema.keys.values().map(|k| (k.env_var(), k)).collect();
    for (var, raw) in &inputs.env {
        if !var.starts_with("AHO_") {
            continue;
        }
        if var == "AHO_DISABLE" {
            for id in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                disabled.push((id.to_string(), Origin::new(Layer::Env, Where::Env { var: var.clone() })));
            }
            continue;
        }
        match env_map.get(var) {
            Some(k) => triples.push((k.path.clone(), parse_scalar(raw), Origin::new(Layer::Env, Where::Env { var: var.clone() }))),
            None => diagnostics.push(Diagnostic {
                lint: Lint::UnknownKey,
                severity: Severity::Warning,
                key: var.clone(),
                detail: format!("{var} matches no config key in this composition"),
            }),
        }
    }
    for (key, raw) in &inputs.sets {
        triples.push((key.clone(), parse_scalar(raw), Origin::new(Layer::Arg, Where::Flag { flag: format!("--set {key}") })));
    }
    for id in &inputs.disable {
        disabled.push((id.clone(), Origin::new(Layer::Arg, Where::Flag { flag: format!("--disable {id}") })));
    }
    for (key, v, by, entry) in &inputs.runtime {
        let at = entry.map(|id| Where::Entry { id }).unwrap_or(Where::Const);
        triples.push((key.clone(), v.clone(), Origin::new(Layer::Runtime(by.clone()), at)));
    }

    // group by key, highest rank first; within a rank, later input wins
    let mut grouped: BTreeMap<String, Vec<(usize, Value, Origin)>> = BTreeMap::new();
    for (i, (k, v, o)) in triples.into_iter().enumerate() {
        grouped.entry(k).or_default().push((i, v, o));
    }
    let mut config = BTreeMap::new();
    let mut secrets = BTreeMap::new();
    for (key, mut vals) in grouped {
        vals.sort_by(|a, b| b.2.layer.rank().cmp(&a.2.layer.rank()).then(b.0.cmp(&a.0)));
        let decl = schema.lookup(&key);
        match decl {
            None => diagnostics.push(Diagnostic {
                lint: Lint::UnknownKey,
                severity: Severity::Warning,
                key: key.clone(),
                detail: format!("`{key}` is not declared by any capability linked into this composition"),
            }),
            Some(k) if !k.ty.accepts(&vals[0].1) && k.path == key => diagnostics.push(Diagnostic {
                lint: Lint::TypeMismatch,
                severity: Severity::Error,
                key: key.clone(),
                detail: format!("expected {:?}, got {} from {}", k.ty, vals[0].1, vals[0].2.render()),
            }),
            _ => {}
        }
        let (_, winner_v, winner_o) = vals.remove(0);
        for (_, _, loser) in &vals {
            if winner_o.scope < loser.scope {
                diagnostics.push(Diagnostic {
                    lint: Lint::BroaderScopeWins,
                    severity: Severity::Warning,
                    key: key.clone(),
                    detail: format!("{} ({:?}) beats {} ({:?})", winner_o.render(), winner_o.scope, loser.render(), loser.scope),
                });
            }
            if matches!(loser.layer, Layer::FileHome) && matches!(winner_o.layer, Layer::FileProject) {
                diagnostics.push(Diagnostic {
                    lint: Lint::DeadKey,
                    severity: Severity::Info,
                    key: key.clone(),
                    detail: format!("{} never takes effect in this project", loser.render()),
                });
            }
        }
        let is_secret = decl.map(|k| k.secret).unwrap_or(false);
        let redact = |v: Value| -> Value {
            if is_secret {
                let plain = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                serde_json::json!({ "digest": Secret::new(plain).digest.to_hex() })
            } else {
                v
            }
        };
        if is_secret {
            let plain = winner_v.as_str().map(str::to_string).unwrap_or_else(|| winner_v.to_string());
            secrets.insert(key.clone(), Secret::new(plain));
        }
        let shadowed = vals.into_iter().map(|(_, v, o)| (o, redact(v))).collect();
        config.insert(key, Sourced { value: redact(winner_v), winner: winner_o, shadowed });
    }

    let schema_hash = schema.hash();
    let hash = loadout_hash(&config, &disabled, schema_hash);
    if strict {
        let errs: Vec<Diagnostic> = diagnostics.iter().filter(|d| d.severity >= Severity::Warning).cloned().collect();
        if !errs.is_empty() {
            return Err(ResolveError::Strict(errs));
        }
    }
    Ok(Loadout { hash, config, secrets, disabled, inputs: input_digests, schema_hash, diagnostics })
}

/// The loadout's identity: winning values, disabled ids and the schema, never origins.
fn loadout_hash(config: &BTreeMap<String, Sourced<Value>>, disabled: &[(String, Origin)], schema_hash: Hash) -> Hash {
    let values: BTreeMap<&String, &Value> = config.iter().map(|(k, s)| (k, &s.value)).collect();
    let dis: Vec<&String> = disabled.iter().map(|(d, _)| d).collect();
    Hash::of_json(&(values, dis, schema_hash))
}

/// Flattens TOML into dotted leaf keys.
fn flatten(prefix: &str, v: &toml::Value, schema: &Schema, out: &mut Vec<(String, Value)>) {
    let _ = schema;
    match v {
        // Always descend to leaves: a declared `Table` key (e.g. `provider`) must not swallow a more
        // specific declared key under it (`provider.openai.key`, secret). Consumers rebuild tables with `Loadout::table`.
        toml::Value::Table(t) => {
            for (k, child) in t {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, child, schema, out);
            }
        }
        other => out.push((prefix.to_string(), toml_to_json(other))),
    }
}

fn toml_to_json(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// `--set k=v` / env values: parse as a TOML value when possible (`true`, `3`, `["a"]`), else a string.
pub fn parse_scalar(raw: &str) -> Value {
    let doc = format!("v = {raw}");
    match doc.parse::<toml::Table>() {
        Ok(t) => t.get("v").map(toml_to_json).unwrap_or_else(|| Value::String(raw.to_string())),
        Err(_) => Value::String(raw.to_string()),
    }
}

/// Maps dotted keys to the 1-based line that assigns them, tracking `[table]` headers.
fn key_lines(text: &str) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    let mut table = String::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(h) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            table = h.trim().replace(['"', '\''], "");
            continue;
        }
        if let Some(h) = line.strip_prefix('[').and_then(|l| l.split(']').next()) {
            table = h.trim().replace(['"', '\''], "");
            continue;
        }
        if let Some((k, _)) = line.split_once('=') {
            let k = k.trim().replace(['"', '\''], "");
            let full = if table.is_empty() { k } else { format!("{table}.{k}") };
            out.entry(full).or_insert(i as u32 + 1);
        }
    }
    out
}

/// `aho explain <key>`.
pub fn explain<'a>(loadout: &'a Loadout, key: &str) -> Option<&'a Sourced<Value>> {
    loadout.config.get(key)
}

/// `aho plan --patch`: `(key, before, after)` for every key whose winning value changed.
pub fn diff(before: &Loadout, after: &Loadout) -> Vec<(String, Option<Value>, Option<Value>)> {
    let mut keys: Vec<&String> = before.config.keys().chain(after.config.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter_map(|k| {
            let a = before.get(k).cloned();
            let b = after.get(k).cloned();
            (a != b).then(|| (k.clone(), a, b))
        })
        .collect()
}

/// `aho apply`: set `key` in a TOML document, comments and formatting preserved.
pub fn apply_to_file(text: &str, key: &str, value: &Value) -> Result<String, ResolveError> {
    let mut doc: toml_edit::DocumentMut =
        text.parse().map_err(|e: toml_edit::TomlError| ResolveError::Parse { path: "<apply>".into(), msg: e.to_string() })?;
    let parts: Vec<&str> = key.split('.').collect();
    let (last, heads) = parts.split_last().ok_or_else(|| ResolveError::Parse { path: "<apply>".into(), msg: "empty key".into() })?;
    let mut table = doc.as_table_mut();
    for h in heads {
        if !table.contains_key(h) {
            table.insert(h, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        table =
            table[h].as_table_mut().ok_or_else(|| ResolveError::Parse { path: "<apply>".into(), msg: format!("`{h}` is not a table") })?;
    }
    table.insert(last, toml_edit::value(json_to_edit(value)?));
    Ok(doc.to_string())
}

fn json_to_edit(v: &Value) -> Result<toml_edit::Value, ResolveError> {
    let bad = || ResolveError::Parse { path: "<apply>".into(), msg: format!("cannot write {v} to TOML") };
    Ok(match v {
        Value::Bool(b) => (*b).into(),
        Value::Number(n) if n.is_i64() => n.as_i64().ok_or_else(bad)?.into(),
        Value::Number(n) => n.as_f64().ok_or_else(bad)?.into(),
        Value::String(s) => s.as_str().into(),
        Value::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(json_to_edit(x)?);
            }
            toml_edit::Value::Array(arr)
        }
        Value::Null | Value::Object(_) => return Err(bad()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Schema {
        let mut s = Schema::default();
        s.add(ConfigKey::new("model.provider", Ty::String, Some(json!("openai")), "provider"));
        s.add(ConfigKey::new("sandbox.mode", Ty::String, Some(json!("host")), "world"));
        s.add(ConfigKey::new("tools.grace_ms", Ty::Int, Some(json!(1000)), "grace"));
        s.add(ConfigKey::new("provider.openai.key", Ty::String, None, "key").secret());
        s.add(ConfigKey::new("mcp.servers", Ty::Table, None, "servers"));
        s
    }

    fn file(path: &str, layer: Layer, text: &str) -> FileSnapshot {
        FileSnapshot { path: path.into(), layer, text: text.into() }
    }

    #[test]
    fn env_beats_file_beats_default_with_line_numbers() {
        let mut inputs = ResolveInputs::default();
        inputs.files.push(file("./aho.toml", Layer::FileProject, "# c\n\n[model]\n\n\n\nprovider = \"anthropic\"\n"));
        inputs.env.insert("AHO_MODEL_PROVIDER".into(), "ollama".into());
        let l = resolve(&inputs, &schema(), false).unwrap();
        let s = &l.config["model.provider"];
        assert_eq!(s.value, json!("ollama"));
        assert_eq!(s.winner.at, Where::Env { var: "AHO_MODEL_PROVIDER".into() });
        assert_eq!(s.shadowed.len(), 2);
        assert_eq!(s.shadowed[0].0.at, Where::File { path: "./aho.toml".into(), line: 7 });
        assert_eq!(s.shadowed[1].0.layer, Layer::Default);
    }

    #[test]
    fn home_value_shadowed_by_project_is_a_dead_key() {
        let mut inputs = ResolveInputs::default();
        inputs.files.push(file("~/.aho/config.toml", Layer::FileHome, "[sandbox]\nmode = \"ns\"\n"));
        inputs.files.push(file("./aho.toml", Layer::FileProject, "[sandbox]\nmode = \"vm\"\n"));
        let l = resolve(&inputs, &schema(), false).unwrap();
        assert_eq!(l.config["sandbox.mode"].value, json!("vm"));
        assert!(l.diagnostics.iter().any(|d| d.lint == Lint::DeadKey && d.key == "sandbox.mode"));
        assert!(!l.diagnostics.iter().any(|d| d.lint == Lint::BroaderScopeWins));
    }

    #[test]
    fn stale_shell_env_beating_project_file_is_broader_scope_wins() {
        let mut inputs = ResolveInputs::default();
        inputs.files.push(file("./aho.toml", Layer::FileProject, "[model]\nprovider = \"anthropic\"\n"));
        inputs.env.insert("AHO_MODEL_PROVIDER".into(), "ollama".into());
        let l = resolve(&inputs, &schema(), false).unwrap();
        assert!(l.diagnostics.iter().any(|d| d.lint == Lint::BroaderScopeWins && d.key == "model.provider"));
        inputs.env.clear();
        inputs.sets.push(("model.provider".into(), "ollama".into()));
        let l = resolve(&inputs, &schema(), false).unwrap();
        assert!(!l.diagnostics.iter().any(|d| d.lint == Lint::BroaderScopeWins), "an explicit --set is not broader");
    }

    #[test]
    fn unknown_env_and_type_mismatch_diagnose_and_strict_rejects() {
        let mut inputs = ResolveInputs::default();
        inputs.env.insert("AHO_NOPE".into(), "1".into());
        inputs.sets.push(("tools.grace_ms".into(), "\"soon\"".into()));
        let l = resolve(&inputs, &schema(), false).unwrap();
        assert!(l.diagnostics.iter().any(|d| d.lint == Lint::UnknownKey && d.key == "AHO_NOPE"));
        assert!(l.diagnostics.iter().any(|d| d.lint == Lint::TypeMismatch));
        assert!(matches!(resolve(&inputs, &schema(), true), Err(ResolveError::Strict(_))));
    }

    #[test]
    fn secrets_never_appear_in_loadout_values() {
        let mut inputs = ResolveInputs::default();
        inputs.env.insert("AHO_PROVIDER_OPENAI_KEY".into(), "sk-live-123".into());
        let l = resolve(&inputs, &schema(), false).unwrap();
        let shown = serde_json::to_string(&l.config["provider.openai.key"]).unwrap();
        assert!(!shown.contains("sk-live-123"));
        assert_eq!(l.secrets["provider.openai.key"].expose(), "sk-live-123");
    }

    #[test]
    fn table_keys_accept_subkeys_and_disable_collects_every_origin() {
        let mut inputs = ResolveInputs::default();
        inputs.files.push(file("./aho.toml", Layer::FileProject, "disable = [\"web-search\"]\n[mcp.servers.github]\ncmd = \"npx\"\n"));
        inputs.env.insert("AHO_DISABLE".into(), "tool-vm, mcp.github".into());
        inputs.disable.push("tool-terminal".into());
        let l = resolve(&inputs, &schema(), false).unwrap();
        assert_eq!(l.config["mcp.servers.github.cmd"].value, json!("npx"));
        assert_eq!(Value::Object(l.table("mcp.servers")), json!({"github": {"cmd": "npx"}}));
        for id in ["web-search", "tool-vm", "mcp.github", "tool-terminal"] {
            assert!(l.is_disabled(id), "{id}");
        }
    }

    #[test]
    fn hash_changes_with_values_not_with_origins() {
        let mut a = ResolveInputs::default();
        a.sets.push(("sandbox.mode".into(), "vm".into()));
        let mut b = ResolveInputs::default();
        b.env.insert("AHO_SANDBOX_MODE".into(), "vm".into());
        let (la, lb) = (resolve(&a, &schema(), false).unwrap(), resolve(&b, &schema(), false).unwrap());
        assert_eq!(la.hash, lb.hash);
        let c = ResolveInputs::default();
        assert_ne!(la.hash, resolve(&c, &schema(), false).unwrap().hash);
        assert_eq!(
            diff(&resolve(&c, &schema(), false).unwrap(), &la),
            vec![("sandbox.mode".into(), Some(json!("host")), Some(json!("vm")))]
        );
    }

    #[test]
    fn a_catalog_value_beats_engineer_defaults_loses_to_operators_and_rederiving_replaces_it() {
        let mut l = resolve(&ResolveInputs::default(), &schema(), false).unwrap();
        let before = l.hash;
        l.derive("tools.grace_ms", Some(json!(5)), "a/m");
        let s = &l.config["tools.grace_ms"];
        assert_eq!((s.value.clone(), s.winner.render()), (json!(5), "Catalog a/m".to_string()));
        assert_eq!(s.shadowed[0].0.layer, Layer::Default);
        assert_ne!(l.hash, before, "the winning value changed, so the loadout's identity did");
        l.derive("tools.grace_ms", Some(json!(7)), "a/n");
        let s = &l.config["tools.grace_ms"];
        assert_eq!((s.value.clone(), s.shadowed.len()), (json!(7), 1), "a re-derived value replaces the stale one");
        l.derive("tools.grace_ms", None, "a/unknown");
        assert_eq!(l.config["tools.grace_ms"].value, json!(1000), "an uncatalogued model falls back to the default");
        assert_eq!(l.hash, before);

        let mut inputs = ResolveInputs::default();
        inputs.files.push(file("./aho.toml", Layer::FileProject, "[tools]\ngrace_ms = 3\n"));
        let mut l = resolve(&inputs, &schema(), false).unwrap();
        l.derive("tools.grace_ms", Some(json!(5)), "a/m");
        let s = &l.config["tools.grace_ms"];
        assert_eq!(s.value, json!(3), "an operator value wins over the catalog");
        assert!(s.winner.layer.is_operator());
        let ranks: Vec<String> = s.shadowed.iter().map(|(o, _)| o.render()).collect();
        assert_eq!(ranks, ["Catalog a/m", "Default"]);
    }

    #[test]
    fn apply_preserves_comments() {
        let out = apply_to_file("# keep me\n[sandbox]\nmode = \"host\" # inline\n", "sandbox.mode", &json!("vm")).unwrap();
        assert!(out.contains("# keep me"));
        assert!(out.contains("mode = \"vm\""));
        let out = apply_to_file("", "skills.deploy.enabled", &json!(true)).unwrap();
        assert!(out.contains("[skills.deploy]") && out.contains("enabled = true"));
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn declared_secret_under_a_table_key_is_redacted_from_files() {
        let mut s = Schema::default();
        s.add(ConfigKey::new("provider", Ty::Table, None, "providers"));
        s.add(ConfigKey::new("provider.openai.key", Ty::String, None, "key").secret());
        let mut i = ResolveInputs::default();
        i.files.push(FileSnapshot {
            path: "./aho.toml".into(),
            layer: Layer::FileProject,
            text: "[provider.openai]\nkey = \"sk-file\"\nbase_url = \"http://x\"\n".into(),
        });
        let l = resolve(&i, &s, false).unwrap();
        assert_eq!(l.secrets["provider.openai.key"].expose(), "sk-file");
        assert!(!serde_json::to_string(&l.config["provider.openai.key"]).unwrap().contains("sk-file"));
        assert_eq!(l.config["provider.openai.key"].winner.at, Where::File { path: "./aho.toml".into(), line: 2 });
        assert_eq!(l.table("provider")["openai"]["base_url"], json!("http://x"));
    }
}
