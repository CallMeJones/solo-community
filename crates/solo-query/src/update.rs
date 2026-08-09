// SPDX-License-Identifier: Apache-2.0

//! Correction/update flow for episodic memories.

use serde::Serialize;
use solo_core::{Error, MemoryId, Result};
use solo_storage::LibraryHandle;

#[derive(Debug, Clone, Serialize)]
pub struct MemoryUpdateResult {
    pub memory_id: String,
    pub rowid: i64,
    pub content: String,
    pub updated_at_ms: i64,
}

pub async fn memory_update(
    tenant: &LibraryHandle,
    audit_principal: Option<String>,
    memory_id: MemoryId,
    content: &str,
) -> Result<MemoryUpdateResult> {
    let content = content.trim();
    if content.is_empty() {
        return Err(Error::invalid_input(
            "updated memory content must not be empty",
        ));
    }

    let embedding = tenant.embedder().embed(content).await?;
    if embedding.as_f32_slice().is_none() {
        return Err(Error::embedder(
            "HNSW expects F32 embeddings; convert dtype upstream",
        ));
    }
    let updated = tenant
        .write()
        .update_as(audit_principal, memory_id, content.to_string(), embedding)
        .await?;
    Ok(MemoryUpdateResult {
        memory_id: updated.memory_id.to_string(),
        rowid: updated.rowid,
        content: updated.content,
        updated_at_ms: updated.updated_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use solo_core::{Embedder, VectorIndex};
    use solo_storage::test_support::{StubVectorIndex, open_test_db_at};
    use solo_storage::{
        EmbedderIdentity, ReaderPool, StubEmbedder, WriterActor, WriterSpawn,
        get_or_insert_embedder_id,
    };

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn memory_update_rewrites_content_embedding_and_recall_surface() {
        let runtime = rt();
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let dim = 16usize;
        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", dim));
        let embedder_id = {
            let conn = open_test_db_at(&db_path);
            get_or_insert_embedder_id(
                &conn,
                &EmbedderIdentity {
                    name: "stub".into(),
                    version: "v1".into(),
                    dim: dim as u32,
                    dtype: "f32".into(),
                },
            )
            .unwrap()
        };
        let conn = open_test_db_at(&db_path);
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full(conn, hnsw.clone(), tmp.path().to_path_buf(), embedder_id);
        let pool =
            runtime.block_on(async { ReaderPool::new(&db_path, None, hnsw.clone()).unwrap() });
        let tenant = solo_storage::LibraryHandle::from_parts_for_tests(
            solo_core::LibraryId::default_tenant(),
            solo_storage::config::SoloConfig {
                schema_version: 1,
                salt_hex: "00000000000000000000000000000000".to_string(),
                embedder: solo_storage::config::EmbedderConfig {
                    name: "stub".to_string(),
                    version: "v1".to_string(),
                    dim: dim as u32,
                    dtype: "f32".to_string(),
                },
                identity: solo_storage::IdentityConfig::default(),
                documents: solo_storage::DocumentConfig::default(),
                workspace_file_access: solo_storage::WorkspaceFileAccessConfig::default(),
                auth: None,
                audit: solo_storage::AuditSettings::default(),
                redaction: solo_storage::RedactionConfig::default(),
                llm: None,
                triples: solo_storage::TriplesConfig::default(),
                sampling: solo_storage::SamplingConfig::default(),
                steward: solo_storage::StewardSettings::default(),
            },
            db_path.clone(),
            tmp.path().to_path_buf(),
            embedder_id,
            hnsw.clone(),
            embedder.clone(),
            handle.clone(),
            std::thread::spawn(|| {}),
            pool,
        );

        runtime.block_on(async {
            let ep = solo_storage::test_support::fixture_episode("old correction text");
            let memory_id = ep.memory_id;
            handle
                .remember(ep, embedder.embed("old correction text").await.unwrap())
                .await
                .unwrap();

            let updated = memory_update(&tenant, None, memory_id, "new correction text")
                .await
                .unwrap();
            assert_eq!(updated.content, "new correction text");

            let recalled = crate::recall::run_recall_inner(
                &embedder,
                &hnsw,
                tenant.read(),
                "new correction text",
                5,
            )
            .await
            .unwrap();
            assert!(
                recalled
                    .hits
                    .iter()
                    .any(|hit| hit.memory_id == memory_id.to_string()
                        && hit.content == "new correction text"),
                "updated memory should recall with new content: {recalled:#?}"
            );
        });

        runtime.block_on(async move {
            drop(handle);
            drop(tenant);
            drop(tmp);
        });
        join.join().unwrap();
    }
}
