// SPDX-License-Identifier: Apache-2.0

//! `embedders` table registry. Every embedder model that produces
//! vectors in this database has a row keyed by `(name, version)` with
//! its dim + dtype + first-seen timestamp.
//!
//! Lookups are O(1) via the UNIQUE(name, version) index. The
//! corresponding `embedder_id` is what `embeddings.embedder_id`
//! references — one source of truth for "which model did this vector
//! come from?", which `solo reembed` needs to decide what to
//! regenerate after a model upgrade.
//!
//! Today the daemon resolves the active embedder_id once at startup
//! (after migrations) and caches it in the WriterActor; every
//! `remember` insert reuses it. The `solo init` flow doesn't
//! pre-populate `embedders` — the row is lazy-inserted on first
//! daemon start.

use rusqlite::{Connection, OptionalExtension, params};
use solo_core::{Error, Result};

/// What we need to know about an embedder to register it. Maps directly
/// to the `embedders` table columns. Unlike `solo_core::Embedder` (a
/// runtime trait with batched embed methods), this is just metadata —
/// can be built without ever loading the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedderIdentity {
    pub name: String,
    pub version: String,
    pub dim: u32,
    /// Serialized form: "f32" | "f16" | "i8" | "binary". Matches the
    /// schema's CHECK constraint and `solo_core::EmbeddingDtype`'s
    /// lowercase serde rename.
    pub dtype: String,
}

impl EmbedderIdentity {
    /// Build from a runtime `solo_core::Embedder` instance.
    pub fn from_embedder(e: &dyn solo_core::Embedder) -> Self {
        Self {
            name: e.name().to_string(),
            version: e.version().to_string(),
            dim: e.dim() as u32,
            dtype: dtype_to_str(e.dtype()).to_string(),
        }
    }
}

fn dtype_to_str(d: solo_core::EmbeddingDtype) -> &'static str {
    match d {
        solo_core::EmbeddingDtype::F32 => "f32",
        solo_core::EmbeddingDtype::F16 => "f16",
        solo_core::EmbeddingDtype::I8 => "i8",
        solo_core::EmbeddingDtype::Binary => "binary",
    }
}

/// Resolve the embedder_id for `identity`, creating the row if it
/// doesn't already exist. Idempotent — safe to call on every daemon
/// startup. Verifies that any pre-existing row's dim + dtype agree
/// with what the caller is providing; mismatch → Conflict (fixing the
/// embedder's metadata while keeping the same name+version is
/// disallowed; `solo reembed` is the supported escape hatch).
pub fn get_or_insert_embedder_id(conn: &Connection, identity: &EmbedderIdentity) -> Result<i64> {
    // Try to find an existing row first.
    let existing: Option<(i64, u32, String)> = conn
        .query_row(
            "SELECT embedder_id, dim, dtype
               FROM embedders
              WHERE name = ? AND version = ?",
            params![&identity.name, &identity.version],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| Error::storage(format!("lookup embedder_id: {e}")))?;

    if let Some((id, dim, dtype)) = existing {
        if dim != identity.dim || dtype != identity.dtype {
            return Err(Error::conflict(format!(
                "embedder ({}, {}) already registered with dim={dim}/dtype={dtype}; \
                 caller provided dim={}/dtype={}. Bump the embedder version + run \
                 `solo reembed` to regenerate vectors.",
                identity.name, identity.version, identity.dim, identity.dtype
            )));
        }
        return Ok(id);
    }

    // Insert.
    let now_ms = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO embedders (name, version, dim, dtype, first_seen_ms)
         VALUES (?, ?, ?, ?, ?)",
        params![
            &identity.name,
            &identity.version,
            identity.dim,
            &identity.dtype,
            now_ms,
        ],
    )
    .map_err(|e| Error::storage(format!("INSERT embedders row: {e}")))?;
    let id = conn.last_insert_rowid();
    tracing::info!(
        embedder_id = id,
        name = %identity.name,
        version = %identity.version,
        dim = identity.dim,
        dtype = %identity.dtype,
        "registered embedder in `embedders` table"
    );
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        run_migrations(&mut c).unwrap();
        c
    }

    fn id() -> EmbedderIdentity {
        EmbedderIdentity {
            name: "test-embedder".into(),
            version: "v1".into(),
            dim: 1024,
            dtype: "f32".into(),
        }
    }

    #[test]
    fn first_call_inserts_row_returns_id() {
        let conn = fresh_conn();
        let id1 = get_or_insert_embedder_id(&conn, &id()).unwrap();
        assert!(id1 > 0);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM embedders", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn second_call_with_same_identity_returns_same_id() {
        let conn = fresh_conn();
        let id1 = get_or_insert_embedder_id(&conn, &id()).unwrap();
        let id2 = get_or_insert_embedder_id(&conn, &id()).unwrap();
        assert_eq!(id1, id2);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM embedders", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "must NOT insert a duplicate row");
    }

    #[test]
    fn different_version_inserts_new_row() {
        let conn = fresh_conn();
        let id_v1 = get_or_insert_embedder_id(&conn, &id()).unwrap();
        let mut id_v2 = id();
        id_v2.version = "v2".into();
        let id_v2 = get_or_insert_embedder_id(&conn, &id_v2).unwrap();
        assert_ne!(id_v1, id_v2);
    }

    #[test]
    fn dim_mismatch_for_same_identity_rejected() {
        let conn = fresh_conn();
        let _ = get_or_insert_embedder_id(&conn, &id()).unwrap();
        let mut bad = id();
        bad.dim = 2048;
        let err = get_or_insert_embedder_id(&conn, &bad).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got: {err:?}");
    }

    #[test]
    fn dtype_mismatch_for_same_identity_rejected() {
        let conn = fresh_conn();
        let _ = get_or_insert_embedder_id(&conn, &id()).unwrap();
        let mut bad = id();
        bad.dtype = "f16".into();
        let err = get_or_insert_embedder_id(&conn, &bad).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got: {err:?}");
    }
}
