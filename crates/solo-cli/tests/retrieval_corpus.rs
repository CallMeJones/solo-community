// SPDX-License-Identifier: Apache-2.0

//! Executable baseline for the production hybrid retrieval corpus.

#![cfg(feature = "bundled-embedder")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use solo_core::{Confidence, Embedder, EncodingContext, Episode, MemoryId, Tier};
use solo_storage::{
    BundledEmbedder, HnswParams, InitParams, KeyMaterial, MemoryLibrary, MemoryLibraryParams,
};
use zeroize::Zeroizing;

#[derive(Debug, Deserialize)]
struct Corpus {
    passing_score: f64,
    passing_mrr: f64,
    passing_top_1_safety: f64,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    category: String,
    query: String,
    max_results: usize,
    expected_memory_ids: Vec<String>,
    #[serde(default)]
    forbidden_memory_ids: Vec<String>,
    memories: Vec<Memory>,
}

#[derive(Debug, Deserialize)]
struct Memory {
    id: String,
    text: String,
    importance: f32,
    status: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn bundled_minilm_hybrid_retrieval_meets_versioned_baseline() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/corpora/retrieval-v1.json");
    let raw = std::fs::read_to_string(&path).expect("read retrieval corpus");
    let corpus: Corpus = serde_json::from_str(&raw).expect("parse retrieval corpus");
    let embedder = Arc::new(BundledEmbedder::new());
    let mut recalled = 0usize;
    let mut reciprocal_rank = 0.0_f64;
    let mut vector_supported = 0usize;
    let mut lexical_supported = 0usize;
    let mut top_1_confusions = 0usize;
    let mut diagnostics = Vec::new();
    let mut query_latency_ms = Vec::new();

    for (case_index, case) in corpus.cases.iter().enumerate() {
        let temporary = tempfile::tempdir().expect("temporary retrieval library");
        let passphrase = Zeroizing::new(format!("solo-retrieval-eval-{}", case.id));
        let initialized = solo_storage::init(InitParams {
            data_dir: temporary.path().to_path_buf(),
            passphrase: passphrase.clone(),
            force: false,
            embedder: solo_storage::EmbedderConfig {
                name: solo_storage::BUNDLED_EMBEDDER_NAME.to_string(),
                version: solo_storage::BUNDLED_EMBEDDER_VERSION.to_string(),
                dim: solo_storage::BUNDLED_EMBEDDER_DIM as u32,
                dtype: "f32".to_string(),
            },
        })
        .expect("initialize retrieval library");
        let config = solo_storage::SoloConfig::read(&initialized.config_path).expect("read config");
        let key = KeyMaterial::derive(&passphrase, &config.salt_bytes().expect("salt"))
            .expect("derive key");
        let library = MemoryLibrary::open(MemoryLibraryParams {
            data_dir: temporary.path().to_path_buf(),
            key,
            embedder: embedder.clone(),
            hnsw_params: HnswParams::default(),
            steward: None,
            runtime_handle: Some(tokio::runtime::Handle::current()),
            steward_factory: None,
            triples_batch_signal: None,
        })
        .expect("open retrieval library");
        let handle = library.handle().await.expect("library handle");
        let mut ids = BTreeMap::new();

        for (memory_index, memory) in case.memories.iter().enumerate() {
            let memory_id = MemoryId::new();
            let embedding = embedder.embed(&memory.text).await.expect("embed memory");
            handle
                .write()
                .remember(
                    Episode {
                        memory_id,
                        ts_ms: 1_700_000_000_000
                            + i64::try_from(case_index * 100 + memory_index).expect("small corpus"),
                        source_type: format!("eval:{}", case.category),
                        source_id: None,
                        content: memory.text.clone(),
                        encoding_context: EncodingContext::default(),
                        provenance: None,
                        confidence: Confidence::new(1.0).expect("valid confidence"),
                        strength: memory.importance.clamp(0.0, 1.0),
                        salience: memory.importance.clamp(0.0, 1.0),
                        tier: Tier::Hot,
                    },
                    embedding,
                )
                .await
                .expect("remember eval memory");
            if memory.status != "active" {
                handle
                    .write()
                    .forget(memory_id, format!("eval status: {}", memory.status))
                    .await
                    .expect("apply inactive eval status");
            }
            ids.insert(memory.id.clone(), memory_id.to_string());
        }

        let query_started = Instant::now();
        let result = solo_query::run_recall(&handle, None, &case.query, case.max_results)
            .await
            .expect("run production hybrid recall");
        query_latency_ms.push(query_started.elapsed().as_millis());
        let ranked = result
            .hits
            .iter()
            .map(|hit| hit.memory_id.as_str())
            .collect::<Vec<_>>();
        let first_expected = result.hits.iter().enumerate().find(|(_, hit)| {
            case.expected_memory_ids
                .iter()
                .filter_map(|id| ids.get(id))
                .any(|id| id == &hit.memory_id)
        });
        if let Some((rank, hit)) = first_expected {
            recalled += 1;
            reciprocal_rank += 1.0 / (rank + 1) as f64;
            vector_supported += usize::from(hit.vector_rank.is_some());
            lexical_supported += usize::from(hit.lexical_rank.is_some());
        } else {
            diagnostics.push(format!("{} missed expected memory", case.id));
        }
        let top_result = ranked.first().copied();
        for forbidden in &case.forbidden_memory_ids {
            let forbidden_id = ids.get(forbidden).expect("forbidden memory exists");
            if top_result == Some(forbidden_id.as_str()) {
                top_1_confusions += 1;
                diagnostics.push(format!(
                    "{} confused forbidden memory {forbidden} for the top result",
                    case.id
                ));
            }
        }

        drop(handle);
        library.shutdown_with_snapshot(false).await;
    }

    let recall_at_k = recalled as f64 / corpus.cases.len() as f64;
    let mrr = reciprocal_rank / corpus.cases.len() as f64;
    let top_1_safety = 1.0 - top_1_confusions as f64 / corpus.cases.len() as f64;
    let mean_latency_ms =
        query_latency_ms.iter().sum::<u128>() as f64 / query_latency_ms.len().max(1) as f64;
    let max_latency_ms = query_latency_ms.iter().copied().max().unwrap_or(0);
    eprintln!(
        "retrieval-v1: recall@k={recall_at_k:.3}, mrr={mrr:.3}, top1_safety={top_1_safety:.3}, vector_supported={vector_supported}, lexical_supported={lexical_supported}, mean_query_ms={mean_latency_ms:.1}, max_query_ms={max_latency_ms}"
    );
    if !diagnostics.is_empty() {
        eprintln!("known retrieval weaknesses: {}", diagnostics.join("; "));
    }
    assert!(
        recall_at_k >= corpus.passing_score,
        "hybrid recall@k {recall_at_k:.3} is below the corpus baseline {:.3}",
        corpus.passing_score
    );
    assert!(
        mrr >= corpus.passing_mrr,
        "hybrid MRR {mrr:.3} is below the corpus baseline {:.3}",
        corpus.passing_mrr
    );
    assert!(
        top_1_safety >= corpus.passing_top_1_safety,
        "top-1 safety {top_1_safety:.3} is below the corpus baseline {:.3}",
        corpus.passing_top_1_safety
    );
    assert!(
        vector_supported > 0 && lexical_supported > 0,
        "corpus must exercise both retrieval channels (vector={vector_supported}, lexical={lexical_supported})"
    );
}
