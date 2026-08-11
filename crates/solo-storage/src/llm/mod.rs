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

use crate::config::{LlmSettings, OllamaEndpointKind};

const MAX_LLM_RESPONSE_BYTES: usize = 1024 * 1024;

async fn bounded_response_bytes(
    mut response: reqwest::Response,
    provider: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_LLM_RESPONSE_BYTES as u64)
    {
        return Err(solo_core::Error::llm(format!(
            "{provider} response exceeded {MAX_LLM_RESPONSE_BYTES} bytes"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| solo_core::Error::llm(format!("read {provider} response: {error}")))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_LLM_RESPONSE_BYTES {
            return Err(solo_core::Error::llm(format!(
                "{provider} response exceeded {MAX_LLM_RESPONSE_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

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
        require_legacy_hosted_consent("Anthropic")?;
        return Ok(Some(client));
    }
    if let Some(client) = build_openai_client_from_env()? {
        let base_url = std::env::var("OPENAI_BASE_URL").unwrap_or_default();
        // An absent base URL means OpenAI's hosted default, not localhost.
        if base_url.trim().is_empty() || !is_loopback_url(&base_url) {
            require_legacy_hosted_consent("OpenAI")?;
        }
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
        LlmSettings::Anthropic {
            api_key_env,
            model,
            hosted_processing_consent,
        } => {
            require_hosted_consent(*hosted_processing_consent, "Anthropic")?;
            let Some(key) = read_configured_key(api_key_env) else {
                return Ok(None);
            };
            Ok(Some(Arc::new(AnthropicClient::new(key, model.clone())?)))
        }
        LlmSettings::Openai {
            api_key_env,
            model,
            hosted_processing_consent,
        } => {
            require_hosted_consent(*hosted_processing_consent, "OpenAI")?;
            let Some(key) = read_configured_key(api_key_env) else {
                return Ok(None);
            };
            Ok(Some(Arc::new(
                OpenAIClient::new(key, model.clone())?
                    .with_json_response_format()
                    .with_temperature(0.0),
            )))
        }
        LlmSettings::Ollama {
            endpoint,
            base_url,
            model,
            api_key_env,
            hosted_processing_consent,
        } => {
            let processes_off_device = ollama_processes_off_device(*endpoint, base_url, model);
            if processes_off_device {
                require_hosted_consent(*hosted_processing_consent, "Ollama")?;
            }
            let token = match api_key_env.as_deref() {
                Some(env_var) => read_configured_key(env_var),
                None => None,
            };
            if matches!(endpoint, OllamaEndpointKind::Cloud) {
                if is_loopback_url(base_url) {
                    return Err(solo_core::Error::invalid_input(
                        "direct Ollama Cloud must use a remote base URL; configure endpoint=local with a -cloud model for a signed-in local Ollama daemon",
                    ));
                }
                if token.is_none() {
                    return Ok(None);
                }
            }
            if matches!(endpoint, OllamaEndpointKind::Local) && !is_loopback_url(base_url) {
                return Err(solo_core::Error::invalid_input(
                    "local Ollama must use a loopback base URL; configure endpoint=custom for another host",
                ));
            }
            let mut client = OllamaChatClient::new(base_url, model.clone())?;
            if let Some(token) = token {
                client = client.with_bearer_token(token);
            }
            client = if model.ends_with("-cloud") {
                client
                    .with_structured_outputs(false)
                    .with_format_fallback(false)
                    .with_display_prefix("ollama-cloud")
            } else {
                match endpoint {
                    OllamaEndpointKind::Local => client.with_display_prefix("ollama-local"),
                    OllamaEndpointKind::Cloud => client
                        .with_structured_outputs(false)
                        .with_format_fallback(false)
                        .with_display_prefix("ollama-cloud"),
                    OllamaEndpointKind::Custom => client.with_display_prefix("ollama-remote"),
                }
            };
            Ok(Some(Arc::new(client)))
        }
        LlmSettings::McpSampling => Ok(None),
    }
}

fn require_hosted_consent(consented: bool, provider: &str) -> Result<()> {
    if consented {
        return Ok(());
    }
    Err(solo_core::Error::invalid_input(format!(
        "{provider} Steward processing is hosted and requires explicit consent; enable hosted_processing_consent after reviewing where memory content is processed"
    )))
}

fn require_legacy_hosted_consent(provider: &str) -> Result<()> {
    let consented = std::env::var("SOLO_HOSTED_PROCESSING_CONSENT")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        });
    require_hosted_consent(consented, provider)
}

fn is_loopback_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return true;
    }
    reqwest::Url::parse(trimmed)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

fn ollama_processes_off_device(endpoint: OllamaEndpointKind, base_url: &str, model: &str) -> bool {
    // Classify by the actual destination as well as the UI preset. A hand-
    // edited `endpoint = "local"` must not bypass consent for a remote URL.
    if !is_loopback_url(base_url) {
        return true;
    }
    if model.ends_with("-cloud") {
        return true;
    }
    match endpoint {
        OllamaEndpointKind::Cloud => true,
        OllamaEndpointKind::Local | OllamaEndpointKind::Custom => false,
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
            hosted_processing_consent: true,
        }))
        .expect("build")
        .expect("client");

        assert_eq!(client.name(), "claude-test");
    }

    #[test]
    fn persisted_ollama_uses_native_client_and_prefixes_name() {
        let client = build_llm_client_from_settings(Some(&LlmSettings::Ollama {
            endpoint: OllamaEndpointKind::Local,
            base_url: "http://localhost:11434".to_string(),
            model: "qwen2.5-coder:7b".to_string(),
            api_key_env: None,
            hosted_processing_consent: false,
        }))
        .expect("build")
        .expect("client");

        assert_eq!(client.name(), "ollama-local:qwen2.5-coder:7b");
        assert_eq!(
            ollama::native_ollama_base_url("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn remote_custom_ollama_requires_consent_but_loopback_custom_does_not() {
        let remote = build_llm_client_from_settings(Some(&LlmSettings::Ollama {
            endpoint: OllamaEndpointKind::Custom,
            base_url: "https://localhost.attacker.example".to_string(),
            model: "qwen3:8b".to_string(),
            api_key_env: None,
            hosted_processing_consent: false,
        }));
        assert!(remote.is_err());

        let local = build_llm_client_from_settings(Some(&LlmSettings::Ollama {
            endpoint: OllamaEndpointKind::Custom,
            base_url: "http://127.0.0.1:11434".to_string(),
            model: "qwen3:8b".to_string(),
            api_key_env: None,
            hosted_processing_consent: false,
        }))
        .expect("build")
        .expect("client");
        assert_eq!(local.name(), "ollama-remote:qwen3:8b");
    }

    #[test]
    fn local_ollama_endpoint_rejects_remote_urls_even_with_consent() {
        let settings = |consent| LlmSettings::Ollama {
            endpoint: OllamaEndpointKind::Local,
            base_url: "https://ollama.example.test".to_string(),
            model: "qwen3:8b".to_string(),
            api_key_env: None,
            hosted_processing_consent: consent,
        };
        assert!(build_llm_client_from_settings(Some(&settings(false))).is_err());
        assert!(build_llm_client_from_settings(Some(&settings(true))).is_err());
    }

    #[test]
    fn signed_in_local_daemon_cloud_model_still_requires_hosted_consent() {
        let settings = |consent| LlmSettings::Ollama {
            endpoint: OllamaEndpointKind::Local,
            base_url: "http://localhost:11434".to_string(),
            model: "gpt-oss:120b-cloud".to_string(),
            api_key_env: None,
            hosted_processing_consent: consent,
        };
        assert!(build_llm_client_from_settings(Some(&settings(false))).is_err());

        let client = build_llm_client_from_settings(Some(&settings(true)))
            .expect("build")
            .expect("client");
        assert_eq!(client.name(), "ollama-cloud:gpt-oss:120b-cloud");

        let mut custom_loopback = LlmSettings::Ollama {
            endpoint: OllamaEndpointKind::Custom,
            base_url: "http://127.0.0.1:22434".to_string(),
            model: "gpt-oss:120b-cloud".to_string(),
            api_key_env: None,
            hosted_processing_consent: false,
        };
        assert!(build_llm_client_from_settings(Some(&custom_loopback)).is_err());
        if let LlmSettings::Ollama {
            hosted_processing_consent,
            ..
        } = &mut custom_loopback
        {
            *hosted_processing_consent = true;
        }
        let custom_client = build_llm_client_from_settings(Some(&custom_loopback))
            .expect("build")
            .expect("client");
        assert_eq!(custom_client.name(), "ollama-cloud:gpt-oss:120b-cloud");
    }

    #[test]
    fn direct_cloud_endpoint_rejects_a_loopback_base_url() {
        let settings = LlmSettings::Ollama {
            endpoint: OllamaEndpointKind::Cloud,
            base_url: "http://localhost:11434".to_string(),
            model: "gpt-oss:120b-cloud".to_string(),
            api_key_env: Some("OLLAMA_API_KEY".to_string()),
            hosted_processing_consent: true,
        };

        assert!(build_llm_client_from_settings(Some(&settings)).is_err());
    }
}
