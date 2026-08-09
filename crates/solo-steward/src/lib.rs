// SPDX-License-Identifier: Apache-2.0

//! Solo steward: consolidation pass (SWS-equivalent dedup, REM-equivalent
//! integration, decay sweep, reconsolidation).
//!
//! Per ADR-0002, `Steward` is a struct, not a trait. The LLM backend is
//! the swap point (via `solo_core::LlmClient`); the consolidation logic
//! lives in one place.
//!
//! ## v0.2 status
//!
//!   - **`cluster_episodes`** (this commit): pure-deterministic SWS
//!     pass. See [`cluster`] for the algorithm + tests. Implementation
//!     does not call the LLM and does not touch the DB — caller pairs
//!     `(Episode, Embedding)` from SQL upstream.
//!   - **`abstract_cluster`**: TODO — REM-equivalent LLM call to
//!     produce a [`SemanticAbstraction`] for each cluster. Will go in
//!     `crate::abstraction` when added.
//!   - **`detect_contradiction`**: TODO — bi-temporal SQL filter +
//!     LLM judge for ambiguous cases. Will go in `crate::contradiction`.
//!
//! Storage wiring (writer → cluster → SQL persistence) lands in
//! `solo-storage` separately.

use std::sync::Arc;
use std::time::Duration;

use solo_core::{
    Cluster, Contradiction, Embedding, Episode, Error, LlmClient, Result, SemanticAbstraction,
    Triple,
};

/// Env var name for tuning the centroid-cosine threshold used by
/// every clustering / existing-merge / merge-candidate count site.
/// One knob governs them all to preserve the SQL-sync invariant
/// between the writer's existing-merge and the doctor's count
/// (see `solo_storage::merge_candidates` module docstring).
pub const ENV_CLUSTER_COSINE_THRESHOLD: &str = "SOLO_CLUSTER_COSINE_THRESHOLD";

/// Env var name for tuning the minimum cluster size (number of
/// episodes) below which a candidate cluster is discarded by the
/// SWS-equivalent clustering pass. Must be a positive integer.
/// Default 3.
pub const ENV_CLUSTER_MIN_SIZE: &str = "SOLO_CLUSTER_MIN_SIZE";

/// Env var name for tuning the maximum cluster size before a connected
/// component is split into chronological chunks. Must be a positive integer.
/// Default 48.
pub const ENV_CLUSTER_MAX_SIZE: &str = "SOLO_CLUSTER_MAX_SIZE";

/// Env var name for tuning the abstraction LLM call's
/// `max_tokens` request. Must be a positive integer in
/// `[1, 65_536]` (upper bound catches typo'd values like
/// `1_000_000` that would silently inflate per-call cost).
/// Default 512.
pub const ENV_ABSTRACTION_MAX_TOKENS: &str = "SOLO_ABSTRACTION_MAX_TOKENS";

/// Env var name for the contradiction-detection toggle. `true` or
/// `false`, case-insensitive. When `false`, the consolidation pass
/// skips the rule filter + LLM judge entirely and
/// `contradictions_found` stays 0. Useful for cost-constrained
/// operators willing to lose contradiction tracking. Default
/// `true`.
pub const ENV_CONTRADICTION_CHECK_ENABLED: &str = "SOLO_CONTRADICTION_CHECK_ENABLED";

pub mod abstraction;
pub mod cluster;
pub mod contradiction;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub struct Steward {
    client: Arc<dyn LlmClient>,
    config: StewardConfig,
}

#[derive(Debug, Clone)]
pub struct StewardConfig {
    pub cluster_min_size: usize,
    pub cluster_max_size: usize,
    pub cluster_cosine_threshold: f32,
    pub abstraction_max_tokens: usize,
    pub contradiction_check_enabled: bool,
}

impl Default for StewardConfig {
    fn default() -> Self {
        Self {
            // Lowered from 3 → 2 to let small-corpus operators see
            // clustering kick in on ≤30 episodes. The original 3 is
            // appropriate at scale but creates a "nothing happens for
            // weeks" UX for a personal-memory user with sparse activity.
            cluster_min_size: 2,
            // Connected components can chain many loosely related memories
            // into one oversized prompt. Keep abstraction clusters small
            // enough to remain specific, useful, and auditable.
            cluster_max_size: 48,
            // Lowered from 0.85 → 0.55 for the bundled `all-MiniLM-L6-v2`
            // embedder, which produces conservative similarity scores
            // (topically-related pairs land in the 0.45-0.65 band rather
            // than 0.80+). The 0.85 default was tuned for a stronger
            // embedder; on the bundled model it catches near-paraphrases
            // only and starves the consolidate pass of clusters.
            //
            // v0.11.1: both `cluster_min_size` and `cluster_cosine_threshold`
            // are also exposed via `[steward]` in `solo.config.toml` (see
            // `solo_storage::StewardSettings`). Operators who want
            // different values from these baked-in defaults set them
            // there (or via the long-standing `SOLO_CLUSTER_*` env vars,
            // which still win); these defaults are now the floor when
            // neither TOML nor env touches the knob.
            cluster_cosine_threshold: 0.55,
            abstraction_max_tokens: 512,
            contradiction_check_enabled: true,
        }
    }
}

/// v0.10.1 (P4 audit m5): outcome of [`Steward::extract_triples_batch`].
///
/// Carries both the per-cluster successes (each abstraction is keyed by
/// its cluster's `MemoryId`) AND the count of clusters that timed out
/// during their per-cluster LLM call. The deferred count surfaces in
/// the writer-actor's `MemoryTriplesExtract` audit row as
/// `details_json.clusters_deferred`, giving operators an explicit
/// "something is slow" signal in the audit log without having to grep
/// `tracing::warn!` lines.
///
/// Pure-Rust failures (LLM returns an error result, not a timeout) are
/// LOGGED but NOT counted in `deferred_count`. They're the implicit
/// `clusters_failed` half of the audit row, computed by the writer as
/// `cluster_count - abstractions_built - clusters_deferred`. The split
/// is operator-actionable: deferred → slow backend, failed → backend
/// rejected the prompt.
#[derive(Debug, Default, Clone)]
pub struct ExtractTriplesBatchOutcome {
    /// One entry per cluster whose `abstract_cluster` call succeeded.
    /// Iteration order matches the input order; the daemon-side
    /// `AttachAbstractionBatch` handler persists these in a single
    /// outer tx (with per-cluster `SAVEPOINT`s, per the v0.9.0 P4-
    /// revision M2 hardening).
    pub abstractions: Vec<(solo_core::MemoryId, SemanticAbstraction)>,
    /// Number of clusters whose `abstract_cluster` call exceeded
    /// `per_cluster_timeout`. Not present in `abstractions`; the
    /// cluster's lack of a `semantic_abstractions` row will re-select
    /// it on the next batch tick.
    pub deferred_count: usize,
    /// Number of clusters whose LLM call returned an error before a
    /// parseable abstraction was produced. Distinct from timeout
    /// deferrals.
    pub failed_count: usize,
    /// Last per-cluster error observed during the batch, for operator
    /// diagnostics. The cluster remains retryable.
    pub last_error: Option<String>,
}

/// Read an env var, trimming whitespace. Returns `Some(value)` only
/// if the var is set AND the trimmed value is non-empty. Treating
/// empty strings as "unset" guards against shells where setting a
/// var to nothing means "leave it default" rather than "force the
/// empty value through the parser".
fn env_trimmed(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|s| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

impl StewardConfig {
    /// Build a config from process env vars, falling back to
    /// [`StewardConfig::default`] for unset / empty fields.
    ///
    /// Honoured env vars (all optional; unset → default):
    ///
    ///   - [`ENV_CLUSTER_COSINE_THRESHOLD`] — finite f32 in
    ///     `(0.0, 1.0]`. Default `0.55`. Affects writer's clustering
    ///     pass, in-run merge, existing-vs-existing merge, and
    ///     doctor's merge-candidate count uniformly (SQL+threshold-
    ///     sync invariant — see `solo_storage::merge_candidates`).
    ///   - [`ENV_CLUSTER_MIN_SIZE`] — positive integer. Default `2`.
    ///     Clusters with fewer episodes than this are discarded by
    ///     the SWS-equivalent clustering pass.
    ///   - [`ENV_ABSTRACTION_MAX_TOKENS`] — positive integer in
    ///     `[1, 65_536]`. Default `512`. The `max_tokens` value the
    ///     abstraction LLM call sends. Upper bound catches typo'd
    ///     values that would silently inflate per-call cost.
    ///   - [`ENV_CONTRADICTION_CHECK_ENABLED`] — `true` or `false`,
    ///     case-insensitive. Default `true`. When `false`, skips
    ///     the contradiction rule filter + LLM judge entirely.
    ///
    /// Returns `Err(Error::InvalidInput)` if any env var is set but
    /// can't be parsed or is out of range. Operators would rather
    /// see early failure than silently fall back to defaults (which
    /// would mask a typo'd value). All-or-nothing: if any field
    /// fails validation, the whole call errors and the prior fields'
    /// successful parses don't take effect — daemon/consolidate
    /// startup fails, operator fixes the env, retries.
    pub fn from_env() -> Result<Self> {
        let mut cfg = Self::default();

        if let Some(raw) = env_trimmed(ENV_CLUSTER_COSINE_THRESHOLD) {
            let parsed: f32 = raw.parse().map_err(|_| {
                Error::invalid_input(format!(
                    "{ENV_CLUSTER_COSINE_THRESHOLD}: not a valid f32 ({raw:?})"
                ))
            })?;
            if !parsed.is_finite() || parsed <= 0.0 || parsed > 1.0 {
                return Err(Error::invalid_input(format!(
                    "{ENV_CLUSTER_COSINE_THRESHOLD}: must be a finite f32 in (0.0, 1.0], got {parsed}"
                )));
            }
            cfg.cluster_cosine_threshold = parsed;
        }

        if let Some(raw) = env_trimmed(ENV_CLUSTER_MIN_SIZE) {
            let parsed: usize = raw.parse().map_err(|_| {
                Error::invalid_input(format!(
                    "{ENV_CLUSTER_MIN_SIZE}: not a valid non-negative integer ({raw:?})"
                ))
            })?;
            if parsed < 1 {
                return Err(Error::invalid_input(format!(
                    "{ENV_CLUSTER_MIN_SIZE}: must be >= 1, got {parsed}"
                )));
            }
            cfg.cluster_min_size = parsed;
        }

        if let Some(raw) = env_trimmed(ENV_CLUSTER_MAX_SIZE) {
            let parsed: usize = raw.parse().map_err(|_| {
                Error::invalid_input(format!(
                    "{ENV_CLUSTER_MAX_SIZE}: not a valid non-negative integer ({raw:?})"
                ))
            })?;
            if parsed < 1 {
                return Err(Error::invalid_input(format!(
                    "{ENV_CLUSTER_MAX_SIZE}: must be >= 1, got {parsed}"
                )));
            }
            cfg.cluster_max_size = parsed;
        }

        if let Some(raw) = env_trimmed(ENV_ABSTRACTION_MAX_TOKENS) {
            let parsed: usize = raw.parse().map_err(|_| {
                Error::invalid_input(format!(
                    "{ENV_ABSTRACTION_MAX_TOKENS}: not a valid non-negative integer ({raw:?})"
                ))
            })?;
            if !(1..=65_536).contains(&parsed) {
                return Err(Error::invalid_input(format!(
                    "{ENV_ABSTRACTION_MAX_TOKENS}: must be in [1, 65536], got {parsed}"
                )));
            }
            cfg.abstraction_max_tokens = parsed;
        }

        if let Some(raw) = env_trimmed(ENV_CONTRADICTION_CHECK_ENABLED) {
            let parsed = match raw.to_ascii_lowercase().as_str() {
                "true" => true,
                "false" => false,
                _ => {
                    return Err(Error::invalid_input(format!(
                        "{ENV_CONTRADICTION_CHECK_ENABLED}: must be \"true\" or \"false\" (case-insensitive), got {raw:?}"
                    )));
                }
            };
            cfg.contradiction_check_enabled = parsed;
        }

        Ok(cfg)
    }

    /// v0.11.1: Build a config by layering TOML overrides under env-var
    /// overrides, both on top of [`StewardConfig::default`]. Used by the
    /// daemon and one-shot CLI paths so an operator can pin
    /// `cluster_min_size` + `cluster_cosine_threshold` in
    /// `solo.config.toml` and still let `SOLO_CLUSTER_*` env vars win
    /// per-runtime (the v0.10.x escape-hatch contract).
    ///
    /// Resolution order (lowest precedence first):
    ///   1. [`StewardConfig::default`]
    ///   2. TOML `[steward]` block — `Some(_)` values from the supplied
    ///      `toml_min_size` / `toml_cosine_threshold`
    ///   3. Env vars — `SOLO_CLUSTER_MIN_SIZE` / `SOLO_CLUSTER_COSINE_THRESHOLD`
    ///      via [`Self::from_env`]
    ///
    /// The two TOML values are passed by value rather than wrapped in a
    /// dedicated struct so `solo-steward` doesn't depend on
    /// `solo-storage::config::StewardSettings` (which would be circular
    /// — `solo-storage` already deps on `solo-steward`). The
    /// `solo-storage` side owns the TOML schema; this constructor's
    /// signature is the carry-the-Option boundary.
    ///
    /// Validation: both TOML overrides are validated to the same bounds
    /// as the env vars (`cluster_min_size >= 1`,
    /// `cluster_cosine_threshold` finite in `(0.0, 1.0]`). A bad TOML
    /// value surfaces as `Error::InvalidInput` with the bound and the
    /// offending value, matching the env-var error messages so the
    /// operator sees the same diagnostic regardless of which surface
    /// they used.
    pub fn from_settings_then_env(
        toml_min_size: Option<usize>,
        toml_cosine_threshold: Option<f32>,
    ) -> Result<Self> {
        let mut cfg = Self::default();

        if let Some(parsed) = toml_min_size {
            if parsed < 1 {
                return Err(Error::invalid_input(format!(
                    "[steward] cluster_min_size: must be >= 1, got {parsed}"
                )));
            }
            cfg.cluster_min_size = parsed;
        }

        if let Some(parsed) = toml_cosine_threshold {
            if !parsed.is_finite() || parsed <= 0.0 || parsed > 1.0 {
                return Err(Error::invalid_input(format!(
                    "[steward] cluster_cosine_threshold: must be a finite f32 in (0.0, 1.0], got {parsed}"
                )));
            }
            cfg.cluster_cosine_threshold = parsed;
        }

        // Layer env vars on top. We re-run the env parser (rather than
        // skip-to-fields) so `abstraction_max_tokens` /
        // `contradiction_check_enabled` continue to read from env — the
        // TOML surface only owns the clustering pair.
        let env_min_size_present = env_trimmed(ENV_CLUSTER_MIN_SIZE).is_some();
        let env_threshold_present = env_trimmed(ENV_CLUSTER_COSINE_THRESHOLD).is_some();
        let env_cfg = Self::from_env()?;
        if env_min_size_present {
            cfg.cluster_min_size = env_cfg.cluster_min_size;
        }
        if env_threshold_present {
            cfg.cluster_cosine_threshold = env_cfg.cluster_cosine_threshold;
        }
        // The non-cluster env-driven fields always take effect — TOML
        // doesn't carry these, so `env_cfg`'s values (which equal default
        // when env was unset) are the right source of truth.
        cfg.cluster_max_size = env_cfg.cluster_max_size;
        cfg.abstraction_max_tokens = env_cfg.abstraction_max_tokens;
        cfg.contradiction_check_enabled = env_cfg.contradiction_check_enabled;

        Ok(cfg)
    }
}

impl Steward {
    pub fn new(client: Arc<dyn LlmClient>, config: StewardConfig) -> Self {
        Self { client, config }
    }

    /// Borrow the configured thresholds. Useful for callers that want
    /// to call `cluster::cluster_episodes` directly (e.g. tests, or
    /// future fine-grained pipelines).
    pub fn config(&self) -> &StewardConfig {
        &self.config
    }

    /// True iff this `Steward`'s LLM client talks to a real backend
    /// (Anthropic, OpenAI, Ollama, future candle/local). False for
    /// stub clients used in tests.
    ///
    /// Callers gate LLM-dependent work on this so the system stays
    /// usable in stub-only configurations: e.g. the writer's
    /// contradiction sweep early-returns when this is `false`,
    /// because a stub's canned response can't faithfully arbitrate
    /// triples it has never seen. The cheap clustering pass and the
    /// abstraction pass remain reachable through their own gates.
    ///
    /// Delegates to [`solo_core::LlmClient::is_real_llm`], the per-
    /// backend distinction.
    pub fn has_llm(&self) -> bool {
        self.client.is_real_llm()
    }

    /// Runtime identity of the LLM client backing this Steward.
    pub fn llm_name(&self) -> &str {
        self.client.name()
    }

    /// SWS-equivalent: cluster the supplied `(Episode, Embedding)`
    /// pairs by UTC day + cosine threshold. Pure-deterministic; does
    /// not call the LLM. See [`cluster::cluster_episodes`] for the
    /// algorithm.
    pub async fn cluster_episodes(&self, inputs: &[(Episode, Embedding)]) -> Result<Vec<Cluster>> {
        // Trivial wrapper today; preserves the async signature for
        // when the algorithm grows entity-aware bucketing that may
        // need to fan out (e.g. small per-entity LLM checks).
        cluster::cluster_episodes(inputs, &self.config)
    }

    /// REM-equivalent: ask the LLM to produce a semantic abstraction
    /// of a cluster. Provenance is preserved — `derived_from`
    /// references every source episode.
    ///
    /// `episodes` must contain (at minimum) every `MemoryId` in
    /// `cluster.episode_ids`. The Steward intentionally does not
    /// hold a DB handle (per ADR-0002 "Steward depends only on
    /// solo-core"), so the caller pairs cluster + episodes upstream.
    pub async fn abstract_cluster(
        &self,
        cluster: &Cluster,
        episodes: &[Episode],
    ) -> Result<SemanticAbstraction> {
        abstraction::abstract_cluster(cluster, episodes, self.client.as_ref()).await
    }

    /// v0.9.0 P4c: batch entry-point for the daemon-side consolidate
    /// timer. For each `(cluster, episodes)` pair, calls
    /// [`Self::abstract_cluster`] and accumulates the resulting
    /// `SemanticAbstraction` (with its embedded `triples`) into the
    /// returned [`ExtractTriplesBatchOutcome::abstractions`].
    ///
    /// Per-cluster failures are LOGGED but do NOT abort the batch:
    /// the daemon-side caller persists every successful abstraction
    /// in a single `WriteCommand::AttachAbstractionBatch` tx. A
    /// half-failed batch (some clusters succeeded, some failed) is
    /// the documented behavior — the failed clusters retry on the
    /// next tick. This matches the v0.8.x writer-actor's
    /// per-cluster log-and-skip discipline; we just moved the loop.
    ///
    /// v0.10.1 (P4 audit m5): each per-cluster `abstract_cluster`
    /// call is wrapped in [`tokio::time::timeout`] with the supplied
    /// `per_cluster_timeout`. A hung LLM call (MCP peer that never
    /// responds, slow Ollama model, network stall on Anthropic) no
    /// longer blocks the next cluster — the timeout fires, the
    /// cluster is counted as "deferred", the batch continues. The
    /// deferred cluster's lack of a `semantic_abstractions` row means
    /// the NEXT batch tick will pick it up again automatically (the
    /// reader pool's `fetch_clusters_without_abstractions` query
    /// selects on the same predicate).
    ///
    /// A `per_cluster_timeout` of `Duration::ZERO` DISABLES the
    /// per-cluster timeout — every call runs to natural completion.
    /// Symmetric with the `[triples] cluster_timeout_secs = 0`
    /// operator escape hatch in `solo.config.toml`. Useful for
    /// operators on very slow local backends; not recommended in
    /// production (a single hung peer can stall the batch).
    ///
    /// Cancellation note (v0.10.1): when the timeout fires,
    /// `tokio::time::timeout` drops the inner `abstract_cluster`
    /// future. If the underlying LLM transport doesn't honor
    /// cancellation (some HTTP clients with in-flight requests may
    /// not), the actual RPC may complete in the background. The
    /// response is discarded — no double-write — but the LLM-side
    /// work is wasted. Acceptable for v0.10.1.
    ///
    /// Output shape: [`ExtractTriplesBatchOutcome`] carries the
    /// abstractions vec + a `deferred_count` for the audit row. The
    /// caller persists `semantic_abstractions(content, provenance,
    /// confidence)` + N `triples` rows per entry, and threads
    /// `deferred_count` into the `MemoryTriplesExtract` audit row's
    /// `details_json.clusters_deferred`.
    ///
    /// `Steward::has_llm() == false` is a NO-OP fast path: returns
    /// an outcome with empty abstractions + zero deferred. Mirrors
    /// the v0.8.x writer-actor's "if no LLM wired, skip abstraction"
    /// gate.
    pub async fn extract_triples_batch(
        &self,
        clusters_with_episodes: Vec<(Cluster, Vec<Episode>)>,
        per_cluster_timeout: Duration,
    ) -> ExtractTriplesBatchOutcome {
        if !self.has_llm() {
            return ExtractTriplesBatchOutcome::default();
        }
        let mut abstractions: Vec<(solo_core::MemoryId, SemanticAbstraction)> =
            Vec::with_capacity(clusters_with_episodes.len());
        let mut deferred_count: usize = 0;
        let mut failed_count: usize = 0;
        let mut last_error: Option<String> = None;
        let timeout_enabled = !per_cluster_timeout.is_zero();
        for (cluster, episodes) in clusters_with_episodes {
            let cluster_id = cluster.cluster_id;
            let call = self.abstract_cluster(&cluster, &episodes);
            let result = if timeout_enabled {
                match tokio::time::timeout(per_cluster_timeout, call).await {
                    Ok(inner) => inner,
                    Err(_elapsed) => {
                        tracing::warn!(
                            cluster_id = %cluster_id,
                            timeout_secs = per_cluster_timeout.as_secs(),
                            "v0.10.1 m5 extract_triples_batch: abstract_cluster \
                             timed out; deferring cluster to next batch tick \
                             (the cluster's lack of a semantic_abstractions \
                             row will re-select it on the next pass)"
                        );
                        deferred_count += 1;
                        continue;
                    }
                }
            } else {
                call.await
            };
            match result {
                Ok(abs) => {
                    abstractions.push((cluster_id, abs));
                }
                Err(e) => {
                    failed_count += 1;
                    last_error = Some(e.to_string());
                    tracing::warn!(
                        cluster_id = %cluster_id,
                        error = %e,
                        "v0.9.0 P4c extract_triples_batch: abstract_cluster \
                         failed; cluster persists, abstraction retries on \
                         next tick"
                    );
                }
            }
        }
        ExtractTriplesBatchOutcome {
            abstractions,
            deferred_count,
            failed_count,
            last_error,
        }
    }

    /// Surface contradictions for the consolidation pass to flag
    /// for resolution. Two-stage: cheap pure-Rust rule filter
    /// (subject + predicate + temporal overlap) + LLM judge for
    /// pairs that survive. See [`contradiction::detect_contradiction`]
    /// for the algorithm + tests.
    pub async fn detect_contradiction(
        &self,
        a: &Triple,
        b: &Triple,
    ) -> Result<Option<Contradiction>> {
        contradiction::detect_contradiction(a, b, self.client.as_ref()).await
    }
}

#[cfg(test)]
mod from_env_tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global mutable state — serialize the tests
    // here so cargo test's per-binary parallel runner doesn't race
    // SOLO_CLUSTER_COSINE_THRESHOLD between cases. `unwrap_or_else
    // (PoisonError::into_inner)` keeps a panicking test from poisoning
    // the lock for the rest of the suite.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Drops every steward env var on Drop so a panicking test doesn't
    /// leak state into the next case (which would expect either "unset"
    /// or its own set value). Cleanup list MUST stay in sync with the
    /// const list at the top of the module.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: ENV_LOCK is held by the caller, so no other
            // thread is concurrently reading or writing these vars.
            for k in [
                ENV_CLUSTER_COSINE_THRESHOLD,
                ENV_CLUSTER_MIN_SIZE,
                ENV_CLUSTER_MAX_SIZE,
                ENV_ABSTRACTION_MAX_TOKENS,
                ENV_CONTRADICTION_CHECK_ENABLED,
            ] {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    /// Set ENV_CLUSTER_COSINE_THRESHOLD specifically. The other env
    /// vars have their own helpers below.
    fn set_env(value: &str) -> EnvGuard {
        set_named_env(ENV_CLUSTER_COSINE_THRESHOLD, value)
    }

    fn set_named_env(name: &str, value: &str) -> EnvGuard {
        // SAFETY: ENV_LOCK held; the only other accessor in-process is
        // our own `from_env`, which the caller invokes synchronously.
        unsafe { std::env::set_var(name, value) };
        EnvGuard
    }

    fn clear_env() -> EnvGuard {
        // SAFETY: same as set_env.
        for k in [
            ENV_CLUSTER_COSINE_THRESHOLD,
            ENV_CLUSTER_MIN_SIZE,
            ENV_CLUSTER_MAX_SIZE,
            ENV_ABSTRACTION_MAX_TOKENS,
            ENV_CONTRADICTION_CHECK_ENABLED,
        ] {
            unsafe { std::env::remove_var(k) };
        }
        EnvGuard
    }

    #[test]
    fn unset_env_yields_default_threshold() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = clear_env();
        let cfg = StewardConfig::from_env().expect("ok");
        assert_eq!(
            cfg.cluster_cosine_threshold,
            StewardConfig::default().cluster_cosine_threshold
        );
    }

    #[test]
    fn empty_or_whitespace_yields_default_threshold() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["", "   ", "\t"] {
            let _g = set_env(value);
            let cfg = StewardConfig::from_env().expect("ok");
            assert_eq!(
                cfg.cluster_cosine_threshold,
                StewardConfig::default().cluster_cosine_threshold,
                "unexpected override for empty value {value:?}"
            );
        }
    }

    #[test]
    fn valid_value_overrides_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_env("0.92");
        let cfg = StewardConfig::from_env().expect("0.92 is valid");
        assert!(
            (cfg.cluster_cosine_threshold - 0.92).abs() < 1e-6,
            "got {}",
            cfg.cluster_cosine_threshold
        );
    }

    #[test]
    fn boundary_one_is_accepted() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_env("1.0");
        let cfg = StewardConfig::from_env().expect("1.0 is valid");
        assert_eq!(cfg.cluster_cosine_threshold, 1.0);
    }

    #[test]
    fn unparseable_value_errors() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_env("not-a-number");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn zero_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_env("0.0");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn negative_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_env("-0.1");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn above_one_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_env("1.01");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn nan_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_env("NaN");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    // ---- ENV_CLUSTER_MIN_SIZE ----

    #[test]
    fn cluster_min_size_valid_overrides_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_CLUSTER_MIN_SIZE, "5");
        let cfg = StewardConfig::from_env().expect("5 is valid");
        assert_eq!(cfg.cluster_min_size, 5);
    }

    #[test]
    fn cluster_min_size_zero_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_CLUSTER_MIN_SIZE, "0");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn cluster_min_size_unparseable_errors() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_CLUSTER_MIN_SIZE, "not-a-number");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    // ---- ENV_CLUSTER_MAX_SIZE ----

    #[test]
    fn cluster_max_size_valid_overrides_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_CLUSTER_MAX_SIZE, "12");
        let cfg = StewardConfig::from_env().expect("12 is valid");
        assert_eq!(cfg.cluster_max_size, 12);
    }

    #[test]
    fn cluster_max_size_zero_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_CLUSTER_MAX_SIZE, "0");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn cluster_max_size_unparseable_errors() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_CLUSTER_MAX_SIZE, "not-a-number");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    // ---- ENV_ABSTRACTION_MAX_TOKENS ----

    #[test]
    fn abstraction_max_tokens_valid_overrides_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_ABSTRACTION_MAX_TOKENS, "1024");
        let cfg = StewardConfig::from_env().expect("1024 is valid");
        assert_eq!(cfg.abstraction_max_tokens, 1024);
    }

    #[test]
    fn abstraction_max_tokens_above_upper_bound_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_ABSTRACTION_MAX_TOKENS, "131072");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn abstraction_max_tokens_zero_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_ABSTRACTION_MAX_TOKENS, "0");
        let err = StewardConfig::from_env().unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    // ---- ENV_CONTRADICTION_CHECK_ENABLED ----

    #[test]
    fn contradiction_check_false_overrides_default_true() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_CONTRADICTION_CHECK_ENABLED, "false");
        let cfg = StewardConfig::from_env().expect("false is valid");
        assert!(!cfg.contradiction_check_enabled);
    }

    #[test]
    fn contradiction_check_accepts_case_insensitive() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["True", "TRUE", "tRuE"] {
            let _g = set_named_env(ENV_CONTRADICTION_CHECK_ENABLED, value);
            let cfg = StewardConfig::from_env().expect("case-insensitive true");
            assert!(cfg.contradiction_check_enabled, "got false for {value:?}");
        }
        for value in ["False", "FALSE", "fAlSe"] {
            let _g = set_named_env(ENV_CONTRADICTION_CHECK_ENABLED, value);
            let cfg = StewardConfig::from_env().expect("case-insensitive false");
            assert!(!cfg.contradiction_check_enabled, "got true for {value:?}");
        }
    }

    #[test]
    fn contradiction_check_rejects_non_bool_strings() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Numeric / yes-no / on-off shorthand intentionally NOT
        // accepted — strict bool spelling avoids ambiguity ("0" looks
        // boolean-like in many languages but `bool::from_str` rejects it).
        for value in ["1", "0", "yes", "no", "on", "off", "maybe"] {
            let _g = set_named_env(ENV_CONTRADICTION_CHECK_ENABLED, value);
            let err = StewardConfig::from_env().unwrap_err();
            assert!(
                matches!(err, Error::InvalidInput(_)),
                "expected error for {value:?}"
            );
        }
    }

    // ---- cross-field ----

    #[test]
    fn all_five_env_vars_set_simultaneously_take_effect() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g1 = set_named_env(ENV_CLUSTER_COSINE_THRESHOLD, "0.91");
        let _g2 = set_named_env(ENV_CLUSTER_MIN_SIZE, "5");
        let _g3 = set_named_env(ENV_CLUSTER_MAX_SIZE, "64");
        let _g4 = set_named_env(ENV_ABSTRACTION_MAX_TOKENS, "1024");
        let _g5 = set_named_env(ENV_CONTRADICTION_CHECK_ENABLED, "false");
        let cfg = StewardConfig::from_env().expect("all five valid together");
        assert!((cfg.cluster_cosine_threshold - 0.91).abs() < 1e-6);
        assert_eq!(cfg.cluster_min_size, 5);
        assert_eq!(cfg.cluster_max_size, 64);
        assert_eq!(cfg.abstraction_max_tokens, 1024);
        assert!(!cfg.contradiction_check_enabled);
    }

    // ----------------------------------------------------------------
    // v0.11.1 — from_settings_then_env layering
    // ----------------------------------------------------------------

    /// Both inputs `None` AND no env set → result equals
    /// `StewardConfig::default()`. Pins the backward-compat path: pre-
    /// v0.11.1 configs (no `[steward]` block) and no env vars must not
    /// change daemon behaviour.
    #[test]
    fn from_settings_then_env_no_overrides_yields_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = clear_env();
        let cfg = StewardConfig::from_settings_then_env(None, None)
            .expect("no inputs, no env, must succeed");
        let d = StewardConfig::default();
        assert_eq!(cfg.cluster_min_size, d.cluster_min_size);
        assert!((cfg.cluster_cosine_threshold - d.cluster_cosine_threshold).abs() < 1e-6);
        assert_eq!(cfg.abstraction_max_tokens, d.abstraction_max_tokens);
        assert_eq!(
            cfg.contradiction_check_enabled,
            d.contradiction_check_enabled
        );
    }

    /// TOML-supplied values flow through when env vars are unset.
    /// This is the v0.11.1 happy path: operator writes
    /// `[steward] cluster_min_size = 4` + `cluster_cosine_threshold = 0.7`
    /// and that's exactly what the daemon uses for clustering.
    #[test]
    fn from_settings_then_env_toml_values_take_effect_without_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = clear_env();
        let cfg =
            StewardConfig::from_settings_then_env(Some(4), Some(0.7)).expect("valid TOML inputs");
        assert_eq!(cfg.cluster_min_size, 4);
        assert!((cfg.cluster_cosine_threshold - 0.7).abs() < 1e-6);
    }

    /// Env vars beat TOML overrides — the operator's runtime escape
    /// hatch (set `SOLO_CLUSTER_*` in the shell to override the config
    /// file without editing it) must keep working in v0.11.1.
    #[test]
    fn from_settings_then_env_env_wins_over_toml() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g1 = set_named_env(ENV_CLUSTER_MIN_SIZE, "8");
        let _g2 = set_named_env(ENV_CLUSTER_COSINE_THRESHOLD, "0.95");
        // TOML asks for (3, 0.5); env asks for (8, 0.95). Env must win.
        let cfg = StewardConfig::from_settings_then_env(Some(3), Some(0.5))
            .expect("env wins over TOML, both valid");
        assert_eq!(cfg.cluster_min_size, 8, "env override beats TOML");
        assert!(
            (cfg.cluster_cosine_threshold - 0.95).abs() < 1e-6,
            "env override beats TOML"
        );
    }

    #[test]
    fn from_settings_then_env_default_valued_env_still_overrides_toml() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let default = StewardConfig::default();
        let _g1 = set_named_env(ENV_CLUSTER_MIN_SIZE, &default.cluster_min_size.to_string());
        let _g2 = set_named_env(
            ENV_CLUSTER_COSINE_THRESHOLD,
            &default.cluster_cosine_threshold.to_string(),
        );

        let cfg = StewardConfig::from_settings_then_env(
            Some(default.cluster_min_size + 3),
            Some((default.cluster_cosine_threshold + 0.2).min(1.0)),
        )
        .expect("default-valued env overrides are valid");

        assert_eq!(
            cfg.cluster_min_size, default.cluster_min_size,
            "env presence, not inequality from default, determines precedence"
        );
        assert!(
            (cfg.cluster_cosine_threshold - default.cluster_cosine_threshold).abs() < 1e-6,
            "env threshold equal to the code default must still override TOML"
        );
    }

    /// Partial layering: env sets ONLY `SOLO_CLUSTER_MIN_SIZE`; TOML
    /// supplies the cosine threshold. Resolution should pick env for
    /// min_size + TOML for the threshold (per-field precedence, not
    /// all-or-nothing).
    #[test]
    fn from_settings_then_env_partial_env_keeps_toml_for_untouched_field() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_named_env(ENV_CLUSTER_MIN_SIZE, "9");
        let cfg = StewardConfig::from_settings_then_env(None, Some(0.42))
            .expect("env on min_size, TOML on threshold");
        assert_eq!(cfg.cluster_min_size, 9, "env override applied");
        assert!(
            (cfg.cluster_cosine_threshold - 0.42).abs() < 1e-6,
            "env unset for threshold → TOML 0.42 survives"
        );
    }

    /// Invalid TOML values surface a typed error rather than silently
    /// falling back. Mirrors the env-var parser's strictness so
    /// operators see the same diagnostic regardless of which surface
    /// they used.
    #[test]
    fn from_settings_then_env_rejects_bad_toml_threshold() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = clear_env();
        // > 1.0 is out of range.
        let err = StewardConfig::from_settings_then_env(None, Some(1.5)).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        // Zero is out of range (must be > 0.0).
        let err = StewardConfig::from_settings_then_env(None, Some(0.0)).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        // NaN.
        let err = StewardConfig::from_settings_then_env(None, Some(f32::NAN)).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        // cluster_min_size = 0 is rejected.
        let err = StewardConfig::from_settings_then_env(Some(0), None).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }
}

#[cfg(test)]
mod has_llm_tests {
    use super::*;
    use crate::test_support::StubLlmClient;

    /// A `Steward` wrapping a default stub reports `has_llm() ==
    /// false` — the writer's contradiction sweep gates on this and
    /// must skip when only a stub is configured.
    #[test]
    fn has_llm_false_for_default_stub() {
        let stub = Arc::new(StubLlmClient::default_stub());
        let s = Steward::new(stub, StewardConfig::default());
        assert!(!s.has_llm());
    }

    /// A stub whose `pretend_real_llm(true)` flag is set reports
    /// `has_llm() == true`. Used by storage-layer integration tests
    /// that need the contradiction-sweep code path to run against
    /// canned responses.
    #[test]
    fn has_llm_true_when_stub_pretends() {
        let stub = Arc::new(StubLlmClient::default_stub().pretend_real_llm(true));
        let s = Steward::new(stub, StewardConfig::default());
        assert!(s.has_llm());
    }
}

// ----------------------------------------------------------------
// v0.10.1 (P4 audit m5): extract_triples_batch per-cluster timeout
// ----------------------------------------------------------------
#[cfg(test)]
mod extract_triples_batch_timeout_tests {
    use super::*;
    use crate::test_support::StubLlmClient;
    use async_trait::async_trait;
    use solo_core::{
        Confidence, Embedding, EmbeddingDtype, Episode, MemoryId, Message, Role, Tier,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A custom LlmClient stub that holds its `complete()` future for a
    /// configurable per-call duration before returning a parseable
    /// abstraction response. Used to force `tokio::time::timeout` to
    /// fire inside `extract_triples_batch`.
    ///
    /// Call N (0-indexed): sleeps `delays[N.min(delays.len()-1)]` ms.
    /// The last entry is reused for every subsequent call (so a single
    /// `vec![5_000]` makes EVERY call slow).
    struct SlowStub {
        delays_ms: Vec<u64>,
        calls: AtomicUsize,
    }

    impl SlowStub {
        fn new(delays_ms: Vec<u64>) -> Self {
            Self {
                delays_ms,
                calls: AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl LlmClient for SlowStub {
        fn name(&self) -> &str {
            "slow-stub"
        }
        async fn complete(&self, _messages: &[Message]) -> Result<Message> {
            let idx = self.calls.fetch_add(1, Ordering::Relaxed);
            let delay = self
                .delays_ms
                .get(idx)
                .or_else(|| self.delays_ms.last())
                .copied()
                .unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Ok(Message {
                role: Role::Assistant,
                content: r#"{"content":"ok","confidence":0.7,"triples":[]}"#.into(),
            })
        }
        fn is_real_llm(&self) -> bool {
            true
        }
    }

    /// Build a minimal cluster + one episode so `abstract_cluster`
    /// has shapeful input. The contents don't matter — the SlowStub
    /// ignores the prompt.
    fn mk_cluster_with_episode() -> (Cluster, Vec<Episode>) {
        let cluster_id = MemoryId::new();
        let memory_id = MemoryId::new();
        let episode = Episode {
            memory_id,
            ts_ms: 0,
            source_type: "user_message".into(),
            content: "hello".into(),
            encoding_context: Default::default(),
            provenance: None,
            confidence: Confidence::new(0.9).unwrap(),
            strength: 0.5,
            salience: 0.5,
            tier: Tier::Hot,
            source_id: None,
        };
        let centroid = Embedding {
            data: bytemuck::cast_slice(&[1.0f32, 0.0, 0.0, 0.0]).to_vec(),
            dim: 4,
            dtype: EmbeddingDtype::F32,
        };
        let cluster = Cluster {
            cluster_id,
            episode_ids: vec![memory_id],
            centroid: Some(centroid),
            coherence: 0.95,
        };
        (cluster, vec![episode])
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// m5 test 1 (the main pin): when one cluster's
    /// `abstract_cluster` hangs past the per-cluster timeout, the
    /// batch CONTINUES with the next cluster. The hung cluster is
    /// counted as deferred — not lumped in with errors.
    #[test]
    fn extract_triples_batch_continues_after_cluster_timeout() {
        let rt = rt();
        rt.block_on(async {
            // Cluster 1: fast (50ms). Cluster 2: hangs (3s — well past
            // the 200ms timeout). Cluster 3: fast (50ms).
            let stub = Arc::new(SlowStub::new(vec![50, 3_000, 50]));
            let steward = Steward::new(stub.clone(), StewardConfig::default());

            let inputs = vec![
                mk_cluster_with_episode(),
                mk_cluster_with_episode(),
                mk_cluster_with_episode(),
            ];

            let outcome = steward
                .extract_triples_batch(inputs, Duration::from_millis(200))
                .await;

            assert_eq!(
                outcome.abstractions.len(),
                2,
                "clusters 1 + 3 must succeed while cluster 2 times out"
            );
            assert_eq!(
                outcome.deferred_count, 1,
                "cluster 2 must be counted as deferred (timeout), not failed"
            );
            // Verify all three calls were attempted (the loop didn't
            // bail out after the timeout).
            assert_eq!(
                stub.call_count(),
                3,
                "every cluster's abstract_cluster must be attempted; \
                 a timeout on one MUST NOT abort the batch"
            );
        });
    }

    /// m5 test 2: happy path — all clusters complete in time → zero
    /// deferred. Pins that the timeout path doesn't introduce
    /// spurious deferrals.
    #[test]
    fn extract_triples_batch_returns_succeeded_count() {
        let rt = rt();
        rt.block_on(async {
            let stub = Arc::new(SlowStub::new(vec![10]));
            let steward = Steward::new(stub, StewardConfig::default());

            let inputs = vec![
                mk_cluster_with_episode(),
                mk_cluster_with_episode(),
                mk_cluster_with_episode(),
            ];

            let outcome = steward
                .extract_triples_batch(inputs, Duration::from_secs(5))
                .await;

            assert_eq!(outcome.abstractions.len(), 3);
            assert_eq!(
                outcome.deferred_count, 0,
                "no clusters exceeded the timeout"
            );
        });
    }

    /// m5 test 3: clusters that return Err (NOT timeouts) are
    /// distinct from deferred. The deferred counter is for timeout
    /// hangs specifically; a per-cluster Err is not counted toward
    /// `deferred_count` (it's the implicit `cluster_count -
    /// abstractions_built - deferred_count` half).
    ///
    /// We use a real `StubLlmClient` with a canned response that
    /// fails to parse — `abstract_cluster` returns an Err. The
    /// `SlowStub` doesn't have a "fail" mode without complicating it.
    #[test]
    fn extract_triples_batch_failure_distinct_from_timeout() {
        let rt = rt();
        rt.block_on(async {
            // Stub returns non-JSON for both the first abstraction
            // response and the repair attempt, so abstract_cluster
            // returns Err. Note we still need `pretend_real_llm(true)`
            // so `has_llm()` returns true.
            let stub = Arc::new(
                StubLlmClient::with_canned("garbage-stub", "NOT-JSON").pretend_real_llm(true),
            );
            stub.push_canned("STILL-NOT-JSON");
            let steward = Steward::new(stub, StewardConfig::default());

            let inputs = vec![mk_cluster_with_episode()];
            let outcome = steward
                .extract_triples_batch(inputs, Duration::from_secs(5))
                .await;

            assert_eq!(
                outcome.abstractions.len(),
                0,
                "the parse failure means no abstraction lands"
            );
            assert_eq!(
                outcome.deferred_count, 0,
                "an Err return is NOT a deferral — only timeout-elapsed \
                 counts as deferred. failed clusters are computed by the \
                 caller as cluster_count - abstractions_built - deferred"
            );
            assert_eq!(outcome.failed_count, 1);
            assert!(
                outcome
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("after repair")),
                "last error should explain the repair failure: {:?}",
                outcome.last_error
            );
        });
    }

    /// m5 test 4: zero-Duration disables the timeout. Verifies the
    /// operator escape hatch (`[triples] cluster_timeout_secs = 0`
    /// → `Duration::ZERO`). The slow cluster STILL completes
    /// because we never wrapped it.
    #[test]
    fn extract_triples_batch_zero_timeout_disables_per_cluster_timeout() {
        let rt = rt();
        rt.block_on(async {
            // 100ms delay; tiny but enough to ensure tokio::timeout
            // (if active) wouldn't fire on a tight bound.
            let stub = Arc::new(SlowStub::new(vec![100]));
            let steward = Steward::new(stub, StewardConfig::default());
            let inputs = vec![mk_cluster_with_episode()];
            let outcome = steward.extract_triples_batch(inputs, Duration::ZERO).await;
            assert_eq!(
                outcome.abstractions.len(),
                1,
                "zero Duration disables the timeout; the cluster must \
                 succeed even with a delay"
            );
            assert_eq!(outcome.deferred_count, 0);
        });
    }

    /// m5 test 5: when the steward has no LLM (`has_llm() == false`),
    /// the batch short-circuits with an empty outcome regardless of
    /// the per-cluster timeout. Pins the fast path.
    #[test]
    fn extract_triples_batch_no_llm_returns_empty_outcome() {
        let rt = rt();
        rt.block_on(async {
            // default stub: is_real_llm == false → has_llm == false.
            let stub = Arc::new(StubLlmClient::default_stub());
            let steward = Steward::new(stub, StewardConfig::default());
            let inputs = vec![mk_cluster_with_episode()];
            let outcome = steward
                .extract_triples_batch(inputs, Duration::from_secs(5))
                .await;
            assert!(outcome.abstractions.is_empty());
            assert_eq!(outcome.deferred_count, 0);
        });
    }

    /// m5 test 6: the passed Duration is actually wired through.
    /// Distinguished from test 1 (which already pins the deferral)
    /// by using a generous timeout that should comfortably swallow
    /// the call — if the wiring is broken (e.g. we accidentally
    /// hardcoded a different Duration), the slow call would deferr
    /// inappropriately.
    ///
    /// Pin both directions: 50ms call with 1s timeout succeeds; the
    /// SAME 50ms call with a 10ms timeout defers.
    #[test]
    fn extract_triples_batch_uses_passed_timeout() {
        let rt = rt();
        rt.block_on(async {
            // 50ms call, generous 1s timeout → success.
            let stub = Arc::new(SlowStub::new(vec![50]));
            let steward = Steward::new(stub, StewardConfig::default());
            let outcome = steward
                .extract_triples_batch(vec![mk_cluster_with_episode()], Duration::from_secs(1))
                .await;
            assert_eq!(outcome.abstractions.len(), 1);
            assert_eq!(outcome.deferred_count, 0);

            // SAME 50ms call shape, tight 10ms timeout → deferral.
            let stub2 = Arc::new(SlowStub::new(vec![50]));
            let steward2 = Steward::new(stub2, StewardConfig::default());
            let outcome2 = steward2
                .extract_triples_batch(vec![mk_cluster_with_episode()], Duration::from_millis(10))
                .await;
            assert_eq!(outcome2.abstractions.len(), 0);
            assert_eq!(outcome2.deferred_count, 1);
        });
    }
}
