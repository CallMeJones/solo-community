// SPDX-License-Identifier: Apache-2.0

//! `StewardFactory` trait — abstracts how a per-tenant `Arc<Steward>` is
//! built at registry-open time.
//!
//! Introduced in v0.9.0 P0c per the locked plan
//! (`docs/dev-log/0098-v0.9.0-implementation-plan.md` §6 "Steward
//! placement" / MAJOR 1 resolution). The trait shape was refined in the
//! v0.9.0 P2 revision (F2 cleanup) to drop the placeholder `MockPeer`
//! parameter that previously leaked into the public surface; see "Why
//! `build()` takes no peer" below.
//!
//! ## Why the trait exists
//!
//! Pre-v0.9.0, the daemon built one `Arc<Steward>` at startup and threaded
//! it through every `LibraryHandle::open` via
//! `LibraryOpenParams.steward`. That worked for static backends
//! (Anthropic / OpenAI / Ollama / `None`) where the `LlmClient` instance
//! is available before the first MCP session attaches.
//!
//! v0.9.0 adds an MCP-sampling backend whose `LlmClient` (the
//! `SamplingLlmClient`, P2) requires a `Peer<RoleServer>` only available
//! AFTER an MCP `initialize` handshake returns. We can't pre-build the
//! Steward at daemon startup because the peer doesn't exist yet.
//!
//! The factory trait expresses "how to build a Steward at registry-open
//! time, if at all". Two production variants:
//!
//!   * **Static** — for Anthropic / OpenAI / Ollama / None. The
//!     `LlmClient` is already known; the wrapped `Arc<Steward>` is
//!     pre-built once at registry-open time and `build()` returns a
//!     clone unconditionally. `requires_peer() = false`.
//!
//!   * **MCP sampling** — for `LlmConfig::McpSampling`. `build()` is a
//!     no-op that returns `Ok(None)` — the real sampling-backed Steward
//!     is constructed by
//!     [`crate::library::LibraryHandle::steward_slot`] consumers (in
//!     practice, `solo_api::mcp::SoloMcpServer::populate_sampling_steward`
//!     at MCP `initialize` time) and written into the slot directly.
//!     `requires_peer() = true`.
//!
//! ## Why `build()` takes no peer
//!
//! Earlier P0c drafts gave the trait a `peer: Option<MockPeer>`
//! parameter, intending v0.9.0 P2 to replace `MockPeer` with the real
//! `rmcp::Peer<RoleServer>` shape. That plan was abandoned in P2: the
//! real peer lives in `solo-api` (with the rmcp transitive), and
//! plumbing it across the `solo-api → solo-storage` boundary would
//! either leak rmcp into the storage layer or require a `Box<dyn Any>`
//! erased handle whose unsafe-by-construction down-cast wins nothing.
//!
//! The cleaner resolution — landed in P2 — is to bypass the factory on
//! the sampling path entirely. `solo_api::mcp::SoloMcpServer::
//! populate_sampling_steward` constructs the sampling-backed
//! `Arc<Steward>` directly from
//! `solo_api::llm::build_sampling_steward(peer, ..)` and writes it into
//! `LibraryHandle::steward_slot()`. The factory's `build()` then never
//! needs a peer argument: it's only ever called at registry-open time
//! (before any MCP session can have attached), so for the sampling
//! backend it correctly returns `Ok(None)`.
//!
//! ## Slot semantics
//!
//! `LibraryHandle` carries an `Arc<RwLock<Option<Arc<Steward>>>>`
//! "steward slot" that the factory populates at registry-open time. For
//! static backends, the slot is `Some(steward)` immediately — the hot
//! path's `slot.read()` always observes a populated value and pays no
//! lock-contention cost in practice (the `RwLock` is uncontested).
//! For the MCP-sampling backend, the slot stays `None` until
//! `SoloMcpServer::initialize` (P2) calls `populate_sampling_steward`
//! and writes the result in.
//!
//! Consumers should:
//!
//! 1. `slot.read().clone()` to get an `Option<Arc<Steward>>` snapshot.
//! 2. If `Some(steward)`, use it.
//! 3. If `None`, fall through to the "no LLM available" path
//!    (cluster persists, abstraction skipped — same as v0.8.x when
//!    `LlmConfig` was `None`).
//!
//! P0c provided the trait + impls + slot wiring. P2 ships the
//! late-population MCP-initialize hook. P4 will refactor the
//! writer-actor to read from the slot per command instead of capturing
//! `Arc<Steward>` at spawn time, which is what activates the
//! sampling-backed Steward end-to-end (until then it is plumbed but
//! inert; see dev log 0101's audit-response section for the full
//! context).

use std::sync::Arc;

use solo_core::Result;

/// Builds (or surfaces a pre-built) `Arc<Steward>` for a tenant at
/// registry-open time.
///
/// `Send + Sync` so the factory can live inside `Arc<dyn StewardFactory>`
/// in the registry; the trait object is cloned-by-Arc into every
/// `LibraryHandle::open` call.
pub trait StewardFactory: Send + Sync {
    /// Construct (or surface) the Steward for this tenant.
    ///
    /// * **Static backends** (`requires_peer() == false`) return a
    ///   pre-built `Some(Arc<Steward>)`.
    /// * **MCP-sampling backend** (`requires_peer() == true`) returns
    ///   `Ok(None)` — the real sampling-backed Steward is built outside
    ///   the factory by `solo_api::mcp::SoloMcpServer::
    ///   populate_sampling_steward` and written into
    ///   `LibraryHandle::steward_slot()` directly (see module docs).
    fn build(&self) -> Result<Option<Arc<solo_steward::Steward>>>;

    /// True iff this factory's Steward depends on an MCP peer. Static
    /// backends return `false` (the registry can eager-populate the
    /// slot at open time); MCP-sampling returns `true` (eager-populate
    /// is a no-op; lazy population happens at MCP `initialize`).
    fn requires_peer(&self) -> bool;
}

/// Static-backend factory: wraps a pre-built `Arc<Steward>` and surfaces
/// it from `build()` unconditionally.
///
/// Used by `LlmSettings::Anthropic`, `LlmSettings::Openai`,
/// `LlmSettings::Ollama`, and `LlmSettings::None` (the latter wrapping
/// a Steward backed by `NoopLlmClient`). Zero hot-path lock-contention
/// cost — the wrapped `Arc<Steward>` is cloned out of the factory in
/// `O(1)`.
pub struct StaticStewardFactory {
    steward: Arc<solo_steward::Steward>,
}

impl StaticStewardFactory {
    /// Wrap an already-built Steward. Production callers build the
    /// Steward from the chosen `LlmClient` (Anthropic, OpenAI, Ollama,
    /// or `NoopLlmClient` for the `None` mode) at registry-open time,
    /// then hand the `Arc<Steward>` to this constructor.
    pub fn new(steward: Arc<solo_steward::Steward>) -> Self {
        Self { steward }
    }
}

impl StewardFactory for StaticStewardFactory {
    fn build(&self) -> Result<Option<Arc<solo_steward::Steward>>> {
        Ok(Some(self.steward.clone()))
    }

    fn requires_peer(&self) -> bool {
        false
    }
}

/// MCP-sampling-backend factory: a no-op `build()` that always returns
/// `Ok(None)`.
///
/// The real sampling-backed Steward needs `rmcp::Peer<RoleServer>`,
/// which lives in `solo-api`. `solo-storage` doesn't depend on
/// `solo-api`, so the storage-layer factory cannot construct the
/// Steward. Instead,
/// [`solo_api::mcp::SoloMcpServer::populate_sampling_steward`] is the
/// canonical builder: at MCP `initialize` time it constructs the
/// `SamplingLlmClient`-backed `Arc<Steward>` via
/// `solo_api::llm::build_sampling_steward` and writes it directly into
/// `LibraryHandle::steward_slot()`.
///
/// This factory exists only so the registry-open path has a uniform
/// `Arc<dyn StewardFactory>` API. `requires_peer() = true` short-
/// circuits the eager-populate code path; the slot stays `None` until
/// `initialize` writes the peer-bound Steward in.
pub struct McpSamplingStewardFactory;

impl McpSamplingStewardFactory {
    pub fn new() -> Self {
        Self
    }
}

impl Default for McpSamplingStewardFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl StewardFactory for McpSamplingStewardFactory {
    fn build(&self) -> Result<Option<Arc<solo_steward::Steward>>> {
        // No-op by design: the sampling-backed Steward is constructed
        // by `solo_api::mcp::SoloMcpServer::populate_sampling_steward`
        // and written directly into `LibraryHandle::steward_slot()`.
        // See the module docs and the `McpSamplingStewardFactory`
        // doc-comment for the full rationale.
        Ok(None)
    }

    fn requires_peer(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};

    // ----------------------------------------------------------------
    // v0.9.0 P0c — StewardFactory trait scaffold tests
    // ----------------------------------------------------------------

    fn fixture_static_factory() -> StaticStewardFactory {
        let client: Arc<dyn solo_core::LlmClient> = Arc::new(StubLlmClient::default_stub());
        let steward = Arc::new(Steward::new(client, StewardConfig::default()));
        StaticStewardFactory::new(steward)
    }

    /// Static factory's `build()` surfaces the pre-built Steward.
    #[test]
    fn static_factory_build_returns_pre_built_steward() {
        let factory = fixture_static_factory();
        let built = factory.build().expect("build");
        assert!(built.is_some(), "static factory must surface a Steward");
    }

    /// Multiple `build()` calls return clones of the same wrapped
    /// `Arc<Steward>` — pointer-equality holds across calls.
    #[test]
    fn static_factory_build_is_stable_across_calls() {
        let factory = fixture_static_factory();
        let a = factory.build().expect("build a").unwrap();
        let b = factory.build().expect("build b").unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "static factory must return the same Arc across build calls"
        );
    }

    #[test]
    fn static_factory_does_not_require_peer() {
        let factory = fixture_static_factory();
        assert!(!factory.requires_peer());
    }

    /// MCP-sampling factory's `build()` is a no-op that returns
    /// `Ok(None)`. The real sampling-backed Steward is built outside
    /// the factory at MCP `initialize` time (P2: `solo_api::mcp::
    /// SoloMcpServer::populate_sampling_steward`).
    #[test]
    fn mcp_sampling_factory_build_returns_none() {
        let factory = McpSamplingStewardFactory::new();
        let built = factory.build().expect("build");
        assert!(
            built.is_none(),
            "sampling factory's build() must yield None; the real Steward \
             is constructed in solo-api at MCP initialize time"
        );
    }

    #[test]
    fn mcp_sampling_factory_requires_peer() {
        let factory = McpSamplingStewardFactory::new();
        assert!(factory.requires_peer());
    }

    /// Trait-object dyn dispatch works: factories drop into
    /// `Arc<dyn StewardFactory>` without trait-object issues. This is
    /// the shape `RegistryDeps` holds; this test verifies the trait is
    /// object-safe.
    #[test]
    fn factories_are_trait_object_safe() {
        let static_factory: Arc<dyn StewardFactory> = Arc::new(fixture_static_factory());
        let sampling_factory: Arc<dyn StewardFactory> = Arc::new(McpSamplingStewardFactory::new());

        // Cross-trait dispatch + build:
        let s = static_factory.build().expect("static build");
        assert!(s.is_some());
        let m = sampling_factory.build().expect("sampling build");
        assert!(m.is_none());

        // `requires_peer` dispatches per-impl correctly.
        assert!(!static_factory.requires_peer());
        assert!(sampling_factory.requires_peer());
    }
}
