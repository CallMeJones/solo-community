// SPDX-License-Identifier: Apache-2.0

//! `LlmClient` trait + supporting types. See ADR-0002.
//!
//! The Steward struct (`solo-steward`) consumes `Arc<dyn LlmClient>` rather
//! than implementing a Steward trait — the LLM is the swap point, not the
//! consolidation logic.

use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Backend identifier — "qwen3-coder-30b-local", "claude-sonnet-4-6",
    /// "gpt-5.6-terra", etc. Used in dev-log entries and consolidation provenance
    /// (`Provenance::by`).
    fn name(&self) -> &str;

    /// Run a single completion turn. Implementations handle their own
    /// retries, rate limits, and context-window management.
    async fn complete(&self, messages: &[Message]) -> Result<Message>;

    /// True iff this client talks to a real LLM backend (HTTP, FFI,
    /// local model), false for deterministic test stubs whose
    /// `complete()` returns canned data.
    ///
    /// Callers gate LLM-dependent work on this so the system stays
    /// usable in stub-only configurations: e.g. the writer's
    /// contradiction sweep early-returns when the steward's client is
    /// a stub, since a canned response can't faithfully arbitrate two
    /// triples it has never seen.
    ///
    /// Default `true` — production backends inherit the right answer
    /// without per-impl plumbing. The stub overrides to `false`.
    fn is_real_llm(&self) -> bool {
        true
    }
}
