// SPDX-License-Identifier: Apache-2.0

//! Shared coverage snapshot for Solo's optional derived-memory layer.
//!
//! The HTTP capability panel and `memory_context` both consume this value so
//! an empty graph cannot be described differently by the UI and MCP clients.

use serde::Serialize;
use solo_core::Result;

use crate::ReaderPool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DerivedCoverageSnapshot {
    pub active_episodes: usize,
    pub clusters: usize,
    pub clustered_episodes: usize,
    pub abstractions: usize,
    pub pending_clusters: usize,
    pub triples: usize,
    pub entities: usize,
    pub relationships: usize,
    pub contradictions: usize,
}

impl DerivedCoverageSnapshot {
    pub fn cluster_coverage_percent(self) -> u8 {
        percentage(self.clustered_episodes, self.active_episodes)
    }

    pub fn abstraction_coverage_percent(self) -> u8 {
        percentage(self.abstractions, self.clusters)
    }
}

fn percentage(covered: usize, total: usize) -> u8 {
    if total == 0 {
        return 100;
    }
    ((covered.saturating_mul(100) / total).min(100)) as u8
}

pub async fn read_derived_coverage(reader: &ReaderPool) -> Result<DerivedCoverageSnapshot> {
    reader
        .interact(|conn| {
            Ok(DerivedCoverageSnapshot {
                active_episodes: count(conn, "SELECT COUNT(*) FROM episodes WHERE status = 'active'")?,
                clusters: count(conn, "SELECT COUNT(*) FROM clusters")?,
                clustered_episodes: count(
                    conn,
                    "SELECT COUNT(DISTINCT ce.memory_id)
                       FROM cluster_episodes ce
                       JOIN episodes e ON e.memory_id = ce.memory_id
                      WHERE e.status = 'active'",
                )?,
                abstractions: count(conn, "SELECT COUNT(*) FROM semantic_abstractions")?,
                pending_clusters: count(
                    conn,
                    "SELECT COUNT(*)
                       FROM clusters c
                      WHERE NOT EXISTS (
                            SELECT 1 FROM semantic_abstractions sa
                             WHERE sa.cluster_id = c.cluster_id
                      )",
                )?,
                triples: count(conn, "SELECT COUNT(*) FROM triples WHERE status = 'active'")?,
                entities: count(
                    conn,
                    "SELECT COUNT(*) FROM entities WHERE status IN ('candidate', 'active')",
                )?,
                relationships: count(
                    conn,
                    "SELECT COUNT(*) FROM relationship_edges WHERE status IN ('candidate', 'active', 'contradicted')",
                )?,
                contradictions: count(conn, "SELECT COUNT(*) FROM contradictions")?,
            })
        })
        .await
}

fn count(conn: &rusqlite::Connection, sql: &str) -> rusqlite::Result<usize> {
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map(|value| value.max(0) as usize)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::{StubVectorIndex, open_test_db_at};

    #[tokio::test]
    async fn empty_library_has_complete_zero_denominator_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("solo.db");
        drop(open_test_db_at(&path));
        let reader = ReaderPool::new(&path, None, Arc::new(StubVectorIndex::new(4))).unwrap();

        let snapshot = read_derived_coverage(&reader).await.unwrap();
        assert_eq!(snapshot, DerivedCoverageSnapshot::default());
        assert_eq!(snapshot.cluster_coverage_percent(), 100);
        assert_eq!(snapshot.abstraction_coverage_percent(), 100);
    }
}
