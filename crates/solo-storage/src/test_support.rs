// SPDX-License-Identifier: Apache-2.0

//! Test fixtures shared across `writer` and `reader` test modules.
//!
//! Lives behind `#[cfg(any(test, feature = "test-support"))]` so it never
//! compiles into release builds. The `StubVectorIndex` is a minimal stand-in
//! for the real HNSW that lands in commit 1.3 — it records every `add` and
//! `remove` call so tests can assert ordering and counts.

#![cfg(any(test, feature = "test-support"))]

use parking_lot::Mutex;
use rusqlite::Connection;
use solo_core::{
    Confidence, Embedding, EmbeddingDtype, EncodingContext, Episode, MemoryId, Result, Tier,
    VectorIndex,
};
use std::path::Path;

/// Open a fresh on-disk SQLite database, run the migrations, return the
/// connection paired with a `tempfile::TempDir` whose lifetime tests must
/// keep alive. Tests use the returned `Connection` directly OR re-open the
/// file by path.
pub fn open_test_db() -> (Connection, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("test.db");
    let mut conn = Connection::open(&path).expect("open db");
    conn.execute_batch(
        "PRAGMA journal_mode = wal;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )
    .expect("startup pragmas");
    crate::migration::run_migrations(&mut conn).expect("migrations");
    (conn, tmp)
}

/// Open a fresh database file without consuming the path's TempDir.
pub fn open_test_db_at(path: &Path) -> Connection {
    let mut conn = Connection::open(path).expect("open db");
    conn.execute_batch(
        "PRAGMA journal_mode = wal;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )
    .expect("startup pragmas");
    crate::migration::run_migrations(&mut conn).expect("migrations");
    conn
}

/// Build a minimal valid `Episode` for tests. Each call generates a fresh
/// `MemoryId` so the sequence is unique even when fixture content matches.
pub fn fixture_episode(content: &str) -> Episode {
    Episode {
        memory_id: MemoryId::new(),
        ts_ms: chrono::Utc::now().timestamp_millis(),
        source_type: "user_message".into(),
        source_id: None,
        content: content.into(),
        encoding_context: EncodingContext::default(),
        provenance: None,
        confidence: Confidence::new(0.9).unwrap(),
        strength: 0.5,
        salience: 0.5,
        tier: Tier::Hot,
    }
}

/// Build a disabled `RedactionRegistry` for test `WriterActor`
/// constructions. Always wrapped in an `Arc` so tests can pass it
/// directly into the actor's `redactor` field without further dance.
/// v0.8.0 P5.
pub fn disabled_test_redactor() -> std::sync::Arc<crate::redaction::RedactionRegistry> {
    std::sync::Arc::new(
        crate::redaction::RedactionRegistry::from_config(
            &crate::config::RedactionConfig::default(),
        )
        .expect("disabled default RedactionConfig must build cleanly"),
    )
}

/// Build a fully-enabled `RedactionRegistry` for redaction-specific
/// writer tests. Mirrors `RedactionRegistry::builtin()` but wrapped in
/// an `Arc` so it can be dropped straight into a `WriterActor`
/// struct literal.
pub fn enabled_test_redactor() -> std::sync::Arc<crate::redaction::RedactionRegistry> {
    std::sync::Arc::new(crate::redaction::RedactionRegistry::builtin())
}

/// v0.9.1 P1 Fix 2 — process-wide lock for tests that mutate
/// LLM-related env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, ...).
///
/// **Problem this solves**: env vars are process-global mutable state.
/// `cargo test` runs every test in a crate's test binary inside the
/// SAME process with multiple threads. If
/// `solo_storage::init::tests::init_writes_llm_anthropic_when_env_key_present`
/// uses a module-local `Mutex` but
/// `solo_storage::llm::anthropic::tests::build_from_env_returns_none_when_key_missing`
/// uses its own (or none), the two modules can race on the same env var
/// during a parallel test run — the `init::tests` lock doesn't protect
/// against `llm::anthropic::tests` calling `remove_var` mid-test.
///
/// **Solution**: a single workspace-shared `Mutex<()>` here, lifted
/// from any single module. Both `init::tests` and
/// `llm::anthropic::tests` (plus any future env-mutating test in
/// `solo-storage`) lock this same mutex before touching env state.
///
/// **Poison handling**: poisoning is recovered via
/// `unwrap_or_else(|p| p.into_inner())` at call sites so one panicking
/// test doesn't sink the rest of the suite.
///
/// **Scope**: per-crate. Other crates that mutate the same env vars
/// (e.g. `solo-cli::commands::common`) run in their OWN test binary
/// (separate OS process), so this mutex doesn't need to cross crates.
/// Within `solo-storage`'s test binary, this is the single source of
/// truth.
pub static LLM_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Build an FP32 embedding of the given dim, filled with zeros. Sufficient
/// for tests that exercise SQL persistence + HNSW dispatch but don't care
/// about vector content.
pub fn fixture_embedding(dim: usize) -> Embedding {
    Embedding {
        dtype: EmbeddingDtype::F32,
        dim,
        data: vec![0u8; dim * 4],
    }
}

/// Test-only `VectorIndex` that records every call. Behaves correctly under
/// `Arc<dyn VectorIndex + Send + Sync>` — interior mutability via
/// `parking_lot::Mutex`. NOT a real ANN index; `search` returns whatever was
/// added in insertion order.
pub struct StubVectorIndex {
    state: Mutex<StubState>,
    dim: usize,
    /// If set, `add` sleeps for this duration before returning. Used by the
    /// "slow writer doesn't deadlock" property test.
    add_sleep: Mutex<Option<std::time::Duration>>,
    /// If true, `save` returns Err. Used by the "snapshot failure
    /// continuation" property test.
    save_fails: Mutex<bool>,
}

struct StubState {
    entries: Vec<(i64, Vec<f32>)>,
    add_calls: usize,
    remove_calls: usize,
    save_calls: usize,
}

impl StubVectorIndex {
    pub fn new(dim: usize) -> Self {
        Self {
            state: Mutex::new(StubState {
                entries: Vec::new(),
                add_calls: 0,
                remove_calls: 0,
                save_calls: 0,
            }),
            dim,
            add_sleep: Mutex::new(None),
            save_fails: Mutex::new(false),
        }
    }

    pub fn add_count(&self) -> usize {
        self.state.lock().add_calls
    }

    pub fn remove_count(&self) -> usize {
        self.state.lock().remove_calls
    }

    pub fn save_count(&self) -> usize {
        self.state.lock().save_calls
    }

    pub fn last_added(&self) -> Option<(i64, Vec<f32>)> {
        self.state.lock().entries.last().cloned()
    }

    pub fn entries(&self) -> Vec<(i64, Vec<f32>)> {
        self.state.lock().entries.clone()
    }

    /// Make every `add` block for `dur`. Useful for testing backpressure
    /// and channel-full scenarios.
    pub fn set_add_sleep(&self, dur: Option<std::time::Duration>) {
        *self.add_sleep.lock() = dur;
    }

    /// Make every `save` return Err. Useful for testing snapshot-failure
    /// continuation per ADR-0003 §"Snapshot save failure handling".
    pub fn set_save_fails(&self, fail: bool) {
        *self.save_fails.lock() = fail;
    }
}

impl VectorIndex for StubVectorIndex {
    fn add(&self, rowid: i64, embedding: &[f32]) -> Result<()> {
        // Sleep BEFORE taking the state lock so concurrent adds can pile
        // up in flight (which is what the channel-full test exercises).
        if let Some(dur) = *self.add_sleep.lock() {
            std::thread::sleep(dur);
        }
        let mut s = self.state.lock();
        s.add_calls += 1;
        s.entries.push((rowid, embedding.to_vec()));
        Ok(())
    }

    fn remove(&self, rowid: i64) -> Result<()> {
        let mut s = self.state.lock();
        s.remove_calls += 1;
        s.entries.retain(|(r, _)| *r != rowid);
        Ok(())
    }

    fn search(&self, _query: &[f32], k: usize) -> Result<Vec<(i64, f32)>> {
        let s = self.state.lock();
        Ok(s.entries.iter().take(k).map(|(r, _)| (*r, 0.0)).collect())
    }

    fn save(&self, _path: &std::path::Path) -> Result<()> {
        self.state.lock().save_calls += 1;
        if *self.save_fails.lock() {
            return Err(solo_core::Error::vector_index(
                "stub configured to fail save",
            ));
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.state.lock().entries.len()
    }

    fn dim(&self) -> usize {
        self.dim
    }
}
