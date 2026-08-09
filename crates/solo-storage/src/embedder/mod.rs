// SPDX-License-Identifier: Apache-2.0

//! Embedder implementations behind the `solo_core::Embedder` trait.
//!
//! Two impls live here:
//!
//!   - [`StubEmbedder`] — deterministic hash-based F32 embedder. Used in
//!     unit tests, integration tests, and offline development. Same input
//!     always produces the same vector; vectors are normalised to unit
//!     length so cosine similarity is well-defined. Vectors are
//!     identity-only, NOT semantic.
//!
//!   - [`OllamaEmbedder`] — real semantic embeddings via a local Ollama
//!     daemon (HTTP `/api/embeddings`). The recommended production
//!     backend since v0.5.1; the default for new deployments per
//!     `docs/dev-log/0071-v0.5.x-roadmap.md`.
//!
//! ## History
//!
//! v0.5.0 shipped a third backend — BGE-M3 via `candle-transformers` —
//! and emitted a deprecation warning whenever it ran. v0.5.1 added the
//! [`OllamaEmbedder`] as the replacement. v0.6.0 (this version) removes
//! BGE-M3 entirely; users who were running it must migrate to Ollama via
//! `solo reembed` before upgrading. The migration runway was two minor
//! releases.

#[cfg(feature = "bundled-embedder")]
pub mod bundled;
pub mod ollama;
pub mod stub;

#[cfg(feature = "bundled-embedder")]
pub use bundled::{
    BUNDLED_EMBEDDER_DIM, BUNDLED_EMBEDDER_NAME, BUNDLED_EMBEDDER_VERSION, BundledEmbedder,
};
pub use ollama::{DEFAULT_OLLAMA_DIM, DEFAULT_OLLAMA_MODEL, OllamaEmbedder};
pub use stub::{STUB_EMBEDDER_NAME, StubEmbedder};

use crate::config::EmbedderConfig;
use solo_core::{Embedder, Error, Result};

const ENV_EMBEDDER_KIND: &str = "SOLO_EMBEDDER";
const ENV_OLLAMA_BASE_URL: &str = "SOLO_OLLAMA_BASE_URL";
const ENV_OLLAMA_EMBED_MODEL: &str = "SOLO_OLLAMA_EMBED_MODEL";

/// `SOLO_EMBEDDER` value that selects the bundled embedder.
///
/// Recognised on every build (feature on AND feature off) so the
/// error message + fallback path is uniform — feature-off builds
/// (musl release shards) surface a structured "feature compiled
/// without bundled embedder" error pointing at release notes rather
/// than silently treating `bundled` as an unknown kind.
pub(crate) const ENV_EMBEDDER_KIND_BUNDLED: &str = "bundled";
/// Legacy env var from the v0.5.x BGE-M3 path. Setting it in v0.6.0+ is
/// a no-op aside from a one-time warning telling the operator how to
/// migrate; kept here as a `const` so the warning + the soft-detection
/// in [`build_embedder_from_env`] / [`probe_embedder_config_from_env`]
/// reference the same string.
const ENV_BGE_M3_DIR: &str = "SOLO_BGE_M3_DIR";

const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Migration message emitted when an operator points at a backend that
/// was removed in v0.6.0 (either by setting `SOLO_EMBEDDER=bge-m3` or by
/// leaving `SOLO_BGE_M3_DIR` set after upgrading).
///
/// Two emission shapes:
///
///   - `SOLO_EMBEDDER=bge-m3` → hard error (the operator explicitly
///     asked for a backend that does not exist), via
///     [`build_embedder_from_env`].
///   - `SOLO_BGE_M3_DIR` set without `SOLO_EMBEDDER` → soft warning to
///     stderr + fall through to Ollama / Stub. Lets stale shell rc
///     files keep working after an upgrade without breaking the daemon.
const BGE_M3_REMOVED_MESSAGE: &str =
    "BGE-M3 was removed in v0.6.0; use SOLO_EMBEDDER=ollama (see docs).";

/// Soft-warning message for legacy [`ENV_BGE_M3_DIR`] without an
/// explicit `SOLO_EMBEDDER`. Distinct from [`BGE_M3_REMOVED_MESSAGE`]
/// because here we want the operator to know their env var is being
/// IGNORED (not just informed of the removal), and we hint at the next
/// concrete step.
const BGE_M3_DIR_IGNORED_MESSAGE: &str = "SOLO_BGE_M3_DIR is set but BGE-M3 was removed in v0.6.0; ignored. \
     Set SOLO_EMBEDDER=ollama to use Ollama embeddings.";

/// Structured error body for the feature-off bundled-embedder path.
/// Module-scope so [`build_embedder_from_env`] and
/// [`probe_embedder_config_from_env`] emit identical wording — keeps
/// the operator-facing message grep-able from a single source of
/// truth. v0.9.0 P3 plan §3 BLOCKER 1 strategy c.
pub(crate) const BUNDLED_EMBEDDER_FEATURE_OFF_MESSAGE: &str = "Bundled embedder requested but this binary was compiled \
     without the `bundled-embedder` Cargo feature. \
     This typically means you're on a musl Linux release shard \
     (Microsoft does not publish musl-targeted ONNX Runtime \
     prebuilts). Options:\n  \
     - Use Ollama: SOLO_EMBEDDER=ollama (see docs/releases/v0.9.0.md)\n  \
     - Use Stub (test only): leave SOLO_EMBEDDER unset\n  \
     - Build from source with --features bundled-embedder on a \
     non-musl target.";

/// Central, env-driven [`Embedder`] constructor.
///
/// **Precedence (highest to lowest)**:
///
/// 1. `SOLO_EMBEDDER=bundled` → [`BundledEmbedder`] (v0.9.0 P3). Only
///    available when the `bundled-embedder` Cargo feature is on
///    (default for non-musl targets). On feature-off builds the call
///    returns the structured error above; callers (`solo daemon`,
///    `solo init`) wrap the error in a `tracing::warn!` + fall back
///    to [`StubEmbedder::default_stub`] when in a recovery context.
/// 2. `SOLO_EMBEDDER=ollama` → [`OllamaEmbedder`] using
///    `SOLO_OLLAMA_BASE_URL` (default `http://localhost:11434`) and
///    `SOLO_OLLAMA_EMBED_MODEL` (default
///    [`DEFAULT_OLLAMA_MODEL`] = `nomic-embed-text`). Dim is hardcoded
///    to [`DEFAULT_OLLAMA_DIM`] = 768 here; `solo init` probes the
///    real dim and persists it in `solo.config.toml.embedder.dim`,
///    which `solo_cli::commands::common::build_embedder` then
///    validates against the runtime backend.
/// 3. `SOLO_EMBEDDER=bge-m3` → hard error (BGE-M3 was removed in
///    v0.6.0). The error message points at the migration path.
/// 4. `SOLO_EMBEDDER` unset, `SOLO_BGE_M3_DIR` set → emit a one-time
///    "your env var is ignored" warning, then fall through to Stub
///    (the same behaviour as case 5). Lets a stale shell rc from a
///    v0.5.x install upgrade cleanly without exploding the daemon.
/// 5. Nothing set → [`StubEmbedder::default_stub`] (`stub` / `v1` / 32).
///
/// **`SOLO_EMBEDDER` with any other value** is an error
/// ([`Error::InvalidInput`]).
///
/// ## Why this lives in `solo-storage`
///
/// `solo-cli::commands::common::build_embedder` reads `&SoloConfig` to
/// preserve the persisted Stub identity (the `embedder_registry` row
/// from a prior init). It calls this function for env resolution, then
/// rebuilds the stub with the persisted (name, version, dim) when this
/// returns the generic stub. See that function's docstring for the
/// three coordination concerns.
///
/// ## Errors
///
/// Returns `Err` if:
///   - `SOLO_EMBEDDER` is set to an unknown value (including `bge-m3`),
///   - `SOLO_EMBEDDER=bundled` is requested on a build compiled
///     without the `bundled-embedder` Cargo feature,
///   - the HTTP client for Ollama cannot be built.
pub fn build_embedder_from_env() -> Result<Box<dyn Embedder>> {
    let kind = std::env::var(ENV_EMBEDDER_KIND).ok();

    match kind.as_deref() {
        Some(ENV_EMBEDDER_KIND_BUNDLED) => build_bundled_from_env(),
        Some("ollama") => build_ollama_from_env(),
        Some("stub") => Ok(Box::new(StubEmbedder::default_stub())),
        Some("bge-m3") => Err(Error::invalid_input(BGE_M3_REMOVED_MESSAGE)),
        Some(other) if !other.is_empty() => Err(Error::invalid_input(format!(
            "Unknown {ENV_EMBEDDER_KIND} value: {other:?}. \
             Expected: bundled, ollama, stub. ({BGE_M3_REMOVED_MESSAGE})"
        ))),
        // Empty string or unset → soft handling for legacy
        // SOLO_BGE_M3_DIR; otherwise stub.
        _ => {
            if std::env::var_os(ENV_BGE_M3_DIR).is_some() {
                emit_bge_m3_dir_ignored_warning();
            }
            // Dev-log 0152 H2: previously this fell through silently
            // with only a one-shot `eprintln!`. The stub produces 32-dim
            // BLAKE3-hash vectors — clusters group on textual hash
            // proximity, not semantic similarity. Downstream LLM
            // abstractions LOOK meaningful but the membership is
            // garbage. A `tracing::warn!` makes the choice visible in
            // structured logs (operators wire log aggregation; stderr
            // gets discarded in many deployments).
            tracing::warn!(
                "no SOLO_EMBEDDER configured — falling back to StubEmbedder \
                 (32-dim BLAKE3 hash). Cluster membership and recall will \
                 be NON-SEMANTIC. Set SOLO_EMBEDDER=bundled or =ollama for \
                 real semantic vectors; see docs/embedder-setup.md. \
                 Consolidation refuses to run with the stub unless \
                 SOLO_ALLOW_STUB_EMBEDDER=1 is set."
            );
            Ok(Box::new(StubEmbedder::default_stub()))
        }
    }
}

/// Build the bundled embedder when the `bundled-embedder` Cargo
/// feature is compiled in. Off-feature builds return a structured
/// error pointing at the release-notes guidance (BLOCKER 1 strategy c).
#[cfg(feature = "bundled-embedder")]
fn build_bundled_from_env() -> Result<Box<dyn Embedder>> {
    Ok(Box::new(BundledEmbedder::new()))
}

/// Off-feature path: surface the locked structured error. Callers in
/// recovery contexts (`solo init`, `solo daemon`) catch this and fall
/// back to [`StubEmbedder::default_stub`] with a `tracing::warn!`.
#[cfg(not(feature = "bundled-embedder"))]
fn build_bundled_from_env() -> Result<Box<dyn Embedder>> {
    Err(Error::invalid_input(BUNDLED_EMBEDDER_FEATURE_OFF_MESSAGE))
}

fn build_ollama_from_env() -> Result<Box<dyn Embedder>> {
    let base_url = std::env::var(ENV_OLLAMA_BASE_URL)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());
    let model = std::env::var(ENV_OLLAMA_EMBED_MODEL)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string());
    // Hardcoded placeholder dim — `solo init` probes the real value
    // and persists it; the CLI wrapper validates dim-vs-config on
    // every subsequent call.
    let embedder = OllamaEmbedder::new(base_url, model, DEFAULT_OLLAMA_DIM)?;
    Ok(Box::new(embedder))
}

/// Emit the "your SOLO_BGE_M3_DIR is being ignored" warning to stderr
/// at most once per process. `Once::call_once` so a future caller (e.g.
/// `solo doctor`) that hits this code path multiple times doesn't spam.
fn emit_bge_m3_dir_ignored_warning() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!("{BGE_M3_DIR_IGNORED_MESSAGE}");
    });
}

/// Resolve the [`EmbedderConfig`] that `solo init` should persist to
/// `solo.config.toml`, based on the same env-var precedence as
/// [`build_embedder_from_env`].
///
/// Precedence (matches `build_embedder_from_env` exactly):
///
/// 1. `SOLO_EMBEDDER=ollama` → probe Ollama for the real dim via
///    [`OllamaEmbedder::probe_dim`] and return
///    `{ name: "ollama:<model>", version: "v1", dim: <probed>, dtype: "f32" }`.
///    **Fails fast** with a clear, operator-actionable error if the probe
///    fails (Ollama not running, model not pulled, wrong base URL).
///    Persisting a guess would only push the failure to `solo daemon` /
///    first write, where the dim check in `build_embedder` would surface
///    a less helpful error.
/// 2. `SOLO_EMBEDDER=bge-m3` → hard error (removed in v0.6.0).
/// 3. `SOLO_EMBEDDER` unset, `SOLO_BGE_M3_DIR` set → soft warning +
///    fall through to stub identity.
/// 4. Nothing set → stub identity (`stub`, `v1`, 32, `f32`). Matches
///    [`StubEmbedder::default_stub`].
///
/// `SOLO_EMBEDDER` with an unknown value is an error
/// ([`Error::InvalidInput`]) — same shape as `build_embedder_from_env`.
pub async fn probe_embedder_config_from_env() -> Result<EmbedderConfig> {
    let kind = std::env::var(ENV_EMBEDDER_KIND).ok();

    match kind.as_deref() {
        Some(ENV_EMBEDDER_KIND_BUNDLED) => probe_bundled_config(),
        Some("ollama") => probe_ollama_config_from_env().await,
        Some("stub") => {
            let stub = StubEmbedder::default_stub();
            Ok(EmbedderConfig {
                name: stub.name().to_string(),
                version: stub.version().to_string(),
                dim: stub.dim() as u32,
                dtype: "f32".to_string(),
            })
        }
        Some("bge-m3") => Err(Error::invalid_input(BGE_M3_REMOVED_MESSAGE)),
        Some(other) if !other.is_empty() => Err(Error::invalid_input(format!(
            "Unknown {ENV_EMBEDDER_KIND} value: {other:?}. \
             Expected: bundled, ollama, stub. ({BGE_M3_REMOVED_MESSAGE})"
        ))),
        // Empty string or unset → soft handling for legacy
        // SOLO_BGE_M3_DIR; otherwise default identity.
        //
        // v0.9.0 P3: when the `bundled-embedder` feature is compiled
        // in, default to the bundled identity (plan §4 P3: "if the
        // `bundled-embedder` feature compiled in, default to
        // `EmbedderConfig::Bundled`; else default to
        // `EmbedderConfig::Stub`"). musl release shards (feature off)
        // fall through to stub with a tracing::warn! at first use,
        // matching v0.8.x UX exactly. The legacy SOLO_BGE_M3_DIR
        // warning still fires regardless.
        _ => {
            if std::env::var_os(ENV_BGE_M3_DIR).is_some() {
                emit_bge_m3_dir_ignored_warning();
            }
            Ok(default_embedder_identity())
        }
    }
}

/// Default identity for `solo init` when no `SOLO_EMBEDDER` env var is
/// set. Bundled is the official Community default; feature-off developer
/// builds use stub. The choice is made at compile time via
/// `cfg!(feature = "bundled-embedder")` rather than at runtime
/// detection, matching the lesson-#19 principle that platform
/// capability is a compile-time fact.
fn default_embedder_identity() -> EmbedderConfig {
    #[cfg(feature = "bundled-embedder")]
    {
        bundled_identity()
    }
    #[cfg(not(feature = "bundled-embedder"))]
    {
        stub_identity()
    }
}

/// `solo init` identity resolver for the bundled embedder. Identity is
/// known statically (`bundled:all-MiniLM-L6-v2` / `v2` / 384 / `f32`)
/// so there's no network probe — we just return the constants.
/// Feature-off builds error with the locked release-notes message.
#[cfg(feature = "bundled-embedder")]
fn probe_bundled_config() -> Result<EmbedderConfig> {
    Ok(bundled_identity())
}

#[cfg(not(feature = "bundled-embedder"))]
fn probe_bundled_config() -> Result<EmbedderConfig> {
    Err(Error::invalid_input(BUNDLED_EMBEDDER_FEATURE_OFF_MESSAGE))
}

/// Static bundled-embedder identity. Mirrors [`BundledEmbedder::name`]
/// / `version` / `dim` exactly — kept on the feature-on path only so a
/// feature-off build doesn't accidentally hand out a config that
/// `build_embedder_from_env` would then refuse on its own host.
#[cfg(feature = "bundled-embedder")]
pub(crate) fn bundled_identity() -> EmbedderConfig {
    EmbedderConfig {
        name: BUNDLED_EMBEDDER_NAME.to_string(),
        version: BUNDLED_EMBEDDER_VERSION.to_string(),
        dim: BUNDLED_EMBEDDER_DIM as u32,
        dtype: "f32".into(),
    }
}

/// Probe Ollama for the embedding dim of `(base_url, model)` and build
/// the [`EmbedderConfig`] to persist. Reads `SOLO_OLLAMA_BASE_URL` +
/// `SOLO_OLLAMA_EMBED_MODEL` with the same defaults as
/// `build_ollama_from_env`.
///
/// Fail-fast error message includes the upstream `reqwest` error plus
/// three operator-facing options (start Ollama, point at a different
/// base URL, switch embedder backend).
async fn probe_ollama_config_from_env() -> Result<EmbedderConfig> {
    let base_url = std::env::var(ENV_OLLAMA_BASE_URL)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());
    let model = std::env::var(ENV_OLLAMA_EMBED_MODEL)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string());

    // Construct with the documented placeholder dim. `probe_dim`
    // bypasses the embedder's strict dim check, so the placeholder
    // doesn't affect correctness — the real dim comes from the server.
    let probe = OllamaEmbedder::new(base_url.clone(), model.clone(), DEFAULT_OLLAMA_DIM)?;
    let dim = probe.probe_dim().await.map_err(|e| {
        Error::embedder(format!(
            "Failed to probe the Ollama embedder's dimension at `solo init`.\n\
             Tried to embed a sentinel via {base_url}/api/embeddings using model `{model}`.\n\
             Upstream error: {e}\n\
             \n\
             Options:\n  \
             - Start Ollama: `ollama serve` (or check that the daemon is running)\n  \
             - Pull the model: `ollama pull {model}`\n  \
             - Point at a different base URL: SOLO_OLLAMA_BASE_URL=<url>\n  \
             - Use a different embedder: unset SOLO_EMBEDDER (falls back to stub).\n  \
             \n\
             Then re-run `solo init`."
        ))
    })?;

    Ok(EmbedderConfig {
        name: format!("ollama:{model}"),
        version: "v1".into(),
        dim: dim as u32,
        dtype: "f32".into(),
    })
}

/// Static stub identity. Matches [`StubEmbedder::default_stub`]'s
/// (name, version, dim) so the persisted config + the runtime stub
/// agree.
fn stub_identity() -> EmbedderConfig {
    let stub = StubEmbedder::default_stub();
    EmbedderConfig {
        name: stub.name().to_string(),
        version: stub.version().to_string(),
        dim: stub.dim() as u32,
        dtype: "f32".into(),
    }
}

#[cfg(test)]
mod build_from_env_tests {
    //! Tests for [`build_embedder_from_env`].
    //!
    //! Env vars are process-global mutable state. The cargo test runner
    //! parallelises tests within a binary, so we serialise these cases
    //! through a module-level `Mutex`. Mirrors the pattern in
    //! `solo-cli::commands::common::ollama_overrides_tests`.
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const ALL_VARS: &[&str] = &[
        ENV_EMBEDDER_KIND,
        ENV_OLLAMA_BASE_URL,
        ENV_OLLAMA_EMBED_MODEL,
        ENV_BGE_M3_DIR,
    ];

    /// Drops the env vars on Drop so a panicking test doesn't leak
    /// state into subsequent cases. Caller must hold `ENV_LOCK`.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: caller holds ENV_LOCK, so no other thread is
            // concurrently reading or writing these vars.
            for k in ALL_VARS {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    fn fresh_env() -> EnvGuard {
        // Pre-clean in case a previous test panicked before its
        // EnvGuard's Drop fired. SAFETY: ENV_LOCK held by caller.
        for k in ALL_VARS {
            unsafe { std::env::remove_var(k) };
        }
        EnvGuard
    }

    /// `Result::expect_err` requires `Debug` on the Ok side; `dyn
    /// Embedder` doesn't implement Debug (and we don't want to add it
    /// to the trait just for tests). This helper unwraps to the Err
    /// or panics with a clear message.
    fn expect_err(result: Result<Box<dyn Embedder>>, msg: &str) -> Error {
        match result {
            Ok(_) => panic!("expected Err: {msg}"),
            Err(e) => e,
        }
    }

    #[test]
    fn env_falls_back_to_stub_when_nothing_set() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        let e = build_embedder_from_env().expect("nothing set → stub");
        assert_eq!(e.name(), "stub");
        assert_eq!(e.version(), "v1");
        // `default_stub()` is 32-dim; if that constant changes,
        // update here. Keeps a regression-guard on the default.
        assert_eq!(e.dim(), 32);
    }

    #[test]
    fn env_picks_ollama_when_kind_is_ollama() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        // SAFETY: lock held; no concurrent reader/writer.
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "ollama") };
        let e = build_embedder_from_env().expect("ollama branch");
        // Default model is nomic-embed-text → name is
        // "ollama:nomic-embed-text".
        assert_eq!(e.name(), "ollama:nomic-embed-text");
        assert_eq!(e.dim(), DEFAULT_OLLAMA_DIM);
    }

    #[test]
    fn env_ollama_uses_custom_base_url_and_model() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe {
            std::env::set_var(ENV_EMBEDDER_KIND, "ollama");
            std::env::set_var(ENV_OLLAMA_BASE_URL, "http://my-host:31000");
            std::env::set_var(ENV_OLLAMA_EMBED_MODEL, "mxbai-embed-large");
        };
        let e = build_embedder_from_env().expect("custom ollama");
        // Identity reflects the override.
        assert_eq!(e.name(), "ollama:mxbai-embed-large");
        // We can't inspect base_url through the trait, but we
        // verified the impl propagates it in
        // `ollama::tests::base_url_trailing_slashes_are_trimmed`
        // and friends. Here we trust the OllamaEmbedder constructor.
    }

    #[test]
    fn env_ollama_empty_base_url_falls_back_to_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe {
            std::env::set_var(ENV_EMBEDDER_KIND, "ollama");
            std::env::set_var(ENV_OLLAMA_BASE_URL, "");
        };
        let e = build_embedder_from_env().expect("empty base url ok");
        assert_eq!(e.name(), "ollama:nomic-embed-text");
    }

    #[test]
    fn env_unknown_kind_errors_cleanly() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "bogus") };
        let err = expect_err(
            build_embedder_from_env(),
            "unknown kind must error, not panic",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("bogus"),
            "error should name the offending value: {msg}"
        );
        assert!(
            msg.contains("ollama"),
            "error should list the valid option: {msg}"
        );
    }

    /// `SOLO_EMBEDDER=bge-m3` after v0.6.0 must error with a clean
    /// migration message naming the version + the replacement.
    /// Defends against a silent fall-through that would surprise
    /// operators upgrading from v0.5.x.
    #[test]
    fn env_explicit_bge_m3_errors_with_removal_message() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "bge-m3") };
        let err = expect_err(
            build_embedder_from_env(),
            "SOLO_EMBEDDER=bge-m3 must error after v0.6.0",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("v0.6.0"),
            "error should name the removal version: {msg}"
        );
        assert!(
            msg.contains("ollama") || msg.contains("SOLO_EMBEDDER=ollama"),
            "error should point at the Ollama migration path: {msg}"
        );
    }

    /// A bare `SOLO_BGE_M3_DIR` after v0.6.0 should emit a soft
    /// warning + fall through to stub, NOT error. Goal: stale shell
    /// rc files from v0.5.x installs upgrade cleanly without breaking
    /// the daemon. Stderr capture in tests is awkward, so we assert
    /// the structural outcome (stub returned) — the warning emission
    /// is verified indirectly via `BGE_M3_DIR_IGNORED_MESSAGE`'s
    /// const reference being the only stderr path through this branch.
    #[test]
    fn env_legacy_bge_m3_dir_emits_warning_and_falls_through() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_BGE_M3_DIR, "C:/some/old/bge-m3/dir") };
        let e =
            build_embedder_from_env().expect("legacy SOLO_BGE_M3_DIR should soft-warn, not error");
        // Stub fallback: the BGE-M3 path is gone, so the env var is
        // effectively ignored.
        assert_eq!(e.name(), "stub");
        assert_eq!(e.dim(), 32);
        // Sanity check on the const text: the warning we emit names
        // both the removed-in version and the migration path.
        assert!(BGE_M3_DIR_IGNORED_MESSAGE.contains("v0.6.0"));
        assert!(BGE_M3_DIR_IGNORED_MESSAGE.contains("ollama"));
    }

    #[test]
    fn env_empty_kind_string_falls_through_to_legacy() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        // Empty SOLO_EMBEDDER + nothing else → stub. Guards against
        // an operator who did `export SOLO_EMBEDDER=` (clearing the
        // value but not the var) accidentally tripping the
        // unknown-kind error.
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "") };
        let e = build_embedder_from_env().expect("empty kind = unset");
        assert_eq!(e.name(), "stub");
    }

    // ----------------------------------------------------------------
    // v0.9.0 P3: SOLO_EMBEDDER=bundled dispatch
    // ----------------------------------------------------------------

    /// `SOLO_EMBEDDER=bundled` on a feature-on build returns the bundled
    /// embedder (no model load — just the handle).
    #[cfg(feature = "bundled-embedder")]
    #[test]
    fn env_picks_bundled_when_kind_is_bundled_and_feature_on() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        // SAFETY: lock held.
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "bundled") };
        let e = build_embedder_from_env().expect("bundled branch builds");
        assert_eq!(e.name(), "bundled:all-MiniLM-L6-v2");
        assert_eq!(e.version(), BUNDLED_EMBEDDER_VERSION);
        assert_eq!(e.dim(), 384);
    }

    /// `SOLO_EMBEDDER=bundled` on a feature-OFF build (e.g., musl
    /// release shard) returns the locked structured error pointing at
    /// release notes. Callers (init / daemon) catch this and fall
    /// back to stub.
    #[cfg(not(feature = "bundled-embedder"))]
    #[test]
    fn env_bundled_kind_rejects_when_feature_off_with_release_notes_pointer() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "bundled") };
        let err = expect_err(
            build_embedder_from_env(),
            "feature-off bundled request must error",
        );
        let msg = format!("{err}");
        assert!(msg.contains("bundled-embedder"), "names the feature: {msg}");
        assert!(msg.contains("musl"), "names the platform context: {msg}");
        assert!(msg.contains("ollama"), "points at the alternative: {msg}");
    }

    /// Unknown-kind error message must list `bundled` as a valid
    /// option (alongside `ollama`). Regression: pre-P3 the message
    /// only listed `ollama`; this test pins the new wording.
    #[test]
    fn env_unknown_kind_error_lists_bundled_as_option() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "wat") };
        let err = expect_err(
            build_embedder_from_env(),
            "unknown kind must error with both options listed",
        );
        let msg = format!("{err}");
        assert!(msg.contains("bundled"), "missing bundled hint: {msg}");
        assert!(msg.contains("ollama"), "missing ollama hint: {msg}");
    }
}

#[cfg(test)]
mod probe_config_from_env_tests {
    //! Tests for [`probe_embedder_config_from_env`] — the `solo init`
    //! identity resolver. Shares the same process-global env var
    //! concern as `build_from_env_tests`, with its own lock so the two
    //! suites don't deadlock when both run in parallel.
    use super::*;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const ALL_VARS: &[&str] = &[
        ENV_EMBEDDER_KIND,
        ENV_OLLAMA_BASE_URL,
        ENV_OLLAMA_EMBED_MODEL,
        ENV_BGE_M3_DIR,
    ];

    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in ALL_VARS {
                // SAFETY: caller holds ENV_LOCK.
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    fn fresh_env() -> EnvGuard {
        for k in ALL_VARS {
            // SAFETY: caller holds ENV_LOCK.
            unsafe { std::env::remove_var(k) };
        }
        EnvGuard
    }

    /// Build a `dim`-element f32 fixture for the mock-Ollama response.
    fn fixture(dim: usize, seed: u32) -> Vec<f32> {
        (0..dim)
            .map(|i| ((seed.wrapping_add(i as u32)) as f32) * 1e-3)
            .collect()
    }

    #[tokio::test]
    async fn probes_ollama_dim_when_kind_is_ollama() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();

        // Mock Ollama returning a 768-dim vector (matches
        // nomic-embed-text). Probe should pick this up regardless of
        // the OllamaEmbedder's placeholder dim.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture(768, 1)
            })))
            .expect(1)
            .mount(&server)
            .await;

        unsafe {
            std::env::set_var(ENV_EMBEDDER_KIND, "ollama");
            std::env::set_var(ENV_OLLAMA_BASE_URL, server.uri());
            // Leave SOLO_OLLAMA_EMBED_MODEL unset → default model.
        };

        let cfg = probe_embedder_config_from_env()
            .await
            .expect("probe should succeed against mock");
        assert_eq!(cfg.name, format!("ollama:{DEFAULT_OLLAMA_MODEL}"));
        assert_eq!(cfg.version, "v1");
        assert_eq!(cfg.dim, 768);
        assert_eq!(cfg.dtype, "f32");
    }

    #[tokio::test]
    async fn probe_picks_up_non_default_dim_from_mock_response() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();

        // Mock returns a 1024-dim vector (matches mxbai-embed-large).
        // The persisted dim must reflect the actual response, not the
        // 768 placeholder the OllamaEmbedder was constructed with.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture(1024, 99)
            })))
            .expect(1)
            .mount(&server)
            .await;

        unsafe {
            std::env::set_var(ENV_EMBEDDER_KIND, "ollama");
            std::env::set_var(ENV_OLLAMA_BASE_URL, server.uri());
            std::env::set_var(ENV_OLLAMA_EMBED_MODEL, "mxbai-embed-large");
        };

        let cfg = probe_embedder_config_from_env().await.expect("probe ok");
        assert_eq!(cfg.name, "ollama:mxbai-embed-large");
        assert_eq!(
            cfg.dim, 1024,
            "probed dim must override the 768 placeholder"
        );
    }

    #[tokio::test]
    async fn probe_fails_clearly_when_ollama_unreachable() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();

        unsafe {
            std::env::set_var(ENV_EMBEDDER_KIND, "ollama");
            // Point at a port that nothing is listening on. Loopback
            // refusal is fast on every supported platform.
            std::env::set_var(ENV_OLLAMA_BASE_URL, "http://127.0.0.1:1");
        };

        let err = probe_embedder_config_from_env()
            .await
            .expect_err("unreachable Ollama must error");
        let msg = format!("{err}");
        // Operator-actionable fragments — the error is meant to read
        // like a runbook entry.
        assert!(
            msg.contains("Failed to probe the Ollama embedder"),
            "missing top-line context: {msg}"
        );
        assert!(
            msg.contains("ollama serve") && msg.contains("ollama pull"),
            "missing remediation hints: {msg}"
        );
        assert!(
            msg.contains("SOLO_OLLAMA_BASE_URL"),
            "missing alt-URL hint: {msg}"
        );
        assert!(
            msg.contains("SOLO_EMBEDDER"),
            "missing backend-switch hint: {msg}"
        );
    }

    /// Pre-v0.9.0 P3 invariant: nothing set → stub identity. Post-P3,
    /// the default depends on `cfg!(feature = "bundled-embedder")` —
    /// feature-off (musl release shards) still defaults to stub.
    /// Feature-on builds default to bundled (covered by
    /// [`returns_bundled_identity_when_feature_on_and_no_env_set`]).
    #[cfg(not(feature = "bundled-embedder"))]
    #[tokio::test]
    async fn returns_stub_identity_when_no_env_set_feature_off() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        let cfg = probe_embedder_config_from_env().await.expect("ok");
        assert_eq!(cfg.name, "stub");
        assert_eq!(cfg.version, "v1");
        assert_eq!(cfg.dim, 32);
        assert_eq!(cfg.dtype, "f32");
    }

    /// v0.9.0 P3: feature-on builds default to the bundled identity
    /// when nothing is set in the env. The default lookup happens at
    /// compile time via `cfg!(feature = ...)`, matching lesson #19.
    #[cfg(feature = "bundled-embedder")]
    #[tokio::test]
    async fn returns_bundled_identity_when_feature_on_and_no_env_set() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        let cfg = probe_embedder_config_from_env().await.expect("ok");
        assert_eq!(cfg.name, "bundled:all-MiniLM-L6-v2");
        assert_eq!(cfg.version, BUNDLED_EMBEDDER_VERSION);
        assert_eq!(cfg.dim, 384);
        assert_eq!(cfg.dtype, "f32");
    }

    /// `SOLO_EMBEDDER=bge-m3` at `solo init` time must error with the
    /// removal message, same shape as `build_embedder_from_env`.
    #[tokio::test]
    async fn probe_explicit_bge_m3_errors_with_removal_message() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "bge-m3") };
        let err = probe_embedder_config_from_env()
            .await
            .expect_err("bge-m3 must error after v0.6.0");
        let msg = format!("{err}");
        assert!(msg.contains("v0.6.0"));
        assert!(msg.contains("ollama"));
    }

    /// `SOLO_BGE_M3_DIR` without an explicit `SOLO_EMBEDDER` at probe
    /// time falls through to the default identity (stub on
    /// feature-off, bundled on feature-on per v0.9.0 P3). Matches
    /// `build_embedder_from_env`'s soft-warning branch — the legacy
    /// SOLO_BGE_M3_DIR warning still fires regardless.
    #[tokio::test]
    async fn probe_legacy_bge_m3_dir_falls_through_to_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_BGE_M3_DIR, "C:/old/bge-m3-dir") };
        let cfg = probe_embedder_config_from_env().await.expect("ok");
        #[cfg(feature = "bundled-embedder")]
        {
            assert_eq!(cfg.name, "bundled:all-MiniLM-L6-v2");
            assert_eq!(cfg.dim, 384);
        }
        #[cfg(not(feature = "bundled-embedder"))]
        {
            assert_eq!(cfg.name, "stub");
            assert_eq!(cfg.dim, 32);
        }
    }

    #[tokio::test]
    async fn unknown_kind_errors_with_options_listed() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "bogus") };
        let err = probe_embedder_config_from_env()
            .await
            .expect_err("unknown kind must error");
        let msg = format!("{err}");
        assert!(msg.contains("bogus"));
        assert!(msg.contains("ollama"));
    }

    // ----------------------------------------------------------------
    // v0.9.0 P3: SOLO_EMBEDDER=bundled identity resolution
    // ----------------------------------------------------------------

    /// `SOLO_EMBEDDER=bundled` at `solo init` time returns the static
    /// bundled identity without any network probe (it's known a priori
    /// because the model is baked into the binary by the
    /// `bundled-embedder` Cargo feature).
    #[cfg(feature = "bundled-embedder")]
    #[tokio::test]
    async fn probe_bundled_returns_static_identity() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "bundled") };
        let cfg = probe_embedder_config_from_env()
            .await
            .expect("bundled probe should succeed without network");
        assert_eq!(cfg.name, "bundled:all-MiniLM-L6-v2");
        assert_eq!(cfg.version, BUNDLED_EMBEDDER_VERSION);
        assert_eq!(cfg.dim, 384);
        assert_eq!(cfg.dtype, "f32");
    }

    /// On a feature-off build (e.g., musl release shard), the bundled
    /// probe must error with the locked release-notes pointer so
    /// `solo init` can fall back to writing `[embedder] name =
    /// "stub"` (with a tracing::warn!) — same UX as the daemon path.
    #[cfg(not(feature = "bundled-embedder"))]
    #[tokio::test]
    async fn probe_bundled_errors_when_feature_off() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var(ENV_EMBEDDER_KIND, "bundled") };
        let err = probe_embedder_config_from_env()
            .await
            .expect_err("feature-off probe must error");
        let msg = format!("{err}");
        assert!(msg.contains("bundled-embedder"), "names the feature: {msg}");
        assert!(msg.contains("musl"), "names the platform context: {msg}");
    }
}
