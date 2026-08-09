// SPDX-License-Identifier: Apache-2.0

//! Production [`LlmClient`](solo_core::LlmClient) backends.
//!
//! Solo's stub LLM lives in `solo_steward::test_support` (deterministic,
//! no-network). This module is the production side: real backends that
//! talk to hosted or local models via HTTP / FFI / candle.
//!
//! v0.2.0 shipped **Anthropic Claude** ([`AnthropicClient`]). v0.3
//! adds **OpenAI Chat Completions** ([`OpenAIClient`]) as a sibling
//! — same `LlmClient` trait, different wire format. Future backends:
//!
//!   - **candle-Qwen3-Coder local** — offline default per ADR-0002.
//!     ~30 GB weights, GPU detection, download flow. Likely lands as
//!     a sibling crate (`solo-llm-candle`?) given the ramp.
//!
//! All backends implement `solo_core::LlmClient` so the
//! `solo_steward::Steward` interactions stay backend-agnostic.
//!
//! ## Selection precedence
//!
//! When more than one backend's env var is set, the CLI's startup
//! glue ([`crate::llm`] consumers in `solo-cli`) prefers
//! Anthropic over OpenAI. Setting `OPENAI_API_KEY` only takes
//! effect when `ANTHROPIC_API_KEY` is unset/empty. This keeps the
//! v0.2 default behavior stable for users who already had
//! Anthropic configured before upgrading.

pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod retry;

pub use anthropic::{AnthropicClient, build_anthropic_client_from_env};
pub use ollama::{OllamaChatClient, OllamaClient, is_ollama_base_url};
pub use openai::{OpenAIClient, build_openai_client_from_env};
pub use retry::RetryConfig;

use std::sync::Arc;

use solo_core::{LlmClient, Result};

use crate::config::LlmSettings;

/// Build an [`LlmClient`] from environment variables, applying the
/// precedence documented at the module level: **Anthropic first**,
/// then **OpenAI**, then `None`.
///
/// Returns:
///
///   - `Ok(Some(client))` if either provider's env var is set.
///   - `Ok(None)` if neither is set/non-empty (caller proceeds
///     without a Steward — clustering-only consolidation).
///   - `Err(_)` if an env var is set but the HTTP client can't be
///     built (rare — TLS init failure).
///
/// Single source of truth — both `solo daemon` (consolidate timer)
/// and one-shot CLI flows use this helper, so the selection rule
/// stays identical across surfaces.
pub fn build_llm_client_from_env() -> Result<Option<Arc<dyn LlmClient>>> {
    if let Some(client) = build_anthropic_client_from_env()? {
        return Ok(Some(client));
    }
    if let Some(client) = build_openai_client_from_env()? {
        return Ok(Some(client));
    }
    Ok(None)
}

/// Build an [`LlmClient`] from persisted `[llm]` settings.
///
/// When the block is absent, keep the historical environment-variable
/// fallback. When the block is present, it is authoritative: `mode =
/// "none"` disables LLM-backed derivation even if provider keys happen
/// to be present in the process environment.
pub fn build_llm_client_from_settings(
    settings: Option<&LlmSettings>,
) -> Result<Option<Arc<dyn LlmClient>>> {
    let Some(settings) = settings else {
        return build_llm_client_from_env();
    };

    match settings {
        LlmSettings::None => Ok(None),
        LlmSettings::Anthropic { api_key_env, model } => {
            let Some(key) = read_configured_key(api_key_env) else {
                return Ok(None);
            };
            Ok(Some(Arc::new(AnthropicClient::new(key, model.clone())?)))
        }
        LlmSettings::Openai { api_key_env, model } => {
            let Some(key) = read_configured_key(api_key_env) else {
                return Ok(None);
            };
            Ok(Some(Arc::new(
                OpenAIClient::new(key, model.clone())?
                    .with_json_response_format()
                    .with_temperature(0.0),
            )))
        }
        LlmSettings::Ollama { base_url, model } => Ok(Some(Arc::new(OllamaChatClient::new(
            base_url,
            model.clone(),
        )?))),
        LlmSettings::McpSampling => Ok(None),
    }
}

fn read_configured_key(env_var: &str) -> Option<String> {
    let key = std::env::var(env_var).ok().filter(|s| !s.is_empty())?;
    eprintln!(
        "warning: reading {env_var} from the process environment; it may be visible to \
         same-user processes or diagnostic tools. \
         File-based key support is a planned follow-up."
    );
    Some(key)
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("SOLO_TEST_ANTHROPIC_KEY");
                std::env::remove_var("SOLO_TEST_OPENAI_KEY");
                std::env::remove_var("ANTHROPIC_API_KEY");
                std::env::remove_var("OPENAI_API_KEY");
            }
        }
    }

    fn fresh_env() -> EnvGuard {
        let guard = EnvGuard;
        unsafe {
            std::env::remove_var("SOLO_TEST_ANTHROPIC_KEY");
            std::env::remove_var("SOLO_TEST_OPENAI_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
        }
        guard
    }

    #[test]
    fn persisted_none_disables_env_fallback() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = fresh_env();
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "hosted-key");
        }

        let client = build_llm_client_from_settings(Some(&LlmSettings::None)).expect("build");
        assert!(client.is_none());
    }

    #[test]
    fn persisted_anthropic_reads_configured_env_name() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = fresh_env();
        unsafe {
            std::env::set_var("SOLO_TEST_ANTHROPIC_KEY", "hosted-key");
        }

        let client = build_llm_client_from_settings(Some(&LlmSettings::Anthropic {
            api_key_env: "SOLO_TEST_ANTHROPIC_KEY".to_string(),
            model: "claude-test".to_string(),
        }))
        .expect("build")
        .expect("client");

        assert_eq!(client.name(), "claude-test");
    }

    #[test]
    fn persisted_ollama_uses_native_client_and_prefixes_name() {
        let client = build_llm_client_from_settings(Some(&LlmSettings::Ollama {
            base_url: "http://localhost:11434".to_string(),
            model: "qwen2.5-coder:7b".to_string(),
        }))
        .expect("build")
        .expect("client");

        assert_eq!(client.name(), "ollama:qwen2.5-coder:7b");
        assert_eq!(
            ollama::native_ollama_base_url("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
    }
}
