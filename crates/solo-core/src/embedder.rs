// SPDX-License-Identifier: Apache-2.0

//! `Embedder` trait. See ADR-0002 for design rationale.

use crate::{
    error::{Error, Result},
    types::{Embedding, EmbeddingDtype},
};
use async_trait::async_trait;

/// Pluggable embedder. Production impls live in `solo-storage` (or a future
/// `solo-embed` crate); this trait is the contract.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embedder identity. The migration tool `solo reembed` keys on
    /// `(name, version)` to decide whether stored embeddings need to be
    /// regenerated.
    fn name(&self) -> &str;

    /// Embedder version. Bump on any change that produces different vectors
    /// for the same input.
    fn version(&self) -> &str;

    /// Output dimension. Must be invariant across calls for a given Embedder
    /// instance.
    fn dim(&self) -> usize;

    /// Output dtype. Determines how raw bytes in `Embedding::data` are
    /// interpreted.
    fn dtype(&self) -> EmbeddingDtype;

    /// Optional non-embedding health endpoint for runtime status checks.
    ///
    /// Implementations should return a probe that does not load model
    /// weights or mutate backend state. The default keeps in-process
    /// embedders simple and lets transports report them as loaded.
    fn runtime_probe_url(&self) -> Option<String> {
        None
    }

    /// Embed a batch of texts. Output is in input order with the same length
    /// as the input. Implementations should batch internally for throughput;
    /// callers may pass any number of texts (including 1).
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Embedding>>;

    /// Convenience: embed a single text. Default impl calls embed_batch.
    async fn embed(&self, text: &str) -> Result<Embedding> {
        let mut results = self.embed_batch(&[text]).await?;
        results.pop().ok_or(Error::EmbedderProtocol(
            "embed_batch returned empty for non-empty input",
        ))
    }
}
