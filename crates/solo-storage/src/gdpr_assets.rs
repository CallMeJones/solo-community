// SPDX-License-Identifier: Apache-2.0

//! Principal-owned originals are erased with their SQL records. The writer
//! serializes this with uploads, attachment changes, and backups. Shared
//! originals fail preflight instead of silently deleting another user's data.

use rusqlite::{Transaction, params};
use solo_core::{Error, Result};
use std::path::{Path, PathBuf};

pub(crate) struct AssetErasure {
    paths: Vec<PathBuf>,
    pub documents_deleted: u64,
    pub assets_deleted: u64,
}

pub(crate) fn prepare(
    tx: &Transaction<'_>,
    principal: &str,
    snapshot_dir: Option<&Path>,
) -> Result<AssetErasure> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS solo_forget_docs (id TEXT PRIMARY KEY);
         CREATE TEMP TABLE IF NOT EXISTS solo_forget_assets (id TEXT PRIMARY KEY);
         DELETE FROM solo_forget_docs;
         DELETE FROM solo_forget_assets;",
    )
    .map_err(|e| Error::storage(format!("prepare principal asset scope: {e}")))?;
    tx.execute(
        "INSERT INTO solo_forget_docs
         SELECT d.doc_id FROM documents d
         WHERE (EXISTS (SELECT 1 FROM document_chunks c
                        WHERE c.doc_id = d.doc_id AND c.ingested_by_principal = ?1)
             OR EXISTS (SELECT 1 FROM document_assets da JOIN assets a USING (asset_id)
                        WHERE da.doc_id = d.doc_id AND a.created_by_principal = ?1))
           AND NOT EXISTS (SELECT 1 FROM document_chunks c
                           WHERE c.doc_id = d.doc_id AND c.ingested_by_principal IS NOT ?1)
           AND NOT EXISTS (SELECT 1 FROM memory_attachments ma JOIN episodes e USING (memory_id)
                           WHERE ma.doc_id = d.doc_id AND e.principal_subject IS NOT ?1)",
        params![principal],
    )
    .map_err(|e| Error::storage(format!("scope principal documents: {e}")))?;
    tx.execute(
        "INSERT INTO solo_forget_assets
         SELECT a.asset_id FROM assets a
         WHERE a.created_by_principal = ?1
            OR EXISTS (SELECT 1 FROM document_assets da JOIN solo_forget_docs d ON d.id = da.doc_id
                       WHERE da.asset_id = a.asset_id)
            OR EXISTS (SELECT 1 FROM memory_attachments ma JOIN episodes e USING (memory_id)
                       WHERE ma.asset_id = a.asset_id AND e.principal_subject = ?1)",
        params![principal],
    )
    .map_err(|e| Error::storage(format!("scope principal originals: {e}")))?;
    let shared: bool = tx
        .query_row(
            "SELECT EXISTS (
             SELECT 1 FROM assets a JOIN solo_forget_assets s ON s.id = a.asset_id
             WHERE (a.created_by_principal IS NOT NULL AND a.created_by_principal <> ?1)
                OR EXISTS (SELECT 1 FROM document_assets da WHERE da.asset_id = a.asset_id
                           AND da.doc_id NOT IN (SELECT id FROM solo_forget_docs))
                OR EXISTS (SELECT 1 FROM memory_attachments ma JOIN episodes e USING (memory_id)
                           WHERE ma.asset_id = a.asset_id AND e.principal_subject IS NOT ?1))",
            params![principal],
            |row| row.get(0),
        )
        .map_err(|e| Error::storage(format!("check shared originals before erasure: {e}")))?;
    if shared {
        return Err(Error::invalid_input(
            "principal erasure includes an original file owned or referenced by another principal; resolve shared document/attachment ownership before retrying; nothing was erased",
        ));
    }
    let mut statement = tx.prepare(
        "SELECT a.storage_path, a.sha256 FROM assets a JOIN solo_forget_assets s ON s.id = a.asset_id",
    ).map_err(|e| Error::storage(format!("select original files for erasure: {e}")))?;
    let rows = statement
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| Error::storage(format!("read original file scope: {e}")))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| Error::storage(format!("collect original file scope: {e}")))?;
    let assets_deleted = rows.len() as u64;
    let mut paths = Vec::new();
    for (storage_path, sha256) in rows {
        let root = snapshot_dir.ok_or_else(|| {
            Error::storage("principal erasure requires the library asset directory")
        })?;
        let path = crate::asset_files::safe_asset_storage_path(root, &storage_path)?;
        if path.file_name().and_then(|v| v.to_str()) != Some(sha256.as_str()) {
            return Err(Error::storage("principal erasure asset path/hash mismatch"));
        }
        match path.canonicalize() {
            Ok(resolved) => {
                let root = root
                    .canonicalize()
                    .map_err(|e| Error::storage(format!("resolve library directory: {e}")))?;
                if !resolved.starts_with(&root) || !resolved.is_file() {
                    return Err(Error::storage(
                        "principal erasure refuses an asset outside the library or a non-file",
                    ));
                }
                paths.push(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // retry after an interrupted erase
            Err(e) => {
                return Err(Error::storage(format!(
                    "inspect original before erasure: {e}"
                )));
            }
        }
    }
    let documents_deleted: i64 = tx
        .query_row("SELECT COUNT(*) FROM solo_forget_docs", [], |r| r.get(0))
        .map_err(|e| Error::storage(format!("count erased documents: {e}")))?;
    Ok(AssetErasure {
        paths,
        documents_deleted: documents_deleted as u64,
        assets_deleted,
    })
}

impl AssetErasure {
    pub(crate) fn delete(&self, tx: &Transaction<'_>) -> Result<()> {
        tx.execute_batch(
            "DELETE FROM documents WHERE doc_id IN (SELECT id FROM solo_forget_docs);
             DELETE FROM assets WHERE asset_id IN (SELECT id FROM solo_forget_assets);",
        )
        .map_err(|e| Error::storage(format!("delete principal originals and documents: {e}")))?;
        // Remove bytes before COMMIT: never return a successful erasure while
        // an original remains. Filesystem removal is irreversible; on failure
        // SQL rolls back, and retry tolerates any originals already removed.
        // The caller's writer actor prevents an upload from recreating a blob
        // between this preflight and deletion.
        for path in &self.paths {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::storage(format!(
                        "remove principal original: {e}; erasure incomplete, retry required",
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use sha2::{Digest, Sha256};

    fn seed(conn: &Connection, root: &Path, principal: &str) -> PathBuf {
        let plaintext = format!("original private document for {principal}");
        let sha = hex::encode(Sha256::digest(plaintext.as_bytes()));
        let key = crate::KeyMaterial::from_bytes_for_tests([17; 32]);
        let blob = crate::asset_blob::encrypt_asset_blob(
            &key,
            plaintext.as_bytes(),
            &sha,
            plaintext.len() as u64,
        )
        .unwrap();
        let storage_path = format!("assets/blobs/{}/{sha}", &sha[..2]);
        let path = root.join(&storage_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &blob.ciphertext).unwrap();
        conn.execute(
            "INSERT INTO documents (doc_id,title,ingested_at_ms,chunk_count) VALUES (?1,?1,1,1)",
            [principal],
        )
        .unwrap();
        conn.execute("INSERT INTO document_chunks
            (chunk_id,doc_id,chunk_index,content,token_count,start_offset,end_offset,created_at_ms,ingested_by_principal)
            VALUES (?1,?1,0,?1,1,0,5,1,?1)", [principal]).unwrap();
        conn.execute("INSERT INTO assets
            (asset_id,sha256,filename,size_bytes,storage_path,created_by_principal,created_at_ms,updated_at_ms,encryption_alg,encryption_nonce,encrypted_size_bytes)
            VALUES (?1,?2,?1,?3,?4,?1,1,1,?5,?6,?7)",
            params![principal,sha,plaintext.len() as i64,storage_path,crate::asset_blob::ASSET_BLOB_ENCRYPTION_ALG,blob.nonce,blob.ciphertext.len() as i64]).unwrap();
        conn.execute("INSERT INTO document_assets (link_id,doc_id,asset_id,created_at_ms) VALUES (?1,?1,?1,1)", [principal]).unwrap();
        path
    }

    fn database() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();
        conn
    }

    #[test]
    fn erases_original_bytes_and_metadata_but_preserves_other_principal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = database();
        let alice = seed(&conn, tmp.path(), "alice");
        let bob = seed(&conn, tmp.path(), "bob");
        crate::gdpr::delete_principal_rows_with_assets(&mut conn, "alice", Some(tmp.path()))
            .unwrap();
        assert!(!alice.exists());
        assert!(bob.is_file());
        for table in ["documents", "document_chunks", "document_assets", "assets"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1, "{table}: only Bob's record should survive");
        }
        let status: String = conn
            .query_row(
                "SELECT status FROM assets WHERE asset_id = 'bob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");
        // A second erasure must neither error nor touch Bob's original.
        crate::gdpr::delete_principal_rows_with_assets(&mut conn, "alice", Some(tmp.path()))
            .unwrap();
        assert!(bob.is_file());
    }

    #[test]
    fn shared_original_fails_before_any_erasure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = database();
        let alice = seed(&conn, tmp.path(), "alice");
        let bob = seed(&conn, tmp.path(), "bob");
        conn.execute("INSERT INTO document_assets (link_id,doc_id,asset_id,created_at_ms) VALUES ('shared','bob','alice',1)", []).unwrap();
        let error =
            crate::gdpr::delete_principal_rows_with_assets(&mut conn, "alice", Some(tmp.path()))
                .unwrap_err();
        assert!(error.to_string().contains("another principal"));
        assert!(alice.is_file() && bob.is_file());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM document_chunks", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn interrupted_blob_removal_can_be_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = database();
        let alice = seed(&conn, tmp.path(), "alice");
        std::fs::remove_file(alice).unwrap();
        crate::gdpr::delete_principal_rows_with_assets(&mut conn, "alice", Some(tmp.path()))
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM assets", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn invalid_blob_path_fails_before_erasing_sql_or_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = database();
        let alice = seed(&conn, tmp.path(), "alice");
        conn.execute("UPDATE assets SET storage_path = '../outside'", [])
            .unwrap();
        assert!(
            crate::gdpr::delete_principal_rows_with_assets(&mut conn, "alice", Some(tmp.path()))
                .is_err()
        );
        assert!(alice.is_file());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM document_chunks", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
