// SPDX-License-Identifier: Apache-2.0

//! Document parsing + chunking for v0.7.0 RAG/document memory.
//!
//! ## Pipeline
//!
//! 1. [`parse::parse_file(path)`] reads a file by path, detects format via
//!    extension, returns the normalized UTF-8 text + mime_type.
//! 2. [`chunk::chunk_text(text, &ChunkConfig)`] splits the text into chunks
//!    of roughly `ChunkConfig::target_tokens` tokens with `overlap_tokens`
//!    overlap, preferring paragraph / heading boundaries.
//! 3. The writer-actor (Priority 3) takes the chunks, embeds them via the
//!    configured Embedder, and persists into the v0.7.0 schema.
//!
//! Token counting is approximated as `chars / 4` (good for English; under-
//! estimates non-Latin). Tokenization upgrade is deferred.
//!
//! See `docs/dev-log/0083-v0.7.0-implementation-plan.md` §2 Priority 2.

pub mod chunk;
pub mod parse;

pub use chunk::{ChunkConfig, ChunkSpec, chunk_text};
pub use parse::{
    FALLBACK_BINARY_EXTRACTOR, NO_EXTRACTABLE_TEXT_ERROR_MARKER, ParseError, ParsedDocument,
    TEXT_EXTRACTOR_VERSION, extractor_name_for_mime, mime_type_for_extension, parse_file,
};
