// SPDX-License-Identifier: Apache-2.0

//! Memory Inbox read model.
//!
//! The Inbox is a review queue over recent active episodes. Review state is
//! stored in `memory_reviews`; a missing row means "needs review".

use rusqlite::params;
use serde::Serialize;
use solo_core::Result;
use solo_storage::{AuditOperation, AuditWriter, ReaderPool};

pub const INBOX_LABEL_CHARS: usize = 80;
pub const INBOX_PREVIEW_CHARS: usize = 200;
pub const INBOX_MAX_LIMIT: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryInboxItem {
    pub memory_id: String,
    pub label: String,
    pub preview: String,
    pub ts_ms: i64,
    pub source_type: String,
    pub salience: f64,
    pub status: String,
    pub review_state: Option<String>,
    pub reviewed_at_ms: Option<i64>,
    pub review_note: Option<String>,
}

pub async fn memory_inbox(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    limit: usize,
) -> Result<Vec<MemoryInboxItem>> {
    let result = memory_inbox_inner(pool, limit).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryInbox, None),
        Err(e) => audit.emit_error(audit_principal, AuditOperation::MemoryInbox, None, e),
    }
    result
}

#[doc(hidden)]
pub async fn memory_inbox_inner(pool: &ReaderPool, limit: usize) -> Result<Vec<MemoryInboxItem>> {
    let limit = limit.clamp(1, INBOX_MAX_LIMIT) as i64;
    pool.interact(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT e.memory_id,
                    e.ts_ms,
                    e.content,
                    e.source_type,
                    e.salience,
                    e.status,
                    mr.state,
                    mr.reviewed_at_ms,
                    mr.note
               FROM episodes e
               LEFT JOIN memory_reviews mr ON mr.memory_id = e.memory_id
              WHERE e.status = 'active'
              ORDER BY e.ts_ms DESC, e.memory_id ASC
              LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                let content: String = r.get(2)?;
                Ok(MemoryInboxItem {
                    memory_id: r.get(0)?,
                    label: episode_label(&content),
                    preview: truncate_preview(&content, INBOX_PREVIEW_CHARS),
                    ts_ms: r.get(1)?,
                    source_type: r.get(3)?,
                    salience: r.get(4)?,
                    status: r.get(5)?,
                    review_state: r.get(6)?,
                    reviewed_at_ms: r.get(7)?,
                    review_note: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await
}

fn episode_label(content: &str) -> String {
    let first_line = content.lines().next().unwrap_or(content);
    truncate_preview(first_line, INBOX_LABEL_CHARS)
}

fn truncate_preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_storage::ReaderPool;
    use solo_storage::test_support::{StubVectorIndex, open_test_db_at};
    use std::sync::Arc;

    fn pool_with_seed(seed: impl FnOnce(&rusqlite::Connection)) -> (ReaderPool, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let conn = open_test_db_at(&db_path);
        seed(&conn);
        drop(conn);
        let hnsw: Arc<dyn solo_core::VectorIndex + Send + Sync> =
            Arc::new(StubVectorIndex::new(16));
        let pool = ReaderPool::new(&db_path, None, hnsw).expect("pool");
        (pool, tmp)
    }

    fn seed_episode(
        conn: &rusqlite::Connection,
        memory_id: &str,
        ts_ms: i64,
        content: &str,
        source_type: &str,
    ) {
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, '{}', 1.0, 0.5, 0.8, 'hot', ?2, ?2)",
            rusqlite::params![memory_id, ts_ms, source_type, content],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn inbox_returns_recent_active_memories_with_review_state() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_episode(
                conn,
                "mem-a",
                1000,
                "first line\nsecond line",
                "user_message",
            );
            seed_episode(conn, "mem-b", 2000, "newer", "assistant_message");
            seed_episode(conn, "mem-c", 3000, "forgotten", "user_message");
            conn.execute(
                "UPDATE episodes SET status = 'forgotten' WHERE memory_id = 'mem-c'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO memory_reviews (
                    memory_id, state, reviewed_at_ms, note, created_at_ms, updated_at_ms
                 ) VALUES ('mem-b', 'approved', 2500, 'ok', 2500, 2500)",
                [],
            )
            .unwrap();
        });

        let rows = memory_inbox_inner(&pool, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].memory_id, "mem-b");
        assert_eq!(rows[0].review_state.as_deref(), Some("approved"));
        assert_eq!(rows[0].review_note.as_deref(), Some("ok"));
        assert_eq!(rows[1].memory_id, "mem-a");
        assert_eq!(rows[1].label, "first line");
        assert!(rows[1].review_state.is_none());
    }
}
