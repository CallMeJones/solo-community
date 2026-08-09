// SPDX-License-Identifier: Apache-2.0

//! Deterministic stub embedder.
//!
//! `StubEmbedder` produces fixed F32 vectors from text via BLAKE3 hashing,
//! one hash per dimension. Properties:
//!
//!   - **Deterministic**: same `(text, dim)` → same vector across runs.
//!   - **Distinct inputs → distinct vectors** with negligible collision rate
//!     (each dim is an independent BLAKE3 hash).
//!   - **Unit-normalised**: cosine similarity is well-defined; identical
//!     inputs produce vectors with cosine = 1.0.
//!
//! Used by unit tests and as a placeholder in the daemon main until
//! BGE-M3 loading lands (commit 1.4.b). NOT suitable for real semantic
//! search — outputs have no semantic meaning, just identity.

use async_trait::async_trait;
use solo_core::{Embedder, Embedding, EmbeddingDtype, Result};

/// Public identity for the StubEmbedder. Production code paths
/// (consolidation, recall) compare `embedder.name()` against this
/// constant to refuse running with the stub unless explicitly opted in.
/// See dev-log 0152 H2.
pub const STUB_EMBEDDER_NAME: &str = "stub";

const DEFAULT_NAME: &str = STUB_EMBEDDER_NAME;
const DEFAULT_VERSION: &str = "v1";
const DEFAULT_DIM: usize = 32;

/// Deterministic, unit-normalised F32 embedder for tests + bring-up.
#[derive(Debug, Clone)]
pub struct StubEmbedder {
    name: String,
    version: String,
    dim: usize,
}

impl StubEmbedder {
    /// Default stub: name=`stub`, version=`v1`, dim=32.
    pub fn default_stub() -> Self {
        Self::new(DEFAULT_NAME, DEFAULT_VERSION, DEFAULT_DIM)
    }

    pub fn new(name: impl Into<String>, version: impl Into<String>, dim: usize) -> Self {
        assert!(dim > 0, "stub embedder dim must be > 0");
        Self {
            name: name.into(),
            version: version.into(),
            dim,
        }
    }

    /// One-shot: hash `text` to `dim` floats; normalise; return the
    /// little-endian-encoded byte buffer.
    fn embed_one(&self, text: &str) -> Embedding {
        let mut data = vec![0u8; self.dim * 4];
        for d in 0..self.dim {
            let mut h = blake3::Hasher::new();
            h.update(text.as_bytes());
            h.update(&(d as u32).to_le_bytes());
            // Take 4 bytes from the XOF stream → u32 → centred [-1.0, 1.0]
            let mut bytes = [0u8; 4];
            h.finalize_xof().fill(&mut bytes);
            let raw = u32::from_le_bytes(bytes) as f64 / u32::MAX as f64;
            let val = (raw * 2.0 - 1.0) as f32;
            data[d * 4..(d + 1) * 4].copy_from_slice(&val.to_le_bytes());
        }

        // Unit-normalise.
        let mut sum_sq = 0.0f32;
        for chunk in data.chunks_exact(4) {
            let v = f32::from_le_bytes(chunk.try_into().unwrap());
            sum_sq += v * v;
        }
        let norm = sum_sq.sqrt().max(1e-9);
        for chunk in data.chunks_exact_mut(4) {
            let v = f32::from_le_bytes((&chunk[..]).try_into().unwrap()) / norm;
            chunk.copy_from_slice(&v.to_le_bytes());
        }

        Embedding {
            dtype: EmbeddingDtype::F32,
            dim: self.dim,
            data,
        }
    }
}

#[async_trait]
impl Embedder for StubEmbedder {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        &self.version
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn dtype(&self) -> EmbeddingDtype {
        EmbeddingDtype::F32
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Embedding>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn embedding_is_deterministic_for_same_input() {
        let e = StubEmbedder::default_stub();
        let a = rt().block_on(e.embed("hello world")).unwrap();
        let b = rt().block_on(e.embed("hello world")).unwrap();
        assert_eq!(a.data, b.data);
        assert_eq!(a.dim, e.dim());
        assert_eq!(a.dtype, EmbeddingDtype::F32);
    }

    #[test]
    fn distinct_inputs_produce_distinct_embeddings() {
        let e = StubEmbedder::default_stub();
        let a = rt().block_on(e.embed("alpha")).unwrap();
        let b = rt().block_on(e.embed("beta")).unwrap();
        assert_ne!(a.data, b.data);
    }

    #[test]
    fn output_is_unit_normalised() {
        let e = StubEmbedder::new("stub", "v1", 16);
        let v = rt().block_on(e.embed("normalise me")).unwrap();
        let slice = v.as_f32_slice().unwrap();
        let norm: f32 = slice.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    #[test]
    fn embedding_validates_against_dim_and_dtype() {
        let e = StubEmbedder::new("stub", "v1", 24);
        let v = rt().block_on(e.embed("validate")).unwrap();
        v.validate()
            .expect("embedding must satisfy length invariant");
        assert_eq!(v.data.len(), 24 * 4);
    }

    #[test]
    fn batch_preserves_input_order() {
        let e = StubEmbedder::default_stub();
        let inputs = ["one", "two", "three", "four"];
        let outputs = rt().block_on(e.embed_batch(&inputs)).unwrap();
        assert_eq!(outputs.len(), inputs.len());
        // Verify each batch entry matches the single-call result.
        for (i, txt) in inputs.iter().enumerate() {
            let single = rt().block_on(e.embed(txt)).unwrap();
            assert_eq!(outputs[i].data, single.data);
        }
    }

    #[test]
    fn cosine_self_similarity_is_one() {
        // Useful sanity check for downstream HNSW recall tests.
        let e = StubEmbedder::default_stub();
        let v = rt().block_on(e.embed("self")).unwrap();
        let s = v.as_f32_slice().unwrap();
        let dot: f32 = s.iter().map(|x| x * x).sum();
        assert!(
            (dot - 1.0).abs() < 1e-4,
            "self dot product (= cosine) ≈ 1, got {dot}"
        );
    }
}
