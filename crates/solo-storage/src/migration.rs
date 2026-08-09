// SPDX-License-Identifier: Apache-2.0

//! SQL schema migrations. Runs once at startup against the SQLCipher database
//! after `PRAGMA key` has been bound.
//!
//! Migrations are append-only — once a version has shipped to a user, never
//! change its SQL. Bug fixes go in subsequent migrations.
//!
//! The runner advances the version tracker row-by-row inside a single
//! `BEGIN IMMEDIATE` transaction per migration, so a crash mid-migration
//! either applies the whole thing or none of it.
//!
//! ## Two schema chains
//!
//! v0.8.0 introduced a second SQLCipher database at
//! `<data_dir>/tenants_index.db` for the multi-tenant registry, with its
//! own independent migration chain tracked in
//! `schema_migrations_tenants_index`. The per-tenant DBs at
//! `<data_dir>/tenants/<id>.db` still use the original `schema_migrations`
//! table.
//!
//! * Per-tenant chain  →  [`run_migrations`] (still 1-3 today; what every
//!   existing callsite means when it says "run migrations").
//! * tenants_index chain → [`run_tenants_index_migrations`] (just 0004 today).
//!
//! Both share the same `BEGIN IMMEDIATE` / step + row insert mechanics via
//! the private [`apply_one`] helper.

use rusqlite::{Connection, TransactionBehavior, params};
use solo_core::{Error, Result};

/// One migration step. The `up` SQL may contain multiple statements (it's
/// passed to `execute_batch`).
#[derive(Debug)]
struct Migration {
    version: u32,
    description: &'static str,
    up: &'static str,
}

/// Per-tenant DB migrations, in order. Append new entries; never modify
/// existing ones.
///
/// As of v0.14.0+ the highest per-tenant version is **18** — migration 4
/// only lives in tenants_index.db (the multi-tenant registry, P1); later
/// per-tenant migrations add audit, redaction attribution, triples-source
/// cascade support, contradiction lifecycle state, and Memory Inbox review
/// state. Migration 16 invalidates stale pre-quality-gate derived graph rows
/// so they rebuild under the quality gate introduced in migration 15, and
/// migration 17 starts Temporal Associative Memory v2 relationship tables.
/// Numbering is monotonic across both chains for easy git blame.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial schema (v0): episodes + triples + steward outputs + pending_index + FTS",
        up: include_str!("migrations/0001_initial.sql"),
    },
    Migration {
        version: 2,
        description: "triples.cluster_id FK + index for absorb→regen cascade",
        up: include_str!("migrations/0002_triples_cluster_id.sql"),
    },
    Migration {
        version: 3,
        description: "documents + document_chunks + chunk_embeddings + pending_index.kind discriminator",
        up: include_str!("migrations/0003_documents.sql"),
    },
    Migration {
        version: 5,
        description: "per-tenant audit_events table + indices (v0.8.0 P4)",
        up: include_str!("migrations/0005_audit.sql"),
    },
    Migration {
        version: 6,
        description: "principal_subject on episodes + ingested_by_principal on document_chunks (v0.8.0 P5+P6)",
        up: include_str!("migrations/0006_redaction_audit.sql"),
    },
    Migration {
        version: 7,
        description: "triples.source_episode_id FK + backfill from provenance_json (v0.8.1 P1)",
        up: include_str!("migrations/0007_triples_source.sql"),
    },
    Migration {
        version: 10,
        description: "contradiction lifecycle fields for resolve/reopen flows",
        up: include_str!("migrations/0010_contradiction_lifecycle.sql"),
    },
    Migration {
        version: 11,
        description: "Memory Inbox review state",
        up: include_str!("migrations/0011_memory_reviews.sql"),
    },
    Migration {
        version: 12,
        description: "raw file assets plus document and memory attachment links",
        up: include_str!("migrations/0012_assets_attachments.sql"),
    },
    Migration {
        version: 13,
        description: "encrypted retained asset blob metadata",
        up: include_str!("migrations/0013_asset_blob_encryption.sql"),
    },
    Migration {
        version: 14,
        description: "asset extraction status records",
        up: include_str!("migrations/0014_asset_extractions.sql"),
    },
    Migration {
        version: 15,
        description: "triple quality reviews and entity aliases",
        up: include_str!("migrations/0015_triple_quality.sql"),
    },
    Migration {
        version: 16,
        description: "invalidate stale derived graph rows after triple quality gate rollout",
        up: include_str!("migrations/0016_derived_quality_rebuild.sql"),
    },
    Migration {
        version: 17,
        description: "temporal relationship edge and evidence tables",
        up: include_str!("migrations/0017_temporal_relationship_edges.sql"),
    },
    Migration {
        version: 18,
        description: "memory claims, retrieval logs, revisions, and entity review operations",
        up: include_str!("migrations/0018_memory_claims_retrieval_revisions.sql"),
    },
    Migration {
        version: 19,
        description: "Community administrative audit log in the single Memory Library",
        up: include_str!("migrations/0019_community_admin_audit.sql"),
    },
];

/// tenants_index.db migrations, in order.
///
/// Numbering shares space with per-tenant migrations (4 follows 3) to keep
/// the on-disk migration file numbering monotonic across the whole
/// codebase — easier to git blame, easier to scan. The runner uses a
/// SEPARATE tracker table so the chains evolve independently.
///
/// As of v0.9.0 P1 the tenants_index chain is **4 → 8 → 9** — migration
/// 0008 (v0.8.1 P3) added `tenants.quota_bytes`; migration 0009 (v0.9.0
/// P1) adds `tenants.last_accessed` and closes the v0.8.0 doc-vs-code
/// gap (lesson #39 second incident — `last_accessed` was referenced in
/// v0.8.0 release notes but never landed in 0004).
/// Name of the version tracker table inside per-tenant DBs.
const PER_TENANT_TRACKER: &str = "schema_migrations";

/// Run every per-tenant migration that hasn't been applied yet.
///
/// Idempotent — calling on an up-to-date per-tenant DB is a no-op + ~1ms read
/// of `schema_migrations`. Returns the highest version applied (after the run).
pub fn run_migrations(conn: &mut Connection) -> Result<u32> {
    run_migrations_for_connection(conn, MIGRATIONS, PER_TENANT_TRACKER)
}

/// Highest per-tenant schema version known to this build.
pub fn current_per_tenant_schema_version() -> u32 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// Generic migration runner.
///
/// Takes a connection, the migration list to apply, and the name of the
/// tracker table to record progress in. The tracker table is created
/// on-demand with the canonical (version, description, applied_at) schema
/// if it doesn't exist yet — this is how a brand-new DB picks up its
/// first migration without requiring out-of-band table creation.
///
/// Each migration runs inside its own `BEGIN IMMEDIATE` transaction, so a
/// crash mid-migration either applies the whole thing (SQL + tracker row)
/// or none of it.
fn run_migrations_for_connection(
    conn: &mut Connection,
    list: &[Migration],
    tracker_table: &str,
) -> Result<u32> {
    // tracker table is created out-of-band so the first migration doesn't
    // have to bootstrap its own tracking row. CREATE IF NOT EXISTS makes
    // this safe to call before checking existing state.
    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS {tracker_table} (
             version     INTEGER PRIMARY KEY,
             description TEXT    NOT NULL,
             applied_at  INTEGER NOT NULL
         );"
    );
    conn.execute_batch(&create_sql)
        .map_err(|e| Error::storage(format!("create {tracker_table}: {e}")))?;

    let current = current_version_in(conn, tracker_table)?;
    let mut highest = current;

    for m in list {
        if m.version <= current {
            continue;
        }
        apply_one(conn, m, tracker_table)?;
        highest = m.version;
        tracing::info!(
            version = m.version,
            description = m.description,
            tracker = tracker_table,
            "applied migration"
        );
    }

    Ok(highest)
}

/// Highest applied version in the per-tenant `schema_migrations` tracker,
/// or 0 if nothing has been applied yet.
///
/// Kept for back-compat with existing callsites that probe the per-tenant
/// chain (e.g. `startup.rs`). The tenants_index equivalent uses
/// [`current_tenants_index_version`].
pub fn current_version(conn: &Connection) -> Result<u32> {
    current_version_in(conn, PER_TENANT_TRACKER)
}

fn current_version_in(conn: &Connection, tracker_table: &str) -> Result<u32> {
    let sql = format!("SELECT MAX(version) FROM {tracker_table}");
    let v: Option<u32> = conn
        .query_row(&sql, [], |row| row.get::<_, Option<u32>>(0))
        .map_err(|e| Error::storage(format!("query current version from {tracker_table}: {e}")))?;
    Ok(v.unwrap_or(0))
}

fn apply_one(conn: &mut Connection, m: &Migration, tracker_table: &str) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for migration {}: {e}", m.version)))?;
    tx.execute_batch(m.up)
        .map_err(|e| Error::storage(format!("apply migration {}: {e}", m.version)))?;
    let now_ms: i64 = chrono::Utc::now().timestamp_millis();
    let insert_sql =
        format!("INSERT INTO {tracker_table} (version, description, applied_at) VALUES (?, ?, ?)");
    tx.execute(&insert_sql, params![m.version, m.description, now_ms])
        .map_err(|e| Error::storage(format!("insert {tracker_table} row {}: {e}", m.version)))?;
    tx.commit()
        .map_err(|e| Error::storage(format!("commit migration {}: {e}", m.version)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_in_memory() -> Connection {
        Connection::open_in_memory().expect("open in-memory DB")
    }

    #[test]
    fn empty_db_runs_all_migrations() {
        let mut conn = open_in_memory();
        let v = run_migrations(&mut conn).unwrap();
        // Migration 0004 applies to tenants_index.db, NOT to per-tenant
        // DBs (the numbering skip is intentional, see MIGRATIONS comment).
        assert_eq!(v, current_per_tenant_schema_version());
        assert_eq!(
            current_version(&conn).unwrap(),
            current_per_tenant_schema_version()
        );
    }

    #[test]
    fn migration_0011_creates_memory_reviews_table() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();

        let cols: Vec<(String, String)> = conn
            .prepare("PRAGMA table_info('memory_reviews')")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
        for required in [
            "memory_id",
            "state",
            "reviewed_at_ms",
            "note",
            "created_at_ms",
            "updated_at_ms",
        ] {
            assert!(
                names.contains(&required),
                "memory_reviews missing column {required}"
            );
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let memory_id = "00000000-0000-0000-0000-000000000011";
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', 'review me', '{}', 1.0, 0.5, 0.5, 'hot', ?, ?)",
            params![memory_id, now_ms, now_ms, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_reviews (
                memory_id, state, reviewed_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?, 'approved', ?, ?, ?)",
            params![memory_id, now_ms, now_ms, now_ms],
        )
        .unwrap();
        let rejected_memory_id = "00000000-0000-0000-0000-000000000012";
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', 'reject me', '{}', 1.0, 0.5, 0.5, 'hot', ?, ?)",
            params![rejected_memory_id, now_ms, now_ms, now_ms],
        )
        .unwrap();
        let rejected = conn.execute(
            "INSERT INTO memory_reviews (
                memory_id, state, reviewed_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?, 'later', ?, ?, ?)",
            params![rejected_memory_id, now_ms, now_ms, now_ms],
        );
        assert!(
            rejected.is_err(),
            "review state CHECK must reject unknown values"
        );
    }

    #[test]
    fn migration_0012_creates_assets_and_attachment_tables() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        for table in ["assets", "document_assets", "memory_attachments"] {
            let exists: u32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing table after 0012: {table}");
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let memory_id = "00000000-0000-0000-0000-000000001212";
        let doc_id = "00000000-0000-0000-0000-00000000d012";
        let asset_id = "00000000-0000-0000-0000-00000000a012";
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', 'attach me', '{}', 1.0, 0.5, 0.5, 'hot', ?, ?)",
            params![memory_id, now_ms, now_ms, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (doc_id, source, mime_type, ingested_at_ms)
             VALUES (?, '/tmp/doc.md', 'text/markdown', ?)",
            params![doc_id, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO assets (
                asset_id, sha256, mime_type, filename, size_bytes,
                storage_path, status, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'text/plain', 'doc.md', 7, 'assets/blobs/aa/hash', 'active', ?, ?)",
            params![asset_id, "a".repeat(64), now_ms, now_ms],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO memory_attachments (
                attachment_id, memory_id, doc_id, relation_type, created_at_ms
             ) VALUES ('00000000-0000-0000-0000-00000000f001', ?, ?, 'source', ?)",
            params![memory_id, doc_id, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_attachments (
                attachment_id, memory_id, asset_id, relation_type, created_at_ms
             ) VALUES ('00000000-0000-0000-0000-00000000f002', ?, ?, 'source_file', ?)",
            params![memory_id, asset_id, now_ms],
        )
        .unwrap();
        let duplicate_doc = conn.execute(
            "INSERT INTO memory_attachments (
                attachment_id, memory_id, doc_id, relation_type, created_at_ms
             ) VALUES ('00000000-0000-0000-0000-00000000f004', ?, ?, 'source', ?)",
            params![memory_id, doc_id, now_ms],
        );
        assert!(
            duplicate_doc.is_err(),
            "memory_attachments must reject duplicate memory/document/relation links"
        );
        let duplicate_asset = conn.execute(
            "INSERT INTO memory_attachments (
                attachment_id, memory_id, asset_id, relation_type, created_at_ms
             ) VALUES ('00000000-0000-0000-0000-00000000f005', ?, ?, 'source_file', ?)",
            params![memory_id, asset_id, now_ms],
        );
        assert!(
            duplicate_asset.is_err(),
            "memory_attachments must reject duplicate memory/asset/relation links"
        );
        let rejected = conn.execute(
            "INSERT INTO memory_attachments (
                attachment_id, memory_id, doc_id, asset_id, relation_type, created_at_ms
             ) VALUES ('00000000-0000-0000-0000-00000000f003', ?, ?, ?, 'ambiguous', ?)",
            params![memory_id, doc_id, asset_id, now_ms],
        );
        assert!(
            rejected.is_err(),
            "memory_attachments CHECK must reject both doc_id and asset_id"
        );
    }

    #[test]
    fn migration_0015_creates_quality_gate_tables() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();

        for table in ["entity_aliases", "triple_reviews"] {
            let exists: u32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing table after 0015: {table}");
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO entity_aliases
                (alias, canonical_id, display_label, source, confidence, created_at_ms, updated_at_ms)
             VALUES ('solo_relay', 'solo-relay', 'solo-relay', 'test', 1.0, ?, ?)",
            params![now_ms, now_ms],
        )
        .unwrap();
        let canonical: String = conn
            .query_row(
                "SELECT canonical_id FROM entity_aliases WHERE alias = 'solo_relay'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical, "solo-relay");

        conn.execute(
            "INSERT INTO triple_reviews
                (review_id, candidate_fingerprint, triple_id, cluster_id,
                 subject_id, predicate, object_id, object_kind, confidence,
                 reason_code, reason, provenance_json, created_at_ms, updated_at_ms)
             VALUES
                ('review-1', 'fingerprint-1', 'triple-1', NULL,
                 'subject', 'predicate', 'object', 'literal', 0.3,
                 'low_confidence', 'too weak', '{}', ?, ?)",
            params![now_ms, now_ms],
        )
        .unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM triple_reviews WHERE review_id = 'review-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "needs_review");
    }

    #[test]
    fn migration_0016_clears_derived_graph_without_deleting_raw_memory() {
        let mut conn = open_in_memory();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 version     INTEGER PRIMARY KEY,
                 description TEXT    NOT NULL,
                 applied_at  INTEGER NOT NULL
             );",
        )
        .unwrap();
        for m in MIGRATIONS.iter().filter(|m| m.version <= 15) {
            apply_one(&mut conn, m, PER_TENANT_TRACKER).unwrap();
        }

        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        let memory_id = "00000000-0000-0000-0000-000000001616";
        let cluster_id = "00000000-0000-0000-0000-00000000c016";
        let abstraction_id = "00000000-0000-0000-0000-00000000a016";
        let triple_id = "00000000-0000-0000-0000-00000000f016";

        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', 'keep this raw memory', '{}', 1.0, 0.5, 0.5, 'hot', ?, ?)",
            params![memory_id, now_ms, now_ms, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clusters (cluster_id, coherence, created_at_ms)
             VALUES (?, 0.9, ?)",
            params![cluster_id, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cluster_episodes (cluster_id, memory_id)
             VALUES (?, ?)",
            params![cluster_id, memory_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO semantic_abstractions (
                abstraction_id, cluster_id, content, provenance_json, confidence, created_at_ms
             ) VALUES (?, ?, 'weak old abstraction', '{}', 0.7, ?)",
            params![abstraction_id, cluster_id, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO triples (
                triple_id, subject_id, predicate, object_id, object_kind,
                valid_from_ms, valid_to_ms, confidence, provenance_json,
                status, created_at_ms, updated_at_ms, cluster_id, source_episode_id
             ) VALUES (?, 'commit_bad', 'mentions', 'C:/path/blob', 'literal',
                ?, NULL, 0.5, '{}', 'active', ?, ?, ?, 1)",
            params![triple_id, now_ms, now_ms, now_ms, cluster_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contradictions (
                a_memory_id, b_memory_id, kind, explanation, detected_at_ms
             ) VALUES ('a', 'b', 'other', 'derived stale contradiction', ?)",
            params![now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entity_aliases
                (alias, canonical_id, display_label, source, confidence, created_at_ms, updated_at_ms)
             VALUES ('solo_relay', 'solo-relay', 'solo-relay', 'test', 1.0, ?, ?)",
            params![now_ms, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO triple_reviews
                (review_id, candidate_fingerprint, triple_id, cluster_id, source_episode_id,
                 subject_id, predicate, object_id, object_kind, confidence,
                 reason_code, reason, provenance_json, created_at_ms, updated_at_ms)
             VALUES
                ('review-16', 'fingerprint-16', ?, ?, 1,
                 'subject', 'predicate', 'object', 'literal', 0.3,
                 'low_confidence', 'too weak', '{}', ?, ?)",
            params![triple_id, cluster_id, now_ms, now_ms],
        )
        .unwrap();

        for m in MIGRATIONS.iter().filter(|m| m.version == 16) {
            apply_one(&mut conn, m, PER_TENANT_TRACKER).unwrap();
        }

        let raw_count: u32 = conn
            .query_row("SELECT COUNT(*) FROM episodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(raw_count, 1, "raw episodes must survive migration 0016");

        for table in [
            "contradictions",
            "triple_reviews",
            "entity_aliases",
            "triples",
            "semantic_abstractions",
            "cluster_episodes",
            "clusters",
        ] {
            let count: u32 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "migration 0016 must clear {table}");
        }
        assert_eq!(current_version(&conn).unwrap(), 16);
    }

    #[test]
    fn migration_0017_backfills_relationship_edges_and_cleans_on_triple_delete() {
        let mut conn = open_in_memory();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 version     INTEGER PRIMARY KEY,
                 description TEXT    NOT NULL,
                 applied_at  INTEGER NOT NULL
             );",
        )
        .unwrap();
        for m in MIGRATIONS.iter().filter(|m| m.version <= 16) {
            apply_one(&mut conn, m, PER_TENANT_TRACKER).unwrap();
        }

        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        let memory_id = "00000000-0000-0000-0000-000000001717";
        let cluster_id = "00000000-0000-0000-0000-00000000c017";
        let triple_id = "00000000-0000-0000-0000-00000000f017";
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', 'Solo Relay supports remote MCP', '{}', 1.0, 0.5, 0.5, 'hot', ?, ?)",
            params![memory_id, now_ms, now_ms, now_ms],
        )
        .unwrap();
        let source_episode_id: i64 = conn
            .query_row(
                "SELECT rowid FROM episodes WHERE memory_id = ?",
                params![memory_id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO clusters (cluster_id, coherence, created_at_ms)
             VALUES (?, 0.9, ?)",
            params![cluster_id, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entity_aliases
                (alias, canonical_id, display_label, source, confidence, created_at_ms, updated_at_ms)
             VALUES ('Solo Relay', 'solo-relay', 'Solo Relay', 'test', 0.9, ?, ?)",
            params![now_ms, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO triples (
                triple_id, subject_id, predicate, object_id, object_kind,
                valid_from_ms, valid_to_ms, confidence, provenance_json,
                status, created_at_ms, updated_at_ms, cluster_id, source_episode_id
             ) VALUES (?, 'solo-relay', 'supports', 'remote-mcp', 'entity',
                ?, NULL, 0.9, '{}', 'active', ?, ?, ?, ?)",
            params![
                triple_id,
                now_ms,
                now_ms,
                now_ms,
                cluster_id,
                source_episode_id
            ],
        )
        .unwrap();

        for m in MIGRATIONS.iter().filter(|m| m.version == 17) {
            apply_one(&mut conn, m, PER_TENANT_TRACKER).unwrap();
        }

        let edge: (String, String, i64) = conn
            .query_row(
                "SELECT subject_entity_id, object_entity_id, evidence_count
                   FROM relationship_edges
                  WHERE edge_id = ?",
                params![triple_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(edge, ("solo-relay".into(), "remote-mcp".into(), 1));
        let evidence_memory: String = conn
            .query_row(
                "SELECT memory_id FROM relationship_evidence WHERE triple_id = ?",
                params![triple_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(evidence_memory, memory_id);

        conn.execute(
            "DELETE FROM triples WHERE triple_id = ?",
            params![triple_id],
        )
        .unwrap();
        let edge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM relationship_edges", [], |row| {
                row.get(0)
            })
            .unwrap();
        let evidence_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM relationship_evidence", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(edge_count, 0);
        assert_eq!(evidence_count, 0);
    }

    #[test]
    fn migration_0018_backfills_rewritten_reviews_as_superseded_claims() {
        let mut conn = open_in_memory();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 version     INTEGER PRIMARY KEY,
                 description TEXT    NOT NULL,
                 applied_at  INTEGER NOT NULL
             );",
        )
        .unwrap();
        for m in MIGRATIONS.iter().filter(|m| m.version <= 17) {
            apply_one(&mut conn, m, PER_TENANT_TRACKER).unwrap();
        }

        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO triple_reviews
                (review_id, candidate_fingerprint, subject_id, predicate,
                 object_id, object_kind, confidence, reason_code, reason,
                 provenance_json, status, created_at_ms, updated_at_ms)
             VALUES
                ('review-0018-rewritten', 'fp-0018-rewritten', 'solo', 'uses',
                 'relay', 'entity', 0.82, 'operator_rewrite',
                 'rewritten into cleaner claims', '{}', 'rewritten', ?1, ?1)",
            params![now_ms],
        )
        .unwrap();

        for m in MIGRATIONS.iter().filter(|m| m.version == 18) {
            apply_one(&mut conn, m, PER_TENANT_TRACKER).unwrap();
        }

        let status: String = conn
            .query_row(
                "SELECT status FROM memory_claims WHERE review_id = 'review-0018-rewritten'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "superseded");
    }

    #[test]
    fn migration_0014_creates_asset_extractions_table() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let exists: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='asset_extractions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "missing asset_extractions table");

        let now_ms = chrono::Utc::now().timestamp_millis();
        let asset_id = "00000000-0000-0000-0000-000000001414";
        conn.execute(
            "INSERT INTO assets (
                asset_id, sha256, mime_type, filename, size_bytes,
                storage_path, status, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'application/octet-stream', 'blob.bin', 3,
                      'assets/blobs/aa/hash', 'active', ?, ?)",
            params![asset_id, "b".repeat(64), now_ms, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_extractions (
                extraction_id, asset_id, extractor_name, extractor_version,
                status, text_chars, error, created_at_ms
             ) VALUES ('00000000-0000-0000-0000-00000000e014', ?, 'fallback_binary',
                      'v1', 'stored_unparsed', 0, NULL, ?)",
            params![asset_id, now_ms],
        )
        .unwrap();
        let rejected = conn.execute(
            "INSERT INTO asset_extractions (
                extraction_id, asset_id, extractor_name, extractor_version,
                status, text_chars, created_at_ms
             ) VALUES ('00000000-0000-0000-0000-00000000e015', ?, 'fallback_binary',
                      'v2', 'mystery', 0, ?)",
            params![asset_id, now_ms],
        );
        assert!(
            rejected.is_err(),
            "asset_extractions.status CHECK must reject unknown values"
        );
    }

    #[test]
    fn migration_0002_adds_triples_cluster_id_column() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        // PRAGMA table_info gives (cid, name, type, notnull, dflt_value, pk)
        let cols: Vec<(String, String)> = conn
            .prepare("PRAGMA table_info('triples')")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"cluster_id"),
            "triples missing cluster_id after 0002; got {names:?}"
        );
        // Index exists.
        let idx_exists: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='idx_triples_cluster'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx_exists, 1, "idx_triples_cluster missing after 0002");
    }

    #[test]
    fn migration_0002_cluster_delete_cascades_to_triples() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        // Seed minimal cluster + triple.
        let cid = "00000000-0000-0000-0000-000000000077";
        let tid = "00000000-0000-0000-0000-000000000099";
        conn.execute(
            "INSERT INTO clusters (cluster_id, coherence, created_at_ms) VALUES (?, ?, ?)",
            params![cid, 0.9, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO triples (
                triple_id, subject_id, predicate, object_id, object_kind,
                valid_from_ms, valid_to_ms, confidence, provenance_json,
                created_at_ms, updated_at_ms, cluster_id
             ) VALUES (?, 'subj', 'pred', 'obj', 'literal', ?, NULL, 0.9, '{}', ?, ?, ?)",
            params![tid, now_ms, now_ms, now_ms, cid],
        )
        .unwrap();
        // Pre-condition: triple exists.
        let n_before: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM triples WHERE triple_id = ?",
                params![tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_before, 1);
        // Drop the cluster — CASCADE should remove the triple.
        conn.execute("DELETE FROM clusters WHERE cluster_id = ?", params![cid])
            .unwrap();
        let n_after: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM triples WHERE triple_id = ?",
                params![tid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_after, 0, "CASCADE on clusters should drop the triple");
    }

    #[test]
    fn second_run_is_a_noop() {
        let mut conn = open_in_memory();
        let v1 = run_migrations(&mut conn).unwrap();
        let v2 = run_migrations(&mut conn).unwrap();
        assert_eq!(v1, v2);
        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "schema_migrations row must not be inserted twice");
    }

    #[test]
    fn all_canonical_tables_present() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let want = [
            "schema_migrations",
            "embedders",
            "episodes",
            "embeddings",
            "pending_index",
            "triples",
            "entities",
            "relationship_edges",
            "relationship_evidence",
            "clusters",
            "cluster_episodes",
            "semantic_abstractions",
            "contradictions",
            "memory_reviews",
            "entity_aliases",
            "triple_reviews",
        ];
        for table in want {
            let exists: u32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing canonical table: {table}");
        }
    }

    #[test]
    fn fts_virtual_table_present() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let exists: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='episodes_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "episodes_fts virtual table missing");
    }

    #[test]
    fn pending_index_schema_matches_adr() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        // The pending_index schema is canonical per ADR-0003 §pending_index.
        // memory_id PK, embedding BLOB, embedding_dim INTEGER, enqueued_at INTEGER.
        let cols: Vec<(String, String)> = conn
            .prepare("PRAGMA table_info('pending_index')")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
        for required in ["memory_id", "embedding", "embedding_dim", "enqueued_at"] {
            assert!(
                names.contains(&required),
                "pending_index missing column {required}"
            );
        }
    }

    #[test]
    fn fts_trigger_keeps_episodes_content_indexed() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        // Insert a minimal episode row.
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', 'the rain in spain falls mainly on the plain',
                       '{}', 0.9, 0.5, 0.5, 'hot', ?, ?)",
            params![
                "00000000-0000-0000-0000-000000000001",
                now_ms,
                now_ms,
                now_ms
            ],
        )
        .unwrap();
        // FTS table should now have a row matching 'spain'.
        let hit: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes_fts WHERE episodes_fts MATCH 'spain'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1);
    }

    #[test]
    fn migration_0010_adds_contradiction_lifecycle_columns() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info('contradictions')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for required in [
            "status",
            "resolved_at_ms",
            "resolution_note",
            "winning_triple_id",
        ] {
            assert!(
                cols.iter().any(|name| name == required),
                "contradictions missing lifecycle column {required}"
            );
        }
    }

    #[test]
    fn cascade_delete_removes_pending_index_row() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        let mid = "00000000-0000-0000-0000-000000000042";
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', 'hello', '{}', 1.0, 0.5, 0.5, 'hot', ?, ?)",
            params![mid, now_ms, now_ms, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pending_index (memory_id, embedding, embedding_dim, enqueued_at)
             VALUES (?, x'00', 1, ?)",
            params![mid, now_ms],
        )
        .unwrap();
        conn.execute("DELETE FROM episodes WHERE memory_id = ?", params![mid])
            .unwrap();
        let remaining: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_index WHERE memory_id = ?",
                params![mid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "CASCADE should have removed the pending row");
    }

    // -------- 0003 documents + document_chunks + pending_index.kind --------

    /// Helper: insert a minimal document row (only NOT NULL cols + reasonable defaults).
    fn insert_test_document(conn: &Connection, doc_id: &str) {
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO documents (doc_id, source, mime_type, ingested_at_ms)
             VALUES (?, ?, ?, ?)",
            params![doc_id, "/tmp/test.md", "text/markdown", now_ms],
        )
        .unwrap();
    }

    /// Helper: insert a minimal chunk row tied to `doc_id` at `idx`.
    /// Returns (chunk_id, rowid).
    fn insert_test_chunk(
        conn: &Connection,
        doc_id: &str,
        idx: i64,
        content: &str,
    ) -> (String, i64) {
        let chunk_id = format!("00000000-0000-0000-0000-{:012x}", idx + 0x100);
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO document_chunks (
                chunk_id, doc_id, chunk_index, content, token_count,
                start_offset, end_offset, created_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                chunk_id,
                doc_id,
                idx,
                content,
                content.split_whitespace().count() as i64,
                0i64,
                content.len() as i64,
                now_ms,
            ],
        )
        .unwrap();
        let rowid = conn.last_insert_rowid();
        (chunk_id, rowid)
    }

    #[test]
    fn migration_0003_creates_documents_and_chunks_tables() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        for table in [
            "documents",
            "document_chunks",
            "chunk_embeddings",
            "document_chunks_fts",
        ] {
            let exists: u32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','virtual','vtable') AND name=?",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing table after 0003: {table}");
        }
    }

    #[test]
    fn migration_0003_pending_index_has_kind_column() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let cols: Vec<(String, String)> = conn
            .prepare("PRAGMA table_info('pending_index')")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
        for required in [
            "kind",
            "memory_id",
            "chunk_id",
            "embedding",
            "embedding_dim",
            "enqueued_at",
        ] {
            assert!(
                names.contains(&required),
                "pending_index missing column {required} after 0003"
            );
        }
    }

    #[test]
    fn migration_0003_backfills_existing_pending_rows_as_episode_kind() {
        // Simulate a DB that was at v0.6.x (migrations 1+2 applied, with a
        // pending_index row pre-existing) and then runs 0003. Pre-0003 rows
        // should land in the rebuilt table with kind='episode'.
        let mut conn = open_in_memory();
        // Apply just the first two migrations manually by slicing MIGRATIONS.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 version     INTEGER PRIMARY KEY,
                 description TEXT    NOT NULL,
                 applied_at  INTEGER NOT NULL
             );",
        )
        .unwrap();
        for m in &MIGRATIONS[..2] {
            apply_one(&mut conn, m, PER_TENANT_TRACKER).unwrap();
        }
        // Seed an episode + pending_index row using the 0001 schema.
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        let mid = "00000000-0000-0000-0000-0000000000aa";
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', 'pre-0003 row', '{}', 1.0, 0.5, 0.5, 'hot', ?, ?)",
            params![mid, now_ms, now_ms, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pending_index (memory_id, embedding, embedding_dim, enqueued_at)
             VALUES (?, x'00', 1, ?)",
            params![mid, now_ms],
        )
        .unwrap();
        // Now run all migrations — 0003 should rebuild the table and preserve the row.
        run_migrations(&mut conn).unwrap();
        let (kind, mem_id, chunk_id): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT kind, memory_id, chunk_id FROM pending_index",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "episode");
        assert_eq!(mem_id.as_deref(), Some(mid));
        assert!(
            chunk_id.is_none(),
            "back-filled row must have NULL chunk_id"
        );
    }

    #[test]
    fn migration_0003_documents_cascade_drops_chunks() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        let doc = "00000000-0000-0000-0000-0000000000d1";
        insert_test_document(&conn, doc);
        for i in 0..3 {
            insert_test_chunk(&conn, doc, i, &format!("chunk {i}"));
        }
        let n_before: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_chunks WHERE doc_id = ?",
                params![doc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_before, 3);
        conn.execute("DELETE FROM documents WHERE doc_id = ?", params![doc])
            .unwrap();
        let n_after: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_chunks WHERE doc_id = ?",
                params![doc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_after, 0, "CASCADE on documents must drop chunks");
    }

    #[test]
    fn migration_0003_pending_index_kind_check_constraint_refuses_bogus_kind() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        let res = conn.execute(
            "INSERT INTO pending_index (kind, memory_id, embedding, embedding_dim, enqueued_at)
             VALUES ('bogus', '00000000-0000-0000-0000-000000000001', x'00', 1, ?)",
            params![now_ms],
        );
        assert!(res.is_err(), "kind='bogus' must violate CHECK constraint");
    }

    #[test]
    fn migration_0003_pending_index_xor_refuses_both_episode_and_chunk_set() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        // Both memory_id AND chunk_id set → violates XOR check (also kind has to disagree)
        let res = conn.execute(
            "INSERT INTO pending_index (kind, memory_id, chunk_id, embedding, embedding_dim, enqueued_at)
             VALUES ('episode', '00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000002', x'00', 1, ?)",
            params![now_ms],
        );
        assert!(
            res.is_err(),
            "memory_id AND chunk_id both NOT NULL must violate XOR"
        );
    }

    #[test]
    fn migration_0003_chunk_fts_keeps_in_sync_on_insert() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let doc = "00000000-0000-0000-0000-0000000000d2";
        insert_test_document(&conn, doc);
        insert_test_chunk(&conn, doc, 0, "the rain in spain falls mainly on the plain");
        let hit: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_chunks_fts WHERE document_chunks_fts MATCH 'spain'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            hit, 1,
            "FTS trigger must index the inserted chunk's content"
        );
    }

    #[test]
    fn migration_0003_chunk_fts_keeps_in_sync_on_delete() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let doc = "00000000-0000-0000-0000-0000000000d3";
        insert_test_document(&conn, doc);
        let (chunk_id, _) =
            insert_test_chunk(&conn, doc, 0, "blackbirds singing in the dead of night");
        conn.execute(
            "DELETE FROM document_chunks WHERE chunk_id = ?",
            params![chunk_id],
        )
        .unwrap();
        let hit: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_chunks_fts WHERE document_chunks_fts MATCH 'blackbirds'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            hit, 0,
            "FTS trigger must remove the chunk from the index after DELETE"
        );
    }

    #[test]
    fn migration_0003_unique_doc_id_chunk_index_enforced() {
        let mut conn = open_in_memory();
        run_migrations(&mut conn).unwrap();
        let doc = "00000000-0000-0000-0000-0000000000d4";
        insert_test_document(&conn, doc);
        insert_test_chunk(&conn, doc, 0, "first");
        // Second chunk with the same chunk_index for the same doc.
        let now_ms: i64 = chrono::Utc::now().timestamp_millis();
        let res = conn.execute(
            "INSERT INTO document_chunks (
                chunk_id, doc_id, chunk_index, content, token_count,
                start_offset, end_offset, created_at_ms
             ) VALUES (?, ?, 0, 'duplicate', 1, 0, 9, ?)",
            params!["00000000-0000-0000-0000-00000000aaaa", doc, now_ms],
        );
        assert!(res.is_err(), "(doc_id, chunk_index) must be UNIQUE");
    }
}
