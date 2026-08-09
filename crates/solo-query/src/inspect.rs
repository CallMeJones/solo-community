// SPDX-License-Identifier: Apache-2.0

//! `inspect` — fetch the full record for a single memory_id.
//!
//! Three transports duplicated this; consolidated into one.

use serde::Serialize;
use solo_core::{Error, MemoryId, Result};
use solo_storage::{AuditOperation, AuditWriter, ReaderPool};

/// Full episode record returned by `inspect_one`. Field shape mirrors
/// the SQL columns we expose; no provenance / encoding-context yet —
/// they live in JSON columns and the inspect transports today don't
/// re-render them.
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeRecord {
    pub memory_id: String,
    pub ts_ms: i64,
    pub source_type: String,
    pub source_id: Option<String>,
    pub content: String,
    pub tier: String,
    pub status: String,
    pub confidence: f64,
    pub strength: f64,
    pub salience: f64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Raw JSON string from `episodes.encoding_context_json`. Caller
    /// can `serde_json::from_str` it into an `EncodingContext` if
    /// they want structured access.
    pub encoding_context_json: String,
    /// Raw JSON string from `episodes.provenance_json`. May be None
    /// if the row has no provenance.
    pub provenance_json: Option<String>,
}

/// Fetch one episode by `memory_id`. Returns `Err(NotFound)` if
/// the id has no row in the `episodes` table.
///
/// v0.8.0 P4: emits an audit row after returning. See
/// [`crate::recall::run_recall`] for the audit-emit conventions.
pub async fn inspect_one(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    memory_id: MemoryId,
) -> Result<EpisodeRecord> {
    let target = Some(memory_id.to_string());
    let result = inspect_one_inner(pool, memory_id).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryInspect, target),
        Err(e) => audit.emit_error(audit_principal, AuditOperation::MemoryInspect, target, e),
    }
    result
}

/// Bare lookup with no audit emit. Kept public for tests that want to
/// exercise the SQL path without wiring an `AuditWriter`. Production
/// callers should use [`inspect_one`].
#[doc(hidden)]
pub async fn inspect_one_inner(pool: &ReaderPool, memory_id: MemoryId) -> Result<EpisodeRecord> {
    let id_str = memory_id.to_string();
    let row: Option<EpisodeRecord> = pool
        .interact(move |conn| {
            conn.query_row(
                "SELECT memory_id, ts_ms, source_type, source_id, content,
                        tier, status, confidence, strength, salience,
                        created_at_ms, updated_at_ms,
                        encoding_context_json, provenance_json
                   FROM episodes
                  WHERE memory_id = ?",
                [&id_str],
                |r| {
                    Ok(EpisodeRecord {
                        memory_id: r.get(0)?,
                        ts_ms: r.get(1)?,
                        source_type: r.get(2)?,
                        source_id: r.get(3)?,
                        content: r.get(4)?,
                        tier: r.get(5)?,
                        status: r.get(6)?,
                        confidence: r.get(7)?,
                        strength: r.get(8)?,
                        salience: r.get(9)?,
                        created_at_ms: r.get(10)?,
                        updated_at_ms: r.get(11)?,
                        encoding_context_json: r.get(12)?,
                        provenance_json: r.get(13)?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await?;
    row.ok_or_else(|| Error::not_found(format!("memory_id {memory_id} not found")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_core::{Embedder, VectorIndex};
    use solo_storage::test_support::{StubVectorIndex, fixture_episode, open_test_db_at};
    use solo_storage::{ReaderPool, StubEmbedder, WriterActor, WriterSpawn};
    use std::sync::Arc;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    #[allow(clippy::type_complexity)]
    fn fixture(
        runtime: &tokio::runtime::Runtime,
    ) -> (
        Arc<dyn Embedder>,
        Arc<dyn VectorIndex + Send + Sync>,
        ReaderPool,
        solo_storage::WriteHandle,
        tempfile::TempDir,
        std::thread::JoinHandle<()>,
    ) {
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = 16usize;
        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", dim));
        let conn = open_test_db_at(&tmp.path().join("test.db"));
        let WriterSpawn { handle, join } = WriterActor::spawn(conn, hnsw.clone());
        let path = tmp.path().join("test.db");
        let pool = runtime.block_on(async { ReaderPool::new(&path, None, hnsw.clone()).unwrap() });
        (embedder, hnsw, pool, handle, tmp, join)
    }

    fn shutdown(
        runtime: &tokio::runtime::Runtime,
        pool: ReaderPool,
        handle: solo_storage::WriteHandle,
        tmp: tempfile::TempDir,
        join: std::thread::JoinHandle<()>,
    ) {
        runtime.block_on(async move {
            drop(handle);
            drop(pool);
            drop(tmp);
            tokio::task::spawn_blocking(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(join.join());
                });
                rx.recv_timeout(std::time::Duration::from_secs(5))
                    .expect("writer did not exit within 5s")
                    .expect("writer panicked");
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn inspect_returns_inserted_episode() {
        let runtime = rt();
        let (embedder, _hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let ep = fixture_episode("inspect-content");
            let mid = ep.memory_id;
            handle
                .remember(ep, embedder.embed("inspect-content").await.unwrap())
                .await
                .unwrap();
            let row = inspect_one_inner(&pool, mid).await.unwrap();
            assert_eq!(row.memory_id, mid.to_string());
            assert_eq!(row.content, "inspect-content");
            assert_eq!(row.status, "active");
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn inspect_unknown_id_returns_not_found() {
        let runtime = rt();
        let (_embedder, _hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let mid = solo_core::MemoryId::new();
            let err = inspect_one_inner(&pool, mid).await.unwrap_err();
            assert!(matches!(err, solo_core::Error::NotFound(_)), "got: {err:?}");
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn inspect_after_forget_shows_status_forgotten() {
        let runtime = rt();
        let (embedder, _hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let ep = fixture_episode("forget-then-inspect");
            let mid = ep.memory_id;
            handle
                .remember(ep, embedder.embed("forget-then-inspect").await.unwrap())
                .await
                .unwrap();
            handle.forget(mid, "test".into()).await.unwrap();
            let row = inspect_one_inner(&pool, mid).await.unwrap();
            assert_eq!(row.status, "forgotten");
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }
}
