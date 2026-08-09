// SPDX-License-Identifier: Apache-2.0

//! `solo documents inspect <doc-id>` — show one document's metadata
//! plus a per-chunk preview.
//!
//! Wraps [`solo_query::inspect_document`]. By default each chunk shows
//! the library-provided 200-char preview; `--full-content` swaps in
//! the full chunk content via a follow-up SQL fetch (preview field
//! stays a preview in the lib, so we issue a second query for the raw
//! `content` column when the user asks for it).

use anyhow::{Context, Result, bail};
use chrono::TimeZone;
use clap::Args;
use solo_query::inspect_document;
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Document id (full UUID or unique hex prefix; at least 4 hex
    /// chars for prefix lookup).
    pub doc_id: String,

    /// Show full chunk content instead of the 200-char preview. Useful
    /// for piping into `less` or grep. Default: preview only.
    #[arg(long)]
    pub full_content: bool,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: InspectArgs) -> Result<()> {
    let ctx = prepare_oneshot(args.data_dir).await?;
    let doc_id = match super::resolve_doc_id(ctx.read_pool(), &args.doc_id).await {
        Ok(id) => id,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e);
        }
    };

    // CLI is implicitly trusted; pass `None` for the audit principal.
    let result_opt =
        match inspect_document(ctx.read_pool(), ctx.library_handle.audit(), None, &doc_id).await {
            Ok(r) => r,
            Err(e) => {
                ctx.shutdown().await.ok();
                return Err(e.into());
            }
        };

    let Some(result) = result_opt else {
        ctx.shutdown().await.ok();
        // Exit 1 with anyhow so the shell wrapper can branch on it.
        bail!("document not found: {doc_id}");
    };

    let d = &result.document;
    println!("doc_id        : {}", d.doc_id);
    println!(
        "title         : {}",
        d.title.as_deref().unwrap_or("(no title)")
    );
    println!("source        : {}", d.source.as_deref().unwrap_or("-"));
    println!("mime_type     : {}", d.mime_type.as_deref().unwrap_or("-"));
    println!("status        : {}", fmt_status(&d.status));
    println!("ingested      : {}", fmt_ms(d.ingested_at_ms));
    if let Some(m) = d.modified_at_ms {
        println!("modified      : {}", fmt_ms(m));
    }
    println!("chunk_count   : {}", d.chunk_count);
    if let Some(h) = &d.content_hash {
        println!("content_hash  : {h}");
    }
    if let Some(b) = d.byte_size {
        println!("byte_size     : {b}");
    }
    println!();

    if result.chunks.is_empty() {
        println!("(no chunks)");
    } else if args.full_content {
        // Follow-up SQL fetch for full content. Cheap — one query per
        // doc, even at 50 chunks. We could add a `solo_query` API for
        // this, but the inspect-with-full path is rare enough that the
        // local SQL is justified.
        let full_chunks = match fetch_full_chunks(ctx.read_pool(), &doc_id).await {
            Ok(c) => c,
            Err(e) => {
                ctx.shutdown().await.ok();
                return Err(e);
            }
        };
        for (i, content) in full_chunks.iter().enumerate() {
            println!("--- chunk {i} ---");
            println!("{content}");
            println!();
        }
    } else {
        for chunk in &result.chunks {
            println!(
                "chunk {:>3} ({:>4} tokens) {}: {}",
                chunk.chunk_index,
                chunk.token_count,
                short(&chunk.chunk_id, 8),
                chunk.content_preview.replace('\n', " "),
            );
        }
    }

    ctx.shutdown()
        .await
        .context("shutdown after documents inspect")
}

async fn fetch_full_chunks(
    pool: &solo_storage::ReaderPool,
    doc_id: &solo_core::DocumentId,
) -> Result<Vec<String>> {
    let id_str = doc_id.to_string();
    let rows: Vec<String> = pool
        .interact(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT content FROM document_chunks
                  WHERE doc_id = ?1
                  ORDER BY chunk_index ASC",
            )?;
            let rows: Vec<String> = stmt
                .query_map(rusqlite::params![&id_str], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
    Ok(rows)
}

fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn fmt_status(s: &solo_core::DocumentStatus) -> &'static str {
    match s {
        solo_core::DocumentStatus::Active => "active",
        solo_core::DocumentStatus::Forgotten => "forgotten",
    }
}

fn fmt_ms(ms: i64) -> String {
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("invalid({ms})"))
}
