//! `12` — local embeddings: explicit lexical hashing or optional real ONNX MiniLM.
//!
//! The default **`hash-ngram-384`** is a deterministic feature-hashed TF-IDF embedder.
//! Features are word unigrams plus character 3-grams of each boundary-padded word
//! (`<cat>` → `<ca`, `cat`, `at>`). Term frequency is sublinear (`ln(1 + tf)`); the IDF factor
//! is a fixed prior (English stopwords weigh 0.1, every other term 1.0) rather than corpus
//! statistics, so a text maps to the same vector forever and stored vectors stay comparable.
//! Each feature is hashed (FNV-1a + murmur finalizer) to one of 384 buckets with a hash-derived
//! sign, and the result is L2-normalized. Lexical, not semantic: paraphrases sharing no
//! words or word pieces score low — BM25 fusion in the memory capability covers exact terms.
//!
//! Feature `minilm` adds pinned `all-MiniLM-L6-v2` ONNX inference. Select it with
//! `embed.local.model`, configure `model_dir` and `runtime_library`, and provision artifacts
//! explicitly with [`provision_minilm`] before embedding. No inference path uses the network
//! or falls back to a different model. Without the feature, selecting MiniLM is an error.
//!
//! Both variants have bounded vector caches. MiniLM additionally drops its native session,
//! tokenizer and cache after idle time, asks the system allocator to purge where supported,
//! and appends actual activation/deactivation deltas to the composition's `aho.log` service.

#![deny(unsafe_code)]

#[cfg(feature = "minilm")]
mod artifacts;
#[cfg(feature = "minilm")]
mod minilm;
#[cfg(feature = "minilm")]
mod purge;
#[cfg(feature = "minilm")]
pub use artifacts::{MINILM_MODEL, MINILM_REVISION, MiniLmArtifacts, provision_minilm};
#[cfg(feature = "minilm")]
pub use minilm::{MiniLm, MiniLmStatus, check_onnx_runtime};

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_embed::{Embed, EmbedError, EmbedHandle, ModelId, Vector};
use aho_log::LogStore;
use aho_types::{BoxFuture, Value};
use serde_json::json;

pub const MODEL: &str = "hash-ngram-384";
pub const DIMS: usize = 384;
#[cfg(not(feature = "minilm"))]
pub const ENFORCED_BY: &str = "hash-ngram-384 lexical hashing, offline; ONNX MiniLM feature not compiled";
#[cfg(feature = "minilm")]
pub const ENFORCED_BY: &str = "selected hash-ngram-384 or verified all-MiniLM-L6-v2 ONNX CPU inference; no model fallback; idle session/cache release and best-effort system allocator purge";

/// Share of each word's character 3-grams relative to the word itself (≈ equal energy for
/// a six-letter word: 6 × 0.4² ≈ 1).
const TRIGRAM_WEIGHT: f32 = 0.4;
/// Fixed IDF prior for function words; they still count, just barely.
const STOPWORD_WEIGHT: f32 = 0.1;

const STOPWORDS: &[&str] = &[
    "a", "about", "above", "after", "again", "against", "all", "am", "an", "and", "any", "are", "as", "at", "be", "because", "been",
    "before", "being", "below", "between", "both", "but", "by", "can", "could", "did", "do", "does", "doing", "down", "during", "each",
    "few", "for", "from", "further", "had", "has", "have", "having", "he", "her", "here", "hers", "him", "his", "how", "i", "if", "in",
    "into", "is", "it", "its", "just", "me", "more", "most", "my", "no", "nor", "not", "now", "of", "off", "on", "once", "only", "or",
    "other", "our", "ours", "out", "over", "own", "same", "she", "should", "so", "some", "such", "than", "that", "the", "their", "them",
    "then", "there", "these", "they", "this", "those", "through", "to", "too", "under", "until", "up", "very", "was", "we", "were", "what",
    "when", "where", "which", "while", "who", "whom", "why", "will", "with", "would", "you", "your",
];

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// murmur3 `fmix64`: spreads FNV's weak low bits before the bucket modulo.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// The embedding itself: pure, deterministic, L2-normalized (all zeros for text with no words).
pub fn hash_ngram_vector(text: &str) -> Vec<f32> {
    // Weighted term counts, keyed by feature string; BTreeMap keeps summation order fixed so
    // the output is bit-identical across runs and processes.
    let mut feats: BTreeMap<String, f32> = BTreeMap::new();
    for w in words(text) {
        let idf = if STOPWORDS.contains(&w.as_str()) { STOPWORD_WEIGHT } else { 1.0 };
        let padded: Vec<char> = std::iter::once('<').chain(w.chars()).chain(std::iter::once('>')).collect();
        for gram in padded.windows(3) {
            let key: String = std::iter::once('c').chain(std::iter::once('\u{1}')).chain(gram.iter().copied()).collect();
            *feats.entry(key).or_insert(0.0) += idf;
        }
        *feats.entry(format!("w\u{1}{w}")).or_insert(0.0) += idf;
    }
    let mut dims = vec![0.0f32; DIMS];
    for (key, tf) in &feats {
        let h = mix(fnv1a(key.as_bytes()));
        let bucket = (h % DIMS as u64) as usize;
        let sign = if h >> 63 == 1 { -1.0 } else { 1.0 };
        let block = if key.starts_with('c') { TRIGRAM_WEIGHT } else { 1.0 };
        dims[bucket] += sign * block * tf.ln_1p();
    }
    let norm = dims.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        dims.iter_mut().for_each(|x| *x /= norm);
    }
    dims
}

#[derive(Default)]
struct Lru {
    map: HashMap<String, (Vec<f32>, u64)>,
    tick: u64,
}

/// `hash-ngram-384` behind the `Embed` seam, with a bounded LRU of recent vectors.
pub struct HashNgram {
    cache: Mutex<Lru>,
    capacity: usize,
    born: Instant,
    last_used_ms: AtomicU64,
}

impl HashNgram {
    /// `capacity = 0` disables the vector cache.
    pub fn new(capacity: usize) -> HashNgram {
        HashNgram { cache: Mutex::new(Lru::default()), capacity, born: Instant::now(), last_used_ms: AtomicU64::new(0) }
    }

    fn lru(&self) -> MutexGuard<'_, Lru> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn touch(&self) {
        self.last_used_ms.store(self.born.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Time since the last `embed` call (since construction if never used).
    pub fn idle_for(&self) -> Duration {
        self.born.elapsed().saturating_sub(Duration::from_millis(self.last_used_ms.load(Ordering::Relaxed)))
    }

    /// Vectors currently held in the cache.
    pub fn resident(&self) -> usize {
        self.lru().map.len()
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        if self.capacity == 0 {
            return hash_ngram_vector(text);
        }
        {
            let mut lru = self.lru();
            lru.tick += 1;
            let tick = lru.tick;
            if let Some((v, t)) = lru.map.get_mut(text) {
                *t = tick;
                return v.clone();
            }
        }
        let v = hash_ngram_vector(text);
        let mut lru = self.lru();
        if lru.map.len() >= self.capacity
            && let Some(oldest) = lru.map.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone())
        {
            lru.map.remove(&oldest);
        }
        lru.tick += 1;
        let tick = lru.tick;
        lru.map.insert(text.to_string(), (v.clone(), tick));
        v
    }
}

impl Embed for HashNgram {
    fn model(&self) -> ModelId {
        ModelId::new(MODEL)
    }

    fn embed(&self, texts: &[String]) -> BoxFuture<'_, Result<Vec<Vector>, EmbedError>> {
        self.touch();
        let out = texts.iter().map(|t| Vector { model: self.model(), dims: self.vector(t) }).collect();
        Box::pin(async move { Ok(out) })
    }

    fn unload(&self) -> BoxFuture<'_, ()> {
        // Replace rather than clear so the map's allocation is released too.
        *self.lru() = Lru::default();
        Box::pin(async {})
    }
}

/// Drops the embedder's cache once it has been idle for `idle`, checking every `tick`.
/// Holds only a weak reference: the task ends when the last handle to the embedder is gone.
pub fn spawn_idle_unload(
    rt: &tokio::runtime::Handle,
    embedder: &Arc<HashNgram>,
    idle: Duration,
    tick: Duration,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(embedder);
    rt.spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            let Some(e) = weak.upgrade() else { break };
            if e.resident() > 0 && e.idle_for() >= idle {
                e.unload().await;
            }
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbedLocalConfig {
    pub model: String,
    pub model_dir: String,
    pub runtime_library: String,
    pub cache: usize,
    /// Zero disables idle unloading.
    pub idle_unload: Duration,
}

fn uint(l: &Loadout, key: &str, default: u64) -> Result<u64, ConfigError> {
    match l.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().ok_or_else(|| ConfigError::Type { key: key.to_string(), expected: "a non-negative integer" }),
    }
}

fn string(l: &Loadout, key: &str, default: &str) -> Result<String, ConfigError> {
    match l.get(key) {
        None | Some(Value::Null) => Ok(default.to_string()),
        Some(v) => v.as_str().map(str::to_string).ok_or_else(|| ConfigError::Type { key: key.to_string(), expected: "a string" }),
    }
}

impl Config for EmbedLocalConfig {
    const PREFIX: &'static str = "embed.local";

    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "embed.local.model",
                Ty::String,
                Some(json!(MODEL)),
                "Embedding model: hash-ngram-384 (lexical) or all-MiniLM-L6-v2 (semantic, requires the minilm feature).",
            ),
            ConfigKey::new(
                "embed.local.model_dir",
                Ty::String,
                Some(json!("")),
                "Explicit directory containing provisioned MiniLM model.onnx and tokenizer.json; inference never downloads.",
            ),
            ConfigKey::new(
                "embed.local.runtime_library",
                Ty::String,
                Some(json!("")),
                "Explicit ONNX Runtime shared library path (1.23 or newer; older runtimes are refused), required when selecting MiniLM.",
            ),
            ConfigKey::new(
                "embed.local.cache",
                Ty::Int,
                Some(json!(1024)),
                "Recent vectors kept in memory by the local embedder (0 disables the cache).",
            ),
            ConfigKey::new(
                "embed.local.idle_unload_s",
                Ty::Int,
                Some(json!(300)),
                "Seconds without an embedding request before dropping the model and vector cache (0 never unloads).",
            ),
        ]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        Ok(EmbedLocalConfig {
            model: string(l, "embed.local.model", MODEL)?,
            model_dir: string(l, "embed.local.model_dir", "")?,
            runtime_library: string(l, "embed.local.runtime_library", "")?,
            cache: usize::try_from(uint(l, "embed.local.cache", 1024)?)
                .map_err(|_| ConfigError::Type { key: "embed.local.cache".into(), expected: "an integer that fits this platform" })?,
            idle_unload: Duration::from_secs(uint(l, "embed.local.idle_unload_s", 300)?),
        })
    }
}

/// The log is an existing composition service, not an ambient process-global sink.
pub struct EmbedLocalDeps {
    pub log: Option<Arc<dyn LogStore>>,
}

impl Deps for EmbedLocalDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(Self { log: r.service("aho.log") })
    }
}

enum Backend {
    Hash(Arc<HashNgram>),
    #[cfg(feature = "minilm")]
    MiniLm(Arc<MiniLm>),
}

pub struct EmbedLocal {
    embedder: Backend,
    idle: Duration,
}

impl Capability for EmbedLocal {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = EmbedLocalDeps;
    type Config = EmbedLocalConfig;

    fn build(deps: EmbedLocalDeps, cfg: EmbedLocalConfig) -> Result<Self, BuildError> {
        let embedder = match cfg.model.as_str() {
            MODEL => Backend::Hash(Arc::new(HashNgram::new(cfg.cache))),
            "all-MiniLM-L6-v2" => {
                #[cfg(feature = "minilm")]
                {
                    if cfg.model_dir.is_empty() || cfg.runtime_library.is_empty() {
                        return Err(BuildError::Probe(
                            "MiniLM requires embed.local.model_dir and embed.local.runtime_library; provision artifacts explicitly first"
                                .into(),
                        ));
                    }
                    let log =
                        deps.log.ok_or(BuildError::Unsupported("MiniLM lifecycle auditing requires the composition's aho.log service"))?;
                    let artifacts =
                        MiniLmArtifacts::open(std::path::Path::new(&cfg.model_dir)).map_err(|e| BuildError::Probe(e.to_string()))?;
                    Backend::MiniLm(Arc::new(MiniLm::with_log(artifacts, cfg.cache, cfg.runtime_library.into(), Some(log))))
                }
                #[cfg(not(feature = "minilm"))]
                {
                    return Err(BuildError::Unsupported(
                        "all-MiniLM-L6-v2 requires rebuilding with feature minilm; no lexical fallback was used",
                    ));
                }
            }
            _ => {
                return Err(BuildError::Probe(format!(
                    "unknown embedding model {:?}; select hash-ngram-384 or all-MiniLM-L6-v2",
                    cfg.model
                )));
            }
        };
        #[cfg(not(feature = "minilm"))]
        drop(deps);
        Ok(Self { embedder, idle: cfg.idle_unload })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        match &self.embedder {
            Backend::Hash(e) => r.provide(EmbedHandle(e.clone())),
            #[cfg(feature = "minilm")]
            Backend::MiniLm(e) => r.provide(EmbedHandle(e.clone())),
        }
        if !self.idle.is_zero()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            match &self.embedder {
                Backend::Hash(e) => {
                    let tick = (self.idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(60));
                    spawn_idle_unload(&rt, e, self.idle, tick);
                }
                #[cfg(feature = "minilm")]
                Backend::MiniLm(e) => {
                    e.spawn_idle_unload(&rt, self.idle);
                }
            }
        }
    }
}

declare_capability!(
    EmbedLocal,
    "embed-local",
    kind = Kind::Provider,
    provides = [aho_embed::SEAM],
    requires = [],
    tools = &[],
    provides_handles = [HandleKind::Embed],
    enforced_by = ENFORCED_BY,
    doc = "Explicit hash-ngram-384 lexical or optional all-MiniLM-L6-v2 ONNX semantic embeddings; bounded LRU, idle unloading, identity isolation, no model fallback (12)."
);

#[cfg(test)]
mod tests {
    use super::*;
    use aho_embed::similarity;

    fn embed(e: &HashNgram, t: &str) -> Vector {
        block(e.embed(&[t.to_string()])).unwrap().remove(0)
    }

    /// Drives a future to completion on a throwaway current-thread runtime.
    fn block<T>(f: BoxFuture<'_, T>) -> T {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(f)
    }

    #[test]
    fn deterministic_across_instances_and_normalized() {
        let text = "Cargo builds the workspace with `cargo nextest run` in CI";
        let a = embed(&HashNgram::new(8), text);
        let b = embed(&HashNgram::new(0), text);
        assert_eq!(a.dims, b.dims, "bit-identical regardless of instance or cache");
        assert_eq!(a.dims, hash_ngram_vector(text));
        assert_eq!(a.dims.len(), DIMS);
        assert_eq!(a.model, ModelId::new(MODEL));
        assert!((a.norm() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn similar_texts_are_closer_than_unrelated_ones() {
        let e = HashNgram::new(16);
        let q = embed(&e, "the database connection pool times out under load");
        let near = embed(&e, "database connection pool timeout errors under heavy load");
        let far = embed(&e, "my cat enjoys sleeping on sunny windowsills");
        let s_near = similarity(&q, &near).unwrap();
        let s_far = similarity(&q, &far).unwrap();
        assert!(s_near > 0.5, "near = {s_near}");
        assert!(s_far < 0.2, "far = {s_far}");
        assert!(s_near > s_far + 0.3);
    }

    #[test]
    fn stopwords_barely_move_the_vector() {
        let a = hash_ngram_vector("deploy the service to staging");
        let b = hash_ngram_vector("deploy service staging");
        let stop = hash_ngram_vector("the of and to is");
        let cos = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>();
        assert!(cos(&a, &b) > 0.95, "{}", cos(&a, &b));
        assert!(cos(&a, &stop) < 0.2);
    }

    #[test]
    fn empty_text_is_the_zero_vector() {
        let z = hash_ngram_vector("  … ,;  ");
        assert!(z.iter().all(|x| *x == 0.0));
        let e = HashNgram::new(4);
        assert_eq!(similarity(&embed(&e, ""), &embed(&e, "anything")), Some(0.0));
    }

    #[test]
    fn lru_is_bounded_and_unload_drops_it() {
        let e = HashNgram::new(2);
        embed(&e, "one");
        embed(&e, "two");
        embed(&e, "one"); // refresh `one`; `two` is now oldest
        embed(&e, "three");
        assert_eq!(e.resident(), 2);
        assert!(e.lru().map.contains_key("one") && e.lru().map.contains_key("three"));
        block(e.unload());
        assert_eq!(e.resident(), 0);
        assert_eq!(embed(&e, "one").dims, hash_ngram_vector("one"), "reload after unload is transparent");
    }

    #[tokio::test]
    async fn idle_sweeper_unloads_and_stops_with_the_embedder() {
        let e = Arc::new(HashNgram::new(8));
        e.embed(&["warm".to_string()]).await.unwrap();
        let task = spawn_idle_unload(&tokio::runtime::Handle::current(), &e, Duration::from_millis(40), Duration::from_millis(10));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(e.resident(), 0, "idle cache dropped");
        drop(e);
        tokio::time::timeout(Duration::from_secs(2), task).await.expect("sweeper exits once the embedder is gone").unwrap();
    }

    #[test]
    fn config_rejects_non_integer_cache_capacity() {
        use aho_config::{ResolveInputs, Schema, resolve};
        let mut schema = Schema::default();
        EmbedLocalConfig::keys().into_iter().for_each(|k| schema.add(k));

        let mut i = ResolveInputs::default();
        i.sets = vec![("embed.local.cache".into(), "\"lots\"".into())];
        let l = resolve(&i, &schema, false).unwrap();
        assert!(matches!(EmbedLocalConfig::from_loadout(&l), Err(ConfigError::Type { key, .. }) if key == "embed.local.cache"));
    }
}
