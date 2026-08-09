// SPDX-License-Identifier: Apache-2.0

//! `solo documents` — parent subcommand for document management.
//!
//! Three children:
//!
//!   * `solo documents list` — paginated listing (`solo_query::list_documents`).
//!   * `solo documents inspect` — metadata + chunk previews
//!     (`solo_query::inspect_document`).
//!   * `solo documents forget` — soft-delete + HNSW tombstone
//!     (`WriteHandle::forget_document`).
//!
//! All three follow the same one-shot pattern as `solo remember` /
//! `solo recall` / `solo forget`: acquire `solo.lock`, run the startup
//! chain, dispatch, shutdown. Only `forget` is a write; `list` and
//! `inspect` go through the read pool.
//!
//! ## doc_id resolution
//!
//! All three children accept either a full UUID (`xxxx-xxxx-xxxx-...`) or
//! a unique short prefix. Short-prefix lookup walks the documents table
//! once; ambiguous prefixes return a clean error rather than silently
//! picking one. See [`resolve_doc_id`].

pub mod forget;
pub mod inspect;
pub mod list;

use anyhow::{Result, bail};
use clap::Subcommand;
use solo_core::DocumentId;
use std::str::FromStr;

#[derive(Debug, Subcommand)]
pub enum DocumentsCommand {
    /// List ingested documents, newest first.
    List(list::ListArgs),
    /// Show one document's metadata + chunk previews.
    Inspect(inspect::InspectArgs),
    /// Soft-delete a document. Chunks survive for forensic value but
    /// the document is hidden from list / search until restored.
    Forget(forget::ForgetArgs),
}

pub async fn run(cmd: DocumentsCommand) -> Result<()> {
    match cmd {
        DocumentsCommand::List(args) => list::run(args).await,
        DocumentsCommand::Inspect(args) => inspect::run(args).await,
        DocumentsCommand::Forget(args) => forget::run(args).await,
    }
}

/// Resolve a user-supplied doc-id string to a typed [`DocumentId`].
///
/// Three paths:
///
///   1. The input is a full 36-char UUID → parse directly.
///   2. The input is a shorter hex prefix → query `documents` for any
///      row whose `doc_id` starts with that prefix.
///      * Exactly one match → return it.
///      * Zero matches → "not found" error.
///      * >1 matches → "ambiguous prefix" error listing the first few
///        candidates so the user can disambiguate.
///   3. Anything else → propagate the UUID parse error.
///
/// Prefix matching uses SQL `LIKE 'prefix%'` — the storage layer's
/// `doc_id` column is a text-form UUID, so this is a simple
/// substring-prefix match. We don't try to normalise hyphens; users
/// typically copy the first 8 chars from a previous CLI output and
/// those never include a hyphen.
pub(crate) async fn resolve_doc_id(
    pool: &solo_storage::ReaderPool,
    input: &str,
) -> Result<DocumentId> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("doc_id must not be empty");
    }

    // Path 1: full UUID — parse directly. Cheaper than a SQL hit, and
    // unambiguous by construction.
    if let Ok(id) = DocumentId::from_str(trimmed) {
        return Ok(id);
    }

    // Path 2: prefix lookup. Refuse single-char prefixes — too many
    // false positives in any realistic dataset.
    if trimmed.len() < 4 {
        bail!(
            "doc_id `{trimmed}` is too short for prefix resolution \
             (need ≥4 hex chars or a full UUID)"
        );
    }
    // Defensive: ensure the prefix only contains hex digits + hyphens.
    // Any other character would be a typo, not a real prefix.
    if !trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        bail!("doc_id `{trimmed}` contains non-hex characters; expected a UUID or hex prefix");
    }
    let needle = trimmed.to_ascii_lowercase();
    let candidates: Vec<String> = pool
        .interact(move |conn| {
            let mut stmt =
                conn.prepare("SELECT doc_id FROM documents WHERE doc_id LIKE ?1 LIMIT 16")?;
            let rows: Vec<String> = stmt
                .query_map(rusqlite::params![format!("{needle}%")], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
    match candidates.as_slice() {
        [] => bail!("no document matches doc_id prefix `{trimmed}`"),
        [one] => DocumentId::from_str(one)
            .map_err(|e| anyhow::anyhow!("matched doc_id `{one}` is not a valid UUID: {e}")),
        many => {
            let preview: Vec<String> = many.iter().take(5).cloned().collect();
            bail!(
                "doc_id prefix `{trimmed}` is ambiguous; matched {} document(s): \
                 first {} → {:?}",
                many.len(),
                preview.len(),
                preview,
            );
        }
    }
}
