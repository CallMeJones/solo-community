// SPDX-License-Identifier: Apache-2.0

//! Legacy MCP-sampling adapters retained for compatibility tests.
//!
//! Production LLM clients (Anthropic, OpenAI, Ollama) live in
//! `solo-storage::llm`. SEP-2577 retired the live MCP callback path.

pub mod sampling;
pub mod sampling_coordinator;

pub use sampling::{DEFAULT_SAMPLING_TIMEOUT, SamplingClient, SamplingError, SamplingLlmClient};
pub use sampling_coordinator::{
    DEFAULT_COALESCE_MAX_BATCH, DEFAULT_COALESCE_WINDOW, SamplingCoordinator,
};
