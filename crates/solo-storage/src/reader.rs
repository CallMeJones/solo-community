// SPDX-License-Identifier: Apache-2.0

//! `ReaderPool`: pool of read-only SQLite connections backed by
//! `deadpool-sqlite`. Each newly-created connection has its raw SQLCipher
//! key bound via a `post_create` hook (PBKDF2 cost paid once per
//! connection, not per query). See ADR-0003 §"Trait shapes" and §P8-A/P8-B.
//!
//! Pool size defaults to 2 per ADR-0003 §"pool size" — the SQLite WAL gives
//! us snapshot isolation, but the single-writer constraint means more than
//! a couple of concurrent reads against a normal hard drive saturate IO
//! before they help latency.
//!
//! The same `Arc<dyn VectorIndex + Send + Sync>` that the writer holds is
//! shared with the pool so vector search runs on whichever Tokio task
//! happens to call `interact`. Concurrency is provided by `hnsw_rs`'s
//! internal `parking_lot::RwLock`, not by the pool.
//!
//! ## A note on lifetime in tests
//!
//! `deadpool-sqlite::Pool::drop` schedules cleanup via the current Tokio
//! runtime. If the runtime is torn down before the pool is dropped, that
//! schedule call panics with "no reactor running". Production code never
//! hits this — the pool lives for the daemon's lifetime, and the daemon
//! exits only after the runtime shuts down gracefully. In tests, **build
//! and drop the pool inside `runtime.block_on(async { ... })`** so the
//! pool's drop runs while the runtime is still active.

use std::path::Path;
use std::sync::Arc;

use deadpool_sqlite::{Config, Hook, HookError, Pool, Runtime};
use solo_core::{Error, Result, VectorIndex};

use crate::key_material::KeyMaterial;

/// Default read pool size per ADR-0003.
pub const DEFAULT_POOL_SIZE: usize = 2;

/// Shared read pool. Cheap to clone (the inner `Pool` is `Arc`-based and
/// the HNSW handle is `Arc<dyn VectorIndex>`). Cloning gives multiple
/// owners — useful when the daemon hands one to the MCP server and keeps
/// one for shutdown.
#[derive(Clone)]
pub struct ReaderPool {
    pool: Pool,
    hnsw: Arc<dyn VectorIndex + Send + Sync>,
}

impl ReaderPool {
    pub fn new(
        db_path: &Path,
        key: Option<KeyMaterial>,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
    ) -> Result<Self> {
        Self::with_size(db_path, key, DEFAULT_POOL_SIZE, hnsw)
    }

    pub fn with_size(
        db_path: &Path,
        key: Option<KeyMaterial>,
        size: usize,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
    ) -> Result<Self> {
        let cfg = Config::new(db_path);
        let mut builder = cfg
            .builder(Runtime::Tokio1)
            .map_err(|e| Error::storage(format!("deadpool config: {e:?}")))?
            .max_size(size);

        if let Some(key) = key {
            let key_hex = key.as_hex();
            builder = builder.post_create(Hook::async_fn(move |conn, _metrics| {
                let key_hex = key_hex.clone();
                Box::pin(async move {
                    // Wrap in Zeroizing<String> so the raw key bytes are
                    // wiped from the heap on drop — parallels the
                    // init.rs + backup.rs PRAGMA key bind sites.
                    let pragma: zeroize::Zeroizing<String> =
                        zeroize::Zeroizing::new(format!("PRAGMA key = \"x'{}'\"", *key_hex));
                    conn.interact(move |c| {
                        c.execute_batch(&pragma)?;
                        c.execute_batch(
                            "PRAGMA foreign_keys = ON;
                             PRAGMA busy_timeout = 5000;",
                        )?;
                        Ok::<_, rusqlite::Error>(())
                    })
                    .await
                    .map_err(|e| HookError::message(format!("interact: {e}")))?
                    .map_err(|e| HookError::message(format!("PRAGMA key: {e}")))?;
                    Ok(())
                })
            }));
        } else {
            builder = builder.post_create(Hook::async_fn(|conn, _metrics| {
                Box::pin(async move {
                    conn.interact(|c| {
                        c.execute_batch(
                            "PRAGMA foreign_keys = ON;
                             PRAGMA busy_timeout = 5000;",
                        )
                    })
                    .await
                    .map_err(|e| HookError::message(format!("interact: {e}")))?
                    .map_err(|e| HookError::message(format!("PRAGMA setup: {e}")))?;
                    Ok(())
                })
            }));
        }

        let pool = builder
            .build()
            .map_err(|e| Error::storage(format!("deadpool build: {e:?}")))?;
        Ok(Self { pool, hnsw })
    }

    pub async fn interact<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| Error::storage(format!("pool get: {e:?}")))?;
        conn.interact(f)
            .await
            .map_err(|e| Error::storage(format!("interact: {e}")))?
            .map_err(|e| Error::storage(format!("rusqlite: {e}")))
    }

    pub fn hnsw(&self) -> &Arc<dyn VectorIndex + Send + Sync> {
        &self.hnsw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        StubVectorIndex, fixture_embedding, fixture_episode, open_test_db_at,
    };
    use crate::writer::WriterActor;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn pool_returns_connections() {
        let runtime = rt();
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.db");
        let _ = open_test_db_at(&path);

        runtime.block_on(async {
            // Pool's life cycle must run within the runtime; create + use +
            // drop inside this async block so pool.drop sees a live reactor.
            let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(4));
            let pool = ReaderPool::new(&path, None, hnsw).unwrap();
            let n: u32 = pool
                .interact(|conn| {
                    conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                        row.get(0)
                    })
                })
                .await
                .unwrap();
            assert_eq!(n, crate::migration::current_per_tenant_schema_version());
        });
    }

    #[test]
    fn reader_sees_writes_committed_through_writer_actor() {
        let runtime = rt();
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.db");
        let writer_conn = open_test_db_at(&path);
        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(4));

        // Writer can be spawned outside the runtime — it owns its OS thread.
        let crate::writer::WriterSpawn { handle, join: _ } =
            WriterActor::spawn(writer_conn, hnsw.clone());

        runtime.block_on(async {
            let pool = ReaderPool::new(&path, None, hnsw).unwrap();

            let episode = fixture_episode("reader-visibility test");
            let mid = handle
                .remember(episode.clone(), fixture_embedding(4))
                .await
                .unwrap();
            assert_eq!(mid, episode.memory_id);

            let mid_str = mid.to_string();
            let content: String = pool
                .interact(move |conn| {
                    conn.query_row(
                        "SELECT content FROM episodes WHERE memory_id = ?",
                        [mid_str],
                        |row| row.get(0),
                    )
                })
                .await
                .unwrap();
            assert_eq!(content, "reader-visibility test");
        });

        drop(handle);
    }

    #[test]
    fn many_concurrent_reads_serve_from_pool() {
        let runtime = rt();
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.db");
        let _ = open_test_db_at(&path);

        runtime.block_on(async {
            let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(4));
            let pool = ReaderPool::with_size(&path, None, 4, hnsw).unwrap();

            let mut tasks = Vec::new();
            for _ in 0..32 {
                let p = pool.pool.clone();
                tasks.push(tokio::spawn(async move {
                    let conn = p.get().await.unwrap();
                    conn.interact(|c| {
                        c.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                            row.get::<_, u32>(0)
                        })
                    })
                    .await
                    .unwrap()
                    .unwrap()
                }));
            }
            let mut counts = Vec::new();
            for t in tasks {
                counts.push(t.await.unwrap());
            }
            assert_eq!(counts.len(), 32);
            let current_version = crate::migration::current_per_tenant_schema_version();
            assert!(counts.iter().all(|c| *c == current_version));
        });
    }
}
