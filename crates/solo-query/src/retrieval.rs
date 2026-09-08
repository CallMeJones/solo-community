// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use solo_core::{Embedder, Error, Result, VectorIndex};
use std::{sync::Arc, time::Duration};

/// Availability of semantic retrieval for this query (including empty results).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    Hybrid,
    LexicalOnly,
}

impl RetrievalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::LexicalOnly => "lexical_only",
        }
    }
}

pub(crate) struct SemanticCandidates {
    pub hits: Vec<(i64, f32)>,
    pub mode: RetrievalMode,
    pub warning: Option<String>,
}

/// Bound query latency even when a configured provider never responds. This
/// timeout does not alter ingestion, re-embedding, or the stored model identity.
const QUERY_EMBEDDING_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn semantic_candidates(
    embedder: &Arc<dyn Embedder>,
    index: &Arc<dyn VectorIndex + Send + Sync>,
    query: &str,
    limit: usize,
) -> Result<SemanticCandidates> {
    semantic_candidates_with_timeout(embedder, index, query, limit, QUERY_EMBEDDING_TIMEOUT).await
}

async fn semantic_candidates_with_timeout(
    embedder: &Arc<dyn Embedder>,
    index: &Arc<dyn VectorIndex + Send + Sync>,
    query: &str,
    limit: usize,
    timeout: Duration,
) -> Result<SemanticCandidates> {
    let embedding = match tokio::time::timeout(timeout, embedder.embed(query)).await {
        Ok(Ok(embedding)) => embedding,
        result => {
            let warning = if result.is_err() {
                "Semantic search timed out; results use keyword search only."
            } else {
                "Semantic search is unavailable; results use keyword search only."
            };
            // Do not include query text or provider error payloads in logs.
            tracing::warn!("{warning}");
            return Ok(SemanticCandidates {
                hits: Vec::new(),
                mode: RetrievalMode::LexicalOnly,
                warning: Some(warning.into()),
            });
        }
    };
    let vector = embedding
        .as_f32_slice()
        .ok_or_else(|| Error::embedder("embedder returned non-F32 vector; HNSW requires F32"))?;
    // Index corruption/configuration errors remain errors; do not hide them as
    // provider outages or substitute a different embedding model.
    Ok(SemanticCandidates {
        hits: index.search(vector, limit)?,
        mode: RetrievalMode::Hybrid,
        warning: None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use solo_core::{Embedding, EmbeddingDtype};

    pub struct UnavailableEmbedder;
    #[async_trait::async_trait]
    impl Embedder for UnavailableEmbedder {
        fn name(&self) -> &str {
            "unavailable"
        }
        fn version(&self) -> &str {
            "v1"
        }
        fn dim(&self) -> usize {
            16
        }
        fn dtype(&self) -> EmbeddingDtype {
            EmbeddingDtype::F32
        }
        async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Embedding>> {
            Err(Error::embedder("provider unavailable"))
        }
    }
    struct HangingEmbedder;
    #[async_trait::async_trait]
    impl Embedder for HangingEmbedder {
        fn name(&self) -> &str {
            "hanging"
        }
        fn version(&self) -> &str {
            "v1"
        }
        fn dim(&self) -> usize {
            16
        }
        fn dtype(&self) -> EmbeddingDtype {
            EmbeddingDtype::F32
        }
        async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Embedding>> {
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn bounds_a_provider_that_never_responds() {
        let embedder: Arc<dyn Embedder> = Arc::new(HangingEmbedder);
        let index: Arc<dyn VectorIndex + Send + Sync> =
            Arc::new(solo_storage::test_support::StubVectorIndex::new(16));
        let candidates = semantic_candidates_with_timeout(
            &embedder,
            &index,
            "query",
            5,
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert_eq!(candidates.mode, RetrievalMode::LexicalOnly);
        assert!(candidates.warning.unwrap().contains("timed out"));
    }
}
