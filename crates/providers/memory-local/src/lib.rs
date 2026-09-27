//! Long-term memory capability: the store, retrieval, the context producer, extraction
//! and maintenance.
//!
//! * **Store**: JSON files `<home>/.aho/memory/<blake3(project) short>.json` (project) and
//!   `<home>/.aho/memory/global.json` (entries tagged `global`). aho-owned state, outside the
//!   workspace.
//! * **Retrieval**: dense (same embedding model only, via the `embed` seam) + BM25
//!   (k1 = 1.2, b = 0.75) fused by RRF (k = 60) over a pool of ≥ 50; then a listwise rerank by
//!   the judge with majority consensus when one is configured and due (first retrieval, topic
//!   change, every `memory.rerank.every_turns`), otherwise only the intersection with the last
//!   verified set (carry-verified degradation); with no judge at all, the dynamic gate.
//! * **Injection**: `Registrar::context` (see [`context`]) with the default suppression rules.
//! * **Extraction**: topic change / every `memory.extract.every_turns` / session end, through the
//!   rule extractor (`remember:` / `note:` lines) plus the LLM extractor when a judge route exists.
//! * **Judge route**: the Registry service `llm:memory` (add a `provider.memory` row on any llm
//!   adapter). The name is fixed because `Deps::resolve` cannot read config.
//! * **Log**: `Memory/*` entries (fold [`aho_memory::fold`]) and skill activations are queued on
//!   the handle (`Memory::drain_events`). `tool-memory` forwards the `Memory/*` customs through
//!   its results' `value.events`; `Cap/Delta` activations stay queued for the session.

#![forbid(unsafe_code)]

pub mod bm25;
pub mod context;
pub mod llm;
pub mod local;
pub mod rules;
pub mod store;

use std::path::PathBuf;
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_embed::EmbedHandle;
use aho_llm::LlmRoute;
use aho_memory::{MIN_POOL, MemoryHandle, NS, Suppression, SystemClock};
use aho_types::Value;
use serde_json::json;

pub use context::{Producer, TopicTracker};
pub use llm::{LlmExtractor, LlmJudge};
pub use local::{LocalMemory, Settings};
pub use rules::RuleExtractor;
pub use store::{GLOBAL_BASE, SKILL_BASE, StorePaths};

/// The `llm:<route>` service used for rerank and extraction when present.
pub const JUDGE_ROUTE: &str = "memory";

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryConfig {
    pub home: Option<PathBuf>,
    pub project: Option<String>,
    pub settings: Settings,
    /// 0 disables the periodic extraction trigger.
    pub extract_every: u32,
    pub judge_model: Option<String>,
}

fn uint(l: &Loadout, key: &str, default: u64, min: u64, expected: &'static str) -> Result<u64, ConfigError> {
    let v = match l.get(key) {
        None | Some(Value::Null) => default,
        Some(v) => v.as_u64().ok_or_else(|| ConfigError::Type { key: key.to_string(), expected })?,
    };
    if v < min {
        return Err(ConfigError::Type { key: key.to_string(), expected });
    }
    Ok(v)
}

fn text(l: &Loadout, key: &str) -> Result<Option<String>, ConfigError> {
    match l.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ConfigError::Type { key: key.to_string(), expected: "a string" }),
    }
}

impl Config for MemoryConfig {
    const PREFIX: &'static str = "memory";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new("memory.home", Ty::String, None, "Directory whose `.aho/memory/` holds the memory store; defaults to $HOME."),
            ConfigKey::new(
                "memory.project",
                Ty::String,
                None,
                "Project identity that keys the per-project memory file; defaults to the working directory.",
            ),
            ConfigKey::new("memory.k", Ty::Int, Some(json!(5)), "Most memories injected per user turn (0 disables injection)."),
            ConfigKey::new("memory.pool", Ty::Int, Some(json!(MIN_POOL)), "Candidates fused and reranked per retrieval (at least 50)."),
            ConfigKey::new(
                "memory.rerank.every_turns",
                Ty::Int,
                Some(json!(3)),
                "With a judge route: rerank at most every N retrievals (always on the first and on a topic change).",
            ),
            ConfigKey::new(
                "memory.rerank.votes",
                Ty::Int,
                Some(json!(1)),
                "Rerank ballots per retrieval; memories chosen by a majority are kept.",
            ),
            ConfigKey::new(
                "memory.extract.every_turns",
                Ty::Int,
                Some(json!(12)),
                "Extract memories every N user turns (0 disables; topic change and session end still extract).",
            ),
            ConfigKey::new(
                "memory.judge.model",
                Ty::String,
                None,
                "Model for rerank and extraction on the `llm:memory` route; defaults to `model.name`.",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        Ok(MemoryConfig {
            home: text(l, "memory.home")?.map(PathBuf::from),
            project: text(l, "memory.project")?,
            settings: Settings {
                k: uint(l, "memory.k", 5, 0, "a non-negative integer")? as usize,
                pool: uint(l, "memory.pool", MIN_POOL as u64, MIN_POOL as u64, "an integer ≥ 50")? as usize,
                rerank_every: uint(l, "memory.rerank.every_turns", 3, 1, "a positive integer")?,
                votes: uint(l, "memory.rerank.votes", 1, 1, "a positive integer")? as u32,
            },
            extract_every: uint(l, "memory.extract.every_turns", 12, 0, "a non-negative integer")? as u32,
            judge_model: match text(l, "memory.judge.model")? {
                Some(m) => Some(m),
                None => text(l, "model.name")?,
            },
        })
    }
}

pub struct MemoryDeps {
    pub embed: EmbedHandle,
    /// `llm:memory`, if an adapter serves that route.
    pub route: Option<LlmRoute>,
}

impl Deps for MemoryDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Embed];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(MemoryDeps { embed: r.handle()?, route: r.service::<LlmRoute>(&aho_llm::service_name(JUDGE_ROUTE)) })
    }
}

pub struct MemoryLocal {
    mem: Arc<LocalMemory>,
    producer: Arc<Producer>,
}

impl MemoryLocal {
    pub fn memory(&self) -> &Arc<LocalMemory> {
        &self.mem
    }
}

impl Capability for MemoryLocal {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = MemoryDeps;
    type Config = MemoryConfig;

    #[allow(clippy::disallowed_methods, reason = "authority: memory-local-config")]
    fn build(deps: MemoryDeps, cfg: MemoryConfig) -> Result<Self, BuildError> {
        let home = cfg.home.or_else(|| std::env::var_os("HOME").map(PathBuf::from)).ok_or_else(|| {
            BuildError::Probe("the memory store needs a home directory and $HOME is unset; set one with `aho set memory.home <dir>`".into())
        })?;
        let project = cfg.project.or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string())).ok_or_else(|| {
            BuildError::Probe(
                "the working directory is unreadable, so the project memory file has no key; set one with `aho set memory.project <name>`"
                    .into(),
            )
        })?;
        let paths = StorePaths::new(&home, &project);
        let mut mem = LocalMemory::open(&paths, &project, deps.embed.0, Arc::new(SystemClock), cfg.settings)
            .map_err(|e| BuildError::Probe(e.to_string()))?;
        if let Some(route) = deps.route {
            let model = cfg.judge_model.ok_or_else(|| BuildError::Config(ConfigError::MissingKey("memory.judge.model".into())))?;
            mem = mem
                .with_judge(Arc::new(LlmJudge::new(route.clone(), model.clone())))
                .with_extractor(Arc::new(LlmExtractor::new(route, model)));
        }
        let mem = Arc::new(mem);
        let producer = Producer::new(mem.clone(), Suppression::default(), cfg.extract_every);
        Ok(MemoryLocal { mem, producer })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(MemoryHandle(self.mem.clone()));
        r.context(self.producer.context_fn());
        r.entry_kind(NS, aho_memory::fold);
        let producer = self.producer.clone();
        r.hook(HookSpec {
            on: &[HookEvent::SessionEnd],
            if_class: None,
            if_tool: None,
            timeout_ms: 60_000,
            once: true,
            on_error: OnError::Deny,
            run: Arc::new(move |_, _| {
                let p = producer.clone();
                Box::pin(async move {
                    p.session_end().await;
                    HookOutput::default()
                })
            }),
        });
    }
}

declare_capability!(
    MemoryLocal,
    "memory-local",
    kind = Kind::Provider,
    provides = [aho_memory::SEAM],
    requires = [Requirement::Seam(aho_embed::SEAM)],
    tools = &[],
    provides_handles = [HandleKind::Custom("memory")],
    entry_kinds = ["Memory/Pruned", "Memory/Maintained", "Memory/Extracted", "Memory/ExtractFailed", "Memory/Error"],
    doc = "Long-term memory under ~/.aho/memory: dense+BM25 → RRF → rerank (llm:memory) or dynamic gate; suppressed Snapshot injection; extraction on topic change / every N turns / session end; logged maintenance."
);
