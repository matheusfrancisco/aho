//! CPU ONNX MiniLM with the official WordPiece tokenizer, masked mean pooling and L2 normalization.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aho_embed::{Embed, EmbedError, ModelId, Vector};
use aho_log::{CapDeltaOp, EntryKind, LogStore};
use aho_types::{BoxFuture, Principal};
use lru::LruCache;
use ort::{ep, session::Session, value::Tensor};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

use crate::DIMS;
use crate::artifacts::{MiniLmArtifacts, unavailable};

const MAX_TOKENS: usize = 256;
const BATCH_SIZE: usize = 16;

type Cache = LruCache<[u8; 32], Vec<f32>>;

struct Runtime {
    session: Session,
    tokenizer: Tokenizer,
}

/// `(major, minor)` of an ONNX Runtime library, read through its stable C entry points. ort
/// 2.0.0-rc.12 builds its "too old" error inside the library `OnceLock` initializer, and building
/// that error re-enters the same lock, so a too-old runtime deadlocks instead of failing: check
/// here first and never hand ort a library it would refuse.
#[allow(unsafe_code)]
fn runtime_version(library: &std::path::Path) -> Result<(u32, u32), EmbedError> {
    let cannot = |what: String| unavailable(format!("ONNX Runtime {}: {what}", library.display()));
    // SAFETY: loading runs the library's initializers exactly as ort's own load would, and
    // `OrtGetApiBase` / `GetVersionString` are the stable C ABI every ONNX Runtime exports; the
    // returned string is static and NUL-terminated per that ABI.
    let text = unsafe {
        let lib = libloading::Library::new(library).map_err(|e| cannot(format!("cannot be loaded ({e})")))?;
        let get: libloading::Symbol<unsafe extern "C" fn() -> *const ort::sys::OrtApiBase> =
            lib.get(b"OrtGetApiBase").map_err(|_| cannot("exports no OrtGetApiBase; it is not ONNX Runtime".into()))?;
        let base = get();
        if base.is_null() {
            return Err(cannot("returned no API base".into()));
        }
        std::ffi::CStr::from_ptr(((*base).GetVersionString)()).to_string_lossy().into_owned()
    };
    let mut parts = text.split('.').map(|p| p.parse::<u32>());
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => Ok((major, minor)),
        _ => Err(cannot(format!("reports an unreadable version `{text}`"))),
    }
}

/// The runtime at `library` as `(major, minor)`, refused unless it is ONNX Runtime 1.23+ (the API
/// level ort is built for here; 1.22.0 crashes creating a session). Loads no model.
pub fn check_onnx_runtime(library: &std::path::Path) -> Result<(u32, u32), EmbedError> {
    let (major, minor) = runtime_version(library)?;
    if (major, minor) < (1, ort::MINOR_VERSION) {
        return Err(unavailable(format!(
            "ONNX Runtime {} is {major}.{minor}; MiniLM needs 1.{} or newer (1.22.0 crashes creating a session). Point embed.local.runtime_library at a newer release",
            library.display(),
            ort::MINOR_VERSION
        )));
    }
    Ok((major, minor))
}

impl Runtime {
    fn load(artifacts: &MiniLmArtifacts, library: &std::path::Path) -> Result<Self, EmbedError> {
        check_onnx_runtime(library)?;
        ort::init_from(library).map_err(|e| unavailable(format!("load ONNX Runtime {}: {e}", library.display())))?.commit();
        let (model, tokenizer) = artifacts.load()?;
        let mut tokenizer = Tokenizer::from_bytes(tokenizer).map_err(unavailable)?;
        tokenizer.with_truncation(Some(TruncationParams { max_length: MAX_TOKENS, ..Default::default() })).map_err(unavailable)?;
        tokenizer.with_padding(Some(PaddingParams::default()));
        let session = Session::builder()
            .map_err(unavailable)?
            .with_execution_providers([ep::CPU::default().with_arena_allocator(false).build()])
            .map_err(unavailable)?
            .with_intra_threads(1)
            .map_err(unavailable)?
            .with_inter_threads(1)
            .map_err(unavailable)?
            .with_memory_pattern(false)
            .map_err(unavailable)?
            .commit_from_memory(&model)
            .map_err(unavailable)?;
        Ok(Self { session, tokenizer })
    }

    fn infer(&mut self, texts: Vec<&str>) -> Result<Vec<Vec<f32>>, EmbedError> {
        let encodings = self.tokenizer.encode_batch(texts, true).map_err(unavailable)?;
        let batch = encodings.len();
        let tokens = encodings.first().map_or(0, |e| e.len());
        if batch == 0 || tokens == 0 || tokens > MAX_TOKENS {
            return Err(unavailable("MiniLM tokenizer produced an invalid sequence length"));
        }
        let mut ids = Vec::with_capacity(batch * tokens);
        let mut mask = Vec::with_capacity(batch * tokens);
        let mut types = Vec::with_capacity(batch * tokens);
        for encoding in encodings {
            if encoding.len() != tokens {
                return Err(unavailable("MiniLM tokenizer did not pad the batch"));
            }
            ids.extend(encoding.get_ids().iter().map(|&id| i64::from(id)));
            mask.extend(encoding.get_attention_mask().iter().map(|&id| i64::from(id)));
            types.extend(encoding.get_type_ids().iter().map(|&id| i64::from(id)));
        }
        let attention = Tensor::from_array(([batch, tokens], mask)).map_err(unavailable)?;
        let outputs = self
            .session
            .run(ort::inputs![
                "input_ids" => Tensor::from_array(([batch, tokens], ids)).map_err(unavailable)?,
                "attention_mask" => &attention,
                "token_type_ids" => Tensor::from_array(([batch, tokens], types)).map_err(unavailable)?
            ])
            .map_err(|e| EmbedError::Transient(format!("MiniLM inference: {e}")))?;
        let output = outputs.get("last_hidden_state").ok_or_else(|| unavailable("MiniLM has no last_hidden_state output"))?;
        let (shape, hidden) = output.try_extract_tensor::<f32>().map_err(unavailable)?;
        if shape.as_ref() != [batch as i64, tokens as i64, DIMS as i64] {
            return Err(unavailable(format!("MiniLM output shape {shape:?} is not [{batch}, {tokens}, {DIMS}]")));
        }
        let (_, mask) = attention.try_extract_tensor::<i64>().map_err(unavailable)?;
        masked_mean(hidden, mask, batch, tokens, DIMS)
    }
}

fn masked_mean(hidden: &[f32], mask: &[i64], batch: usize, tokens: usize, dims: usize) -> Result<Vec<Vec<f32>>, EmbedError> {
    if mask.len() != batch * tokens || hidden.len() != mask.len() * dims || dims == 0 {
        return Err(unavailable("invalid MiniLM pooling tensor dimensions"));
    }
    let mut result = Vec::with_capacity(batch);
    for b in 0..batch {
        let mut pooled = vec![0.0_f32; dims];
        let mut count = 0_u32;
        for t in 0..tokens {
            match mask[b * tokens + t] {
                0 => continue,
                1 => count += 1,
                _ => return Err(unavailable("MiniLM attention mask is not binary")),
            }
            let start = (b * tokens + t) * dims;
            for (out, &value) in pooled.iter_mut().zip(&hidden[start..start + dims]) {
                if !value.is_finite() {
                    return Err(unavailable("MiniLM output contains a non-finite value"));
                }
                *out += value;
            }
        }
        if count == 0 {
            return Err(unavailable("MiniLM attention mask contains no tokens"));
        }
        for value in &mut pooled {
            *value /= count as f32;
        }
        let norm = pooled.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>().sqrt();
        if !norm.is_finite() || norm <= 1e-12 {
            return Err(unavailable("MiniLM produced a zero or non-finite pooled vector"));
        }
        for value in &mut pooled {
            *value = (f64::from(*value) / norm) as f32;
        }
        result.push(pooled);
    }
    Ok(result)
}

struct State {
    runtime: Option<Runtime>,
    cache: Option<Cache>,
    last_used: Instant,
    audit_error: Option<String>,
}

struct Inner {
    artifacts: MiniLmArtifacts,
    library: PathBuf,
    capacity: usize,
    state: Mutex<State>,
    log: Option<Arc<dyn LogStore>>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn delta(&self, op: CapDeltaOp, reason: &str) -> Result<(), EmbedError> {
        if let Some(log) = &self.log {
            log.append(None, Principal::system("embed-local"), EntryKind::CapDelta { id: "embed-local".into(), op, reason: reason.into() })
                .map_err(|e| EmbedError::Transient(format!("embedding lifecycle audit: {e}")))?;
        }
        Ok(())
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vector>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = self.state();
        if let Some(error) = &state.audit_error {
            return Err(EmbedError::Transient(error.clone()));
        }
        if state.runtime.is_none() {
            let runtime = Runtime::load(&self.artifacts, &self.library)?;
            self.delta(CapDeltaOp::Activated, "ModelLoaded")?;
            state.runtime = Some(runtime);
            state.cache = NonZeroUsize::new(self.capacity).map(LruCache::new);
        }
        let result = self.embed_loaded(&mut state, texts);
        // Start idle time at completion, never unload an inference in flight.
        state.last_used = Instant::now();
        result
    }

    fn embed_loaded(&self, state: &mut State, texts: &[String]) -> Result<Vec<Vector>, EmbedError> {
        let mut result = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(BATCH_SIZE) {
            let mut slots: Vec<Option<Vec<f32>>> = vec![None; chunk.len()];
            let mut unique: HashMap<[u8; 32], usize> = HashMap::new();
            let mut pending: Vec<(&str, [u8; 32], Vec<usize>)> = Vec::new();
            for (i, text) in chunk.iter().enumerate() {
                let key: [u8; 32] = Sha256::digest(text.as_bytes()).into();
                if let Some(vector) = state.cache.as_mut().and_then(|cache| cache.get(&key)) {
                    slots[i] = Some(vector.clone());
                } else if let Some(&j) = unique.get(&key) {
                    pending[j].2.push(i);
                } else {
                    unique.insert(key, pending.len());
                    pending.push((text, key, vec![i]));
                }
            }
            if !pending.is_empty() {
                let runtime = state.runtime.as_mut().ok_or_else(|| unavailable("MiniLM runtime is not loaded"))?;
                let vectors = runtime.infer(pending.iter().map(|p| p.0).collect())?;
                for ((_, key, mut positions), vector) in pending.into_iter().zip(vectors) {
                    if let Some(cache) = &mut state.cache {
                        cache.put(key, vector.clone());
                    }
                    if let Some(last) = positions.pop() {
                        for position in positions {
                            slots[position] = Some(vector.clone());
                        }
                        slots[last] = Some(vector);
                    }
                }
            }
            for dims in slots {
                result.push(Vector {
                    model: self.artifacts.model(),
                    dims: dims.ok_or_else(|| unavailable("MiniLM returned fewer vectors than requested"))?,
                });
            }
        }
        Ok(result)
    }

    fn unload(&self, idle: Option<Duration>, reason: &str) -> Result<bool, EmbedError> {
        let mut state = self.state();
        if state.runtime.is_none() || idle.is_some_and(|limit| state.last_used.elapsed() < limit) {
            return Ok(false);
        }
        // Drop session-owned native allocations, tokenizer and LRU allocations before requesting
        // allocator pressure relief. The process-global ORT library/environment remain loaded.
        state.runtime = None;
        state.cache = None;
        crate::purge::allocator_purge();
        if let Err(error) = self.delta(CapDeltaOp::Deactivated, reason) {
            state.audit_error = Some(error.to_string());
            return Err(error);
        }
        Ok(true)
    }
}

/// Real semantic embeddings. Construction verifies artifacts but does not load ONNX Runtime.
/// The library must be ONNX Runtime 1.23 or newer (older ones are refused); a missing/incompatible library is an
/// error, never a hash fallback. Clones share one inference session and bounded cache.
#[derive(Clone)]
pub struct MiniLm {
    inner: Arc<Inner>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MiniLmStatus {
    pub model: ModelId,
    pub loaded: bool,
    pub cached_vectors: usize,
    pub cache_capacity: usize,
    pub idle_ms: u128,
    pub audit_error: Option<String>,
    pub allocator_purge: &'static str,
}

impl MiniLm {
    pub fn new(artifacts: MiniLmArtifacts, capacity: usize, runtime_library: PathBuf) -> Self {
        Self::with_log(artifacts, capacity, runtime_library, None)
    }

    pub fn with_log(artifacts: MiniLmArtifacts, capacity: usize, runtime_library: PathBuf, log: Option<Arc<dyn LogStore>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                artifacts,
                library: runtime_library,
                capacity,
                log,
                state: Mutex::new(State { runtime: None, cache: None, last_used: Instant::now(), audit_error: None }),
            }),
        }
    }

    pub fn status(&self) -> MiniLmStatus {
        let state = self.inner.state();
        MiniLmStatus {
            model: self.model(),
            loaded: state.runtime.is_some(),
            cached_vectors: state.cache.as_ref().map_or(0, LruCache::len),
            cache_capacity: self.inner.capacity,
            idle_ms: state.last_used.elapsed().as_millis(),
            audit_error: state.audit_error.clone(),
            allocator_purge: crate::purge::ENFORCEMENT,
        }
    }

    /// Unload atomically with the idle check, so a concurrent embedding cannot be evicted.
    pub fn unload_if_idle(&self, idle: Duration) -> Result<bool, EmbedError> {
        self.inner.unload(Some(idle), "Idle")
    }

    /// The task retains only a weak reference; dropping all embedders stops the timer.
    pub fn spawn_idle_unload(&self, rt: &tokio::runtime::Handle, idle: Duration) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.inner);
        let tick = (idle / 4).clamp(Duration::from_millis(10), Duration::from_secs(60));
        rt.spawn(async move {
            if idle.is_zero() {
                return;
            }
            loop {
                tokio::time::sleep(tick).await;
                let Some(inner) = weak.upgrade() else { break };
                // Dropping a model and purging a native allocator must not block an async worker.
                match tokio::task::spawn_blocking(move || inner.unload(Some(idle), "Idle")).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) | Err(_) => break, // audit failure is latched and visible in status/next embed
                }
            }
        })
    }
}

impl Embed for MiniLm {
    fn model(&self) -> ModelId {
        self.inner.artifacts.model()
    }

    fn embed(&self, texts: &[String]) -> BoxFuture<'_, Result<Vec<Vector>, EmbedError>> {
        let inner = self.inner.clone();
        let texts = texts.to_vec();
        Box::pin(async move {
            let rt = tokio::runtime::Handle::try_current().map_err(|e| unavailable(format!("MiniLM requires a Tokio runtime: {e}")))?;
            rt.spawn_blocking(move || inner.embed(&texts)).await.map_err(|e| EmbedError::Transient(format!("MiniLM worker: {e}")))?
        })
    }

    fn unload(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // The seam has no error return; failed audit writes are latched in status and cause
            // subsequent embeddings to fail rather than conceal an unaudited transition.
            match self.inner.unload(None, "Explicit") {
                Ok(_) | Err(_) => {}
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_pooling_ignores_padding_and_normalizes_each_sentence() {
        let hidden = [3., 0., 0., 4., f32::NAN, f32::NAN, 0., 2., 0., 2., 0., 2.];
        let result = masked_mean(&hidden, &[1, 1, 0, 1, 1, 1], 2, 3, 2).unwrap();
        assert!((result[0][0] - 0.6).abs() < 1e-6);
        assert!((result[0][1] - 0.8).abs() < 1e-6);
        assert_eq!(result[1], [0., 1.]);
    }

    #[test]
    fn invalid_model_outputs_fail_instead_of_creating_vectors() {
        for (hidden, mask) in [(vec![f32::NAN, 1.], vec![1]), (vec![0., 0.], vec![1]), (vec![1., 2.], vec![0]), (vec![1., 2.], vec![2])] {
            assert!(masked_mean(&hidden, &mask, 1, 1, 2).is_err());
        }
    }
}
