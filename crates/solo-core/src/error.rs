// SPDX-License-Identifier: Apache-2.0

//! Workspace-wide error type.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("vector index error: {0}")]
    VectorIndex(String),

    #[error("embedder error: {0}")]
    Embedder(String),

    #[error("embedder protocol violation: {0}")]
    EmbedderProtocol(&'static str),

    #[error("LLM client error: {0}")]
    Llm(String),

    #[error("steward error: {0}")]
    Steward(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    /// v0.8.1 P3: a request was rejected by a policy gate rather than a
    /// data-shape problem. Used today by the per-tenant `quota_bytes`
    /// enforcement path; the transport layer translates this to a 403
    /// Forbidden in HTTP and the audit log records `result =
    /// 'forbidden'`.
    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("uuid error: {0}")]
    Uuid(#[from] uuid::Error),

    #[error("other: {0}")]
    Other(String),
}

impl Error {
    pub fn storage(msg: impl Into<String>) -> Self {
        Self::Storage(msg.into())
    }
    pub fn vector_index(msg: impl Into<String>) -> Self {
        Self::VectorIndex(msg.into())
    }
    pub fn embedder(msg: impl Into<String>) -> Self {
        Self::Embedder(msg.into())
    }
    pub fn llm(msg: impl Into<String>) -> Self {
        Self::Llm(msg.into())
    }
    pub fn steward(msg: impl Into<String>) -> Self {
        Self::Steward(msg.into())
    }
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self::InvalidInput(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict(msg.into())
    }
    /// v0.8.1 P3: construct a Forbidden error (policy rejection).
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }
}
