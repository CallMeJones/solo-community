// SPDX-License-Identifier: Apache-2.0

//! Solo core: shared types, traits, and error definitions used across the
//! workspace.
//!
//! This crate has no production-side dependencies on storage, indexing, or LLM
//! runtimes — it is the "contract" layer that those crates implement against.
//! See `docs/adr/0002-traits.md` for the design rationale behind the trait
//! shapes (`Embedder`, `VectorIndex`, `LlmClient`).

pub mod build_info;
pub mod embedder;
pub mod error;
pub mod llm;
pub mod project_memory;
pub mod types;
pub mod vector_index;

pub use embedder::Embedder;
pub use error::{Error, Result};
pub use llm::{LlmClient, Message, Role};
pub use project_memory::{
    ProjectMemoryDescriptor, ProjectPolicyClient, project_decision_content,
    project_decision_encoding_context, project_decision_scope_matches, project_decision_source_id,
    render_project_policy,
};
pub use types::{
    AssetId, AttachmentId, ChunkId, Cluster, Confidence, Contradiction, ContradictionKind,
    DEFAULT_TENANT_ID, Document, DocumentChunk, DocumentId, DocumentStatus, Embedding,
    EmbeddingDtype, EncodingContext, Episode, InvalidateEvent, LibraryId, MemoryId, Provenance,
    SemanticAbstraction, TENANT_ID_MAX_LEN, TenantIdError, Tier, Triple, TripleObjectKind,
};
pub use vector_index::{VectorIndex, VectorIndexFactory};
