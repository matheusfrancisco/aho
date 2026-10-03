//! Project instruction files (`AGENTS.md`, `CLAUDE.md`, …) as dynamic context.
//!
//! * A `PreStep` hook reads `context_files.names` from the workspace root through the bound
//!   world's `FsRead` before every request, so an edit (a tool call, the shell, the operator's
//!   editor) shows in the next request. Identical content is shown once (`same-as`); each file
//!   is capped at `context_files.max_bytes` and all of them together at twice that, with the
//!   truncation marked in place.
//! * Nested files with the same basenames are listed, paths only: one gitignore-aware glob on the
//!   first step and again after a tool call whose arguments mention one of the basenames.
//! * The `Registrar::context` returns the cached render; the session logs it as a `Snapshot`
//!   user message only when the text changed.
//!
//! Repo text is conversation data, never authority: the decision chain reads tool calls and
//! policy, not message text (docs/authority.md).
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_fs::{FsError, FsRead};
use aho_log::State;
use aho_types::*;
use serde_json::json;

/// The one fixed line above the files (prose ratchet).
const PREAMBLE: &str = include_str!("../prompts/context.md");
pub const DEFAULT_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md"];
pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024;
/// Nested paths listed before the rest are counted instead.
pub const MAX_NESTED: usize = 50;
const HOOK_TIMEOUT_MS: u64 = 10_000;
const ON_PRE_STEP: &[HookEvent] = &[HookEvent::PreStep];

/// `context_files.*` keys.
#[derive(Clone, Debug)]
pub struct ContextFilesConfig {
    pub enabled: bool,
    /// Workspace-relative paths read from the root, in display order.
    pub names: Vec<String>,
    /// Per-file cap; the total cap is twice this.
    pub max_bytes: usize,
}

impl Config for ContextFilesConfig {
    const PREFIX: &'static str = "context_files";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "context_files.enabled",
                Ty::Bool,
                Some(json!(true)),
                "Show the workspace's instruction files (AGENTS.md, CLAUDE.md, …) to the model as context.",
            ),
            ConfigKey::new(
                "context_files.names",
                Ty::Array,
                Some(json!(DEFAULT_NAMES)),
                "Instruction files read from the workspace root, in order; nested files with the same basenames are listed by path.",
            ),
            ConfigKey::new(
                "context_files.max_bytes",
                Ty::Int,
                Some(json!(DEFAULT_MAX_BYTES)),
                "Bytes shown per instruction file (all files together: twice this); the rest is cut with a marker.",
            ),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let names_err = || ConfigError::Type { key: "context_files.names".into(), expected: "an array of non-empty paths" };
        let names = match l.get("context_files.names") {
            None => DEFAULT_NAMES.iter().map(|s| s.to_string()).collect(),
            Some(v) => v
                .as_array()
                .ok_or_else(names_err)?
                .iter()
                .map(|n| n.as_str().map(|s| s.trim_start_matches("./").to_string()).filter(|s| !s.is_empty()).ok_or_else(names_err))
                .collect::<Result<Vec<_>, _>>()?,
        };
        let max_bytes = usize::try_from(cfg_u64(l, "context_files.max_bytes", DEFAULT_MAX_BYTES))
            .ok()
            .filter(|n| *n >= 1)
            .ok_or(ConfigError::Type { key: "context_files.max_bytes".into(), expected: "a positive integer" })?;
        Ok(ContextFilesConfig { enabled: cfg_bool(l, "context_files.enabled", true), names, max_bytes })
    }
}

/// One root file as read this step.
enum Loaded {
    Text(Vec<u8>),
    /// Exists but could not be read; the code is shown so the gap is visible.
    Unreadable(&'static str),
}

struct Shared {
    fs: FsRead,
    cfg: ContextFilesConfig,
    /// Basenames of `names` usable in a glob (no glob metacharacters).
    basenames: Vec<String>,
    /// Re-run the nested glob at the next refresh.
    nested_stale: AtomicBool,
    nested: Mutex<Vec<String>>,
    text: Mutex<Option<String>>,
}

impl Shared {
    async fn refresh(&self) {
        let mut files = Vec::with_capacity(self.cfg.names.len());
        for name in &self.cfg.names {
            match self.fs.0.read_bytes(name).await {
                Ok(bytes) => files.push((name.as_str(), Loaded::Text(bytes))),
                Err(FsError::NotFound(_)) => {}
                Err(e) => files.push((name.as_str(), Loaded::Unreadable(e.code()))),
            }
        }
        if self.nested_stale.swap(false, Ordering::AcqRel)
            && let Some(found) = self.glob_nested().await
        {
            *lock(&self.nested) = found;
        }
        let rendered = render(&files, &lock(&self.nested), self.cfg.max_bytes);
        *lock(&self.text) = rendered;
    }

    /// Nested instruction files, sorted; `None` (keep the last list) when the world refused the walk.
    async fn glob_nested(&self) -> Option<Vec<String>> {
        let pattern = match self.basenames.as_slice() {
            [] => return Some(Vec::new()),
            [one] => format!("**/{one}"),
            many => format!("**/{{{}}}", many.join(",")),
        };
        match self.fs.0.glob(&pattern, None).await {
            Ok(paths) => Some(paths.into_iter().filter(|p| !self.cfg.names.iter().any(|n| n == p)).collect()),
            Err(_) => {
                self.nested_stale.store(true, Ordering::Release);
                None
            }
        }
    }

    /// A call whose arguments mention an instruction file may have created or moved one.
    fn note_call(&self, args: &Value) {
        if mentions(args, &self.basenames) {
            self.nested_stale.store(true, Ordering::Release);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn mentions(v: &Value, needles: &[String]) -> bool {
    match v {
        Value::String(s) => needles.iter().any(|n| s.contains(n.as_str())),
        Value::Array(a) => a.iter().any(|x| mentions(x, needles)),
        Value::Object(o) => o.values().any(|x| mentions(x, needles)),
        _ => false,
    }
}

/// The envelope: one `<repo-instructions>` per distinct file, then the nested paths. `None` when
/// there is nothing to show.
fn render(files: &[(&str, Loaded)], nested: &[String], max_bytes: usize) -> Option<String> {
    if files.is_empty() && nested.is_empty() {
        return None;
    }
    let mut out = String::from(PREAMBLE.trim_end());
    let mut budget = max_bytes.saturating_mul(2);
    for (i, (path, loaded)) in files.iter().enumerate() {
        match loaded {
            Loaded::Unreadable(code) => out.push_str(&format!("\n<repo-instructions path=\"{path}\" unreadable=\"{code}\"/>")),
            Loaded::Text(bytes) => {
                let twin = files[..i].iter().find_map(|(p, l)| matches!(l, Loaded::Text(b) if b == bytes).then_some(*p));
                if let Some(first) = twin {
                    out.push_str(&format!("\n<repo-instructions path=\"{path}\" same-as=\"{first}\"/>"));
                    continue;
                }
                let text = String::from_utf8_lossy(bytes);
                let mut cut = text.len().min(max_bytes).min(budget);
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                budget -= cut;
                out.push_str(&format!("\n<repo-instructions path=\"{path}\">\n{}", text[..cut].trim_end()));
                if cut < text.len() {
                    out.push_str(&format!("\n[truncated at {cut} of {} bytes]", text.len()));
                }
                out.push_str("\n</repo-instructions>");
            }
        }
    }
    if !nested.is_empty() {
        out.push_str("\n<nested-instructions>");
        for p in nested.iter().take(MAX_NESTED) {
            out.push('\n');
            out.push_str(p);
        }
        if nested.len() > MAX_NESTED {
            out.push_str(&format!("\n[{} more]", nested.len() - MAX_NESTED));
        }
        out.push_str("\n</nested-instructions>");
    }
    Some(out)
}

pub struct ContextFiles {
    shared: Arc<Shared>,
}

impl Capability for ContextFiles {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = (FsRead,);
    type Config = ContextFilesConfig;

    fn build((fs,): (FsRead,), cfg: ContextFilesConfig) -> Result<Self, BuildError> {
        let mut basenames: Vec<String> = cfg
            .names
            .iter()
            .filter_map(|n| n.rsplit('/').next())
            .filter(|b| !b.is_empty() && !b.contains(['*', '?', '[', ']', '{', '}', ',', '\\']))
            .map(str::to_string)
            .collect();
        basenames.sort();
        basenames.dedup();
        let shared =
            Shared { fs, cfg, basenames, nested_stale: AtomicBool::new(true), nested: Mutex::new(Vec::new()), text: Mutex::new(None) };
        Ok(ContextFiles { shared: Arc::new(shared) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        if !self.shared.cfg.enabled {
            return;
        }
        let sh = self.shared.clone();
        r.hook(HookSpec {
            on: ON_PRE_STEP,
            if_class: None,
            if_tool: None,
            timeout_ms: HOOK_TIMEOUT_MS,
            once: false,
            // Loading context never vetoes a step; a slow world keeps the last render.
            on_error: OnError::Allow,
            run: Arc::new(move |_, _| {
                let sh = sh.clone();
                Box::pin(async move {
                    sh.refresh().await;
                    HookOutput::default()
                })
            }),
        });
        let sh = self.shared.clone();
        r.post_execute(Arc::new(move |view: &ExecView, _: &ExecOutcome| {
            sh.note_call(&view.args);
            PostDecision::Accept { content: None, deferred_context: Vec::new() }
        }));
        let sh = self.shared.clone();
        r.context(Arc::new(move |_: &State| lock(&sh.text).clone()));
    }
}

declare_capability!(
    ContextFiles,
    "context-files",
    kind = Kind::PromptSection,
    provides = [],
    requires = [Requirement::Seam(aho_fs::SEAM)],
    tools = &[],
    doc = "AGENTS.md / CLAUDE.md from the workspace root (and nested paths) as dynamic context."
);

#[cfg(test)]
mod tests;
