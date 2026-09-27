//! Embeddings. Every stored vector carries its model id; dense similarity only compares
//! same-model vectors (`similarity` returns `None` across models, never a silent 0). Providers
//! drop resident state on `unload` (idle unload, allocator purge).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::Arc;

use aho_cap::{Handle, HandleKind, SeamId};
use aho_types::BoxFuture;
use serde::{Deserialize, Serialize};

pub const SEAM: SeamId = SeamId("embed");

/// Names the vector space a `Vector` lives in (`hash-ngram-384`, `all-MiniLM-L6-v2`, …).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    pub fn new(id: impl Into<String>) -> ModelId {
        ModelId(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vector {
    pub model: ModelId,
    pub dims: Vec<f32>,
}

impl Vector {
    pub fn norm(&self) -> f32 {
        self.dims.iter().map(|x| x * x).sum::<f32>().sqrt()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbedError {
    /// The model cannot be loaded (missing weights, unsupported platform); retrying will not help.
    ModelUnavailable(String),
    /// A retry may succeed (busy runtime, interrupted download).
    Transient(String),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmbedError::ModelUnavailable(d) => write!(f, "the embedding model is unavailable: {d}"),
            EmbedError::Transient(d) => write!(f, "embedding failed temporarily ({d}); the call can be retried"),
        }
    }
}

impl std::error::Error for EmbedError {}

pub trait Embed: Send + Sync {
    fn model(&self) -> ModelId;
    /// One vector per input text, in order, all tagged with `model()`.
    fn embed(&self, texts: &[String]) -> BoxFuture<'_, Result<Vec<Vector>, EmbedError>>;
    /// Drop resident state (weights, vector cache) after an idle period; the next `embed` reloads.
    fn unload(&self) -> BoxFuture<'_, ()>;
}

#[derive(Clone)]
pub struct EmbedHandle(pub Arc<dyn Embed>);
impl Handle for EmbedHandle {
    const KIND: HandleKind = HandleKind::Embed;
}

/// Cosine only when `a.model == b.model` and both have the same dimensionality; `None` otherwise
/// (invariant, never silently 0). A zero vector has no direction, so its similarity is `0.0`.
pub fn similarity(a: &Vector, b: &Vector) -> Option<f32> {
    if a.model != b.model || a.dims.len() != b.dims.len() {
        return None;
    }
    let (na, nb) = (a.norm(), b.norm());
    if na == 0.0 || nb == 0.0 {
        return Some(0.0);
    }
    let dot: f32 = a.dims.iter().zip(&b.dims).map(|(x, y)| x * y).sum();
    Some((dot / (na * nb)).clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(model: &str, dims: &[f32]) -> Vector {
        Vector { model: ModelId::new(model), dims: dims.to_vec() }
    }

    #[test]
    fn cross_model_similarity_is_none_even_for_identical_dims() {
        let a = v("hash-ngram-384", &[1.0, 0.0]);
        let b = v("all-MiniLM-L6-v2", &[1.0, 0.0]);
        assert_eq!(similarity(&a, &b), None);
        assert_eq!(similarity(&a, &a.clone()), Some(1.0));
    }

    #[test]
    fn cosine_is_scale_invariant_and_signed() {
        let a = v("m", &[1.0, 1.0]);
        let b = v("m", &[3.0, 3.0]);
        let c = v("m", &[-1.0, -1.0]);
        let d = v("m", &[1.0, -1.0]);
        assert!((similarity(&a, &b).unwrap() - 1.0).abs() < 1e-6);
        assert!((similarity(&a, &c).unwrap() + 1.0).abs() < 1e-6);
        assert!(similarity(&a, &d).unwrap().abs() < 1e-6);
    }

    #[test]
    fn mismatched_dimensionality_and_zero_vectors() {
        assert_eq!(similarity(&v("m", &[1.0]), &v("m", &[1.0, 0.0])), None);
        assert_eq!(similarity(&v("m", &[0.0, 0.0]), &v("m", &[1.0, 0.0])), Some(0.0));
    }

    #[test]
    fn vectors_round_trip_with_their_model() {
        let a = v("hash-ngram-384", &[0.5, -0.25]);
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, r#"{"model":"hash-ngram-384","dims":[0.5,-0.25]}"#);
        assert_eq!(serde_json::from_str::<Vector>(&json).unwrap(), a);
    }
}
