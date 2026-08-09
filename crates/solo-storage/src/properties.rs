// SPDX-License-Identifier: Apache-2.0

//! Property tests that exercise multi-component invariants from ADR-0003
//! §"Final consolidated action items" #14–#27.
//!
//! Tests in this module cross writer + reader + recovery + snapshot
//! boundaries and tend to be slower / heavier than the per-module unit
//! tests. They live separately so the standard `cargo test` loop stays
//! fast — each is `#[test]` (not `#[ignore]`) but they're meant to be
//! treated as a smoke-level integration suite.
//!
//! ### Items NOT covered here (require process-spawning)
//!
//! - #9  kill -9 between SQL commit and HNSW write (needs a separate
//!   subprocess we can SIGKILL mid-flight).
//! - #10 panic inside writer dispatch (needs a panic-aware harness).
//! - #15 shutdown-timeout (needs a hung shutdown to bound).
//!
//! Those land in a future "process-level integration tests" pass.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;
use solo_core::{Embedding, EmbeddingDtype, Result, VectorIndex};

use crate::recovery::replay_pending_index;
use crate::test_support::{
    StubVectorIndex, fixture_embedding, fixture_episode, open_test_db, open_test_db_at,
};
use crate::writer::{WriterActor, WriterSpawn};

fn rt_multi(threads: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .unwrap()
}

/// ADR-0003 §"Final consolidated action items" #18: pre-populate
/// `pending_index` with N rows; daemon startup completes within the
/// configured budget, all rows drained, HNSW count matches.
///
/// We use 10_000 rows. On the developer machine this completes in well
/// under a second against the StubVectorIndex; the real-HNSW figure is
/// dominated by hnsw_rs's ~1 ms per-insert cost (~10 sec at this scale,
/// still under budget).
///
/// **v0.11.1 dev-vs-CI budget split**: the budget reads from
/// [`PERF_BUDGET_ENV`] at test time. When unset (default — interactive
/// `cargo test` on a dev workstation), the budget is
/// [`PERF_BUDGET_DEV_SECS`] (30s — restores ADR-0003's developer-target
/// ceiling). When `SOLO_PERF_CI_BUDGET=1` (or any non-empty value), the
/// budget bumps to [`PERF_BUDGET_CI_SECS`] (60s) to absorb GitHub-hosted
/// runner variance. Release workflows may also set the bounded
/// `SOLO_PERF_CI_BUDGET_SECONDS` override for a demonstrably slower host.
/// This brings back the dev-target
/// regression-detection signal lost when commit `38b9d3e` blanket-bumped
/// the budget to 60s — a developer-machine 2× slowdown will now fail
/// locally instead of being absorbed by the CI-sized budget.
#[test]
fn ten_thousand_pending_rows_replay_within_budget() {
    let (mut conn, _tmp) = open_test_db();
    // Insert N episodes + their pending_index rows.
    let n = 10_000usize;
    let dim = 4usize;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let tx = conn.transaction().unwrap();
    for i in 0..n {
        let ep = fixture_episode(&format!("p{i}"));
        tx.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, source_id, content,
                encoding_context_json, provenance_json, confidence,
                strength, salience, tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                ep.memory_id.to_string(),
                ep.ts_ms,
                ep.source_type,
                ep.source_id,
                ep.content,
                "{}",
                Option::<String>::None,
                ep.confidence.0,
                0.5f32,
                0.5f32,
                "hot",
                now_ms,
                now_ms,
            ],
        )
        .unwrap();
        let zeros = vec![0u8; dim * 4];
        tx.execute(
            "INSERT INTO pending_index (memory_id, embedding, embedding_dim, enqueued_at)
             VALUES (?, ?, ?, ?)",
            params![ep.memory_id.to_string(), &zeros[..], dim as i64, 0i64],
        )
        .unwrap();
    }
    tx.commit().unwrap();

    let stub = StubVectorIndex::new(dim);
    let started = std::time::Instant::now();
    let report = replay_pending_index(&mut conn, &stub).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(report.rows_seen, n);
    assert_eq!(report.rows_replayed, n);
    assert_eq!(report.rows_failed, 0);
    assert_eq!(stub.add_count(), n);
    // pending_index is fully drained.
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM pending_index", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 0);
    // v0.11.1: per-test perf budget split. On developer hardware the
    // actual cost is dominated by 10k SQL DELETEs (~1 ms each on Windows
    // + SQLite WAL = a few seconds); per-row HNSW.add against the stub
    // is ~50 ns and lost in the noise. Real-HNSW replay is dominated
    // by hnsw_rs's ~1 ms inserts (still well under the CI 60s budget
    // for 10k rows).
    //
    // Three budget tiers:
    //   - 30s default (interactive `cargo test` on a workstation) —
    //     restores ADR-0003's original developer-hardware target.
    //     Observed dev-machine baselines: ~4s on a fast Linux box,
    //     ~15-17s on a busier Windows box; both well under 30s. A
    //     real regression — say, an O(n²) accidentally introduced in
    //     the replay path — pushes 10k rows well above 30s and the
    //     test fails locally. This is the regression-detection signal
    //     lost when commit `38b9d3e` blanket-bumped 30s → 60s for
    //     v0.10.2.
    //   - 60s when `SOLO_PERF_CI_BUDGET` is set for normal CI runs. This
    //     is the post-`38b9d3e` ceiling and covers GitHub-
    //     hosted runner variance that already burned v0.10.2 (run
    //     26120153333 attempt 1 with the prior 30s budget — see dev
    //     log 0131).
    //   - A bounded 60-300s override for unusually slow release runners;
    //     the Windows release workflow uses 120s after observed runner
    //     variance exceeded 60s without a code regression.
    //
    // The 30s developer ceiling remains the primary regression signal;
    // larger budgets are explicitly CI-only and capped.
    let budget = perf_budget();
    assert!(
        elapsed < budget,
        "10k pending replay took {elapsed:?} (budget {budget:?}; \
         set SOLO_PERF_CI_BUDGET=1 and, if needed, a bounded \
         SOLO_PERF_CI_BUDGET_SECONDS override on slow CI runners)"
    );
}

/// Env var name read by [`perf_budget`]. Any non-empty value selects
/// the CI-sized budget; unset / empty selects the developer-target one.
///
/// Release workflows set this on their `cargo test` step.
/// Local `cargo test` runs leave it unset so a 10× regression on dev
/// hardware fails immediately instead of being absorbed by the
/// CI-sized budget.
const PERF_BUDGET_ENV: &str = "SOLO_PERF_CI_BUDGET";

/// Optional numeric override for unusually slow CI hosts. This is honored
/// only when [`PERF_BUDGET_ENV`] is also enabled and is deliberately bounded
/// so a typo cannot silently disable the regression check.
const PERF_BUDGET_SECONDS_ENV: &str = "SOLO_PERF_CI_BUDGET_SECONDS";

/// Budget when [`PERF_BUDGET_ENV`] is unset. Restores ADR-0003's
/// original developer-hardware target (which the v0.10.2 incident
/// bump to 60s eroded). A real 10× regression in the replay path
/// trips this; CI runner noise lives under the larger
/// [`PERF_BUDGET_CI_SECS`] budget instead.
const PERF_BUDGET_DEV_SECS: u64 = 30;

/// Budget when [`PERF_BUDGET_ENV`] is set (any non-empty value). Same
/// 60s ceiling commit `38b9d3e` bumped to; absorbs GitHub-hosted
/// runner variance (see dev log 0131).
const PERF_BUDGET_CI_SECS: u64 = 60;

const PERF_BUDGET_OVERRIDE_MIN_SECS: u64 = PERF_BUDGET_CI_SECS;
const PERF_BUDGET_OVERRIDE_MAX_SECS: u64 = 300;

/// Resolve the active perf budget. Read on each call (rather than
/// cached) so tests can set/unset the env var in-process if needed in
/// a future tweak.
fn perf_budget() -> Duration {
    let ci_enabled = std::env::var(PERF_BUDGET_ENV).is_ok_and(|value| !value.trim().is_empty());
    let seconds_override = std::env::var(PERF_BUDGET_SECONDS_ENV).ok();
    resolve_perf_budget(ci_enabled, seconds_override.as_deref())
}

fn resolve_perf_budget(ci_enabled: bool, seconds_override: Option<&str>) -> Duration {
    let seconds_override = seconds_override
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(value) = seconds_override {
        assert!(
            ci_enabled,
            "{PERF_BUDGET_SECONDS_ENV} requires {PERF_BUDGET_ENV}"
        );
        let seconds = value.parse::<u64>().unwrap_or_else(|_| {
            panic!("{PERF_BUDGET_SECONDS_ENV} must be an integer number of seconds")
        });
        assert!(
            (PERF_BUDGET_OVERRIDE_MIN_SECS..=PERF_BUDGET_OVERRIDE_MAX_SECS).contains(&seconds),
            "{PERF_BUDGET_SECONDS_ENV} must be between \
             {PERF_BUDGET_OVERRIDE_MIN_SECS} and {PERF_BUDGET_OVERRIDE_MAX_SECS}"
        );
        return Duration::from_secs(seconds);
    }

    Duration::from_secs(if ci_enabled {
        PERF_BUDGET_CI_SECS
    } else {
        PERF_BUDGET_DEV_SECS
    })
}

#[test]
fn perf_budget_resolution_preserves_dev_ci_and_bounded_override_tiers() {
    assert_eq!(resolve_perf_budget(false, None), Duration::from_secs(30));
    assert_eq!(resolve_perf_budget(true, None), Duration::from_secs(60));
    assert_eq!(
        resolve_perf_budget(true, Some(" 120 ")),
        Duration::from_secs(120)
    );
}

#[test]
#[should_panic(expected = "SOLO_PERF_CI_BUDGET_SECONDS requires SOLO_PERF_CI_BUDGET")]
fn perf_budget_override_requires_ci_mode() {
    let _ = resolve_perf_budget(false, Some("120"));
}

#[test]
#[should_panic(expected = "must be between 60 and 300")]
fn perf_budget_override_is_bounded() {
    let _ = resolve_perf_budget(true, Some("301"));
}

/// ADR-0003 #19: snapshot save failure (mock `hnsw.save` to return Err);
/// writer continues serving writes; `save_count` increments; no crash.
#[test]
fn snapshot_failure_does_not_crash_writer() {
    let (conn, _tmp) = open_test_db();
    let stub = Arc::new(StubVectorIndex::new(4));
    stub.set_save_fails(true);
    let WriterSpawn { handle, join } = WriterActor::spawn_with_snapshot_dir(
        conn,
        stub.clone(),
        std::path::PathBuf::from("/dev/null"), // never actually written to
    );

    let runtime = rt_multi(2);
    runtime.block_on(async {
        // The save call returns Err but doesn't panic — caller observes Err.
        let err = handle.save_snapshot().await.unwrap_err();
        assert!(
            err.to_string().contains("stub configured to fail"),
            "got: {err}"
        );
        // The writer is still serving — subsequent remember succeeds.
        let mid = handle
            .remember(fixture_episode("post-fail"), fixture_embedding(4))
            .await
            .unwrap();
        let _ = mid;
    });
    drop(handle);
    join.join().expect("writer thread joined cleanly");

    // save was attempted exactly once.
    assert_eq!(stub.save_count(), 1);
    assert_eq!(stub.add_count(), 1);
}

/// ADR-0003 #14: write channel saturated; `WriteHandle::send().await`
/// blocks the caller correctly; no panic; backpressure clears once
/// writer drains.
///
/// We construct the actor with capacity=2 and slow-add via a 100ms sleep.
/// 5 sequential awaits: first 2 land in the channel, third blocks, etc.
/// Total wall time ≥ ~300ms. We assert the ordering implicitly via the
/// writer's serial dispatch.
#[test]
fn write_channel_full_blocks_caller_then_drains() {
    let (conn, _tmp) = open_test_db();
    let stub = Arc::new(StubVectorIndex::new(4));
    stub.set_add_sleep(Some(Duration::from_millis(50)));
    let WriterSpawn { handle, join } = WriterActor::spawn_with_capacity(conn, stub.clone(), 2);

    let runtime = rt_multi(2);
    let started = std::time::Instant::now();
    runtime.block_on(async {
        // 5 sequential remembers. Each `add` sleeps 50ms inside the writer
        // thread. Channel capacity 2 means after the first 2 are queued
        // the next send().await blocks until one drains.
        for i in 0..5 {
            handle
                .remember(fixture_episode(&format!("burst-{i}")), fixture_embedding(4))
                .await
                .unwrap();
        }
    });
    let elapsed = started.elapsed();

    // 5 writes × 50ms each (serialised) = ~250ms minimum, plus mpsc
    // overhead. Assert ≥ 200ms (allowing for a fast machine).
    assert!(
        elapsed >= Duration::from_millis(200),
        "5 slow writes finished in {elapsed:?}; expected ≥ 200ms"
    );
    assert_eq!(stub.add_count(), 5);

    drop(handle);
    join.join().expect("writer thread joined cleanly");
}

/// ADR-0003 #25: slow `hnsw.add` simulated to take 5 sec; channel saturates;
/// `WriteHandle::send().await` blocks; verify no deadlock and recovery
/// after the slow add completes.
///
/// Scaled-down version: 200ms add, capacity 1, 3 sequential writes.
/// Total wall time ≈ 600ms; we assert ≤ 2s (lenient ceiling for CI).
#[test]
fn very_slow_hnsw_add_does_not_deadlock() {
    let (conn, _tmp) = open_test_db();
    let stub = Arc::new(StubVectorIndex::new(4));
    stub.set_add_sleep(Some(Duration::from_millis(200)));
    let WriterSpawn { handle, join } = WriterActor::spawn_with_capacity(conn, stub.clone(), 1);

    let runtime = rt_multi(2);
    let started = std::time::Instant::now();
    runtime.block_on(async {
        for i in 0..3 {
            handle
                .remember(fixture_episode(&format!("slow-{i}")), fixture_embedding(4))
                .await
                .unwrap();
        }
    });
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(500),
        "expected ≥ 500ms, got {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "expected < 2s, got {elapsed:?} (deadlock?)"
    );
    assert_eq!(stub.add_count(), 3);

    drop(handle);
    join.join().expect("writer thread joined cleanly");
}

/// ADR-0003 #14 (extension of unit-test version): 200 concurrent writes
/// + 50 concurrent reads against the file-backed pool, all complete
/// without `SQLITE_BUSY`. Stress-tests the writer-actor + reader-pool
/// model on real SQLite WAL mode.
#[test]
fn high_concurrency_reads_and_writes_complete_without_sqlite_busy() {
    use crate::reader::ReaderPool;
    use crate::test_support::open_test_db_at;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("stress.db");
    // Lay down the schema first.
    let _ = open_test_db_at(&path);

    let stub: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(4));

    // Writer connection (separate from the schema-init handle).
    let writer_conn = open_test_db_at(&path);
    let WriterSpawn { handle, join } =
        WriterActor::spawn_with_capacity(writer_conn, stub.clone(), 1024);

    let runtime = rt_multi(4);
    runtime.block_on(async {
        let pool = ReaderPool::new(&path, None, stub.clone()).unwrap();

        let mut tasks = Vec::new();

        // 200 writers.
        for i in 0..200 {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                h.remember(
                    fixture_episode(&format!("stress-{i}")),
                    fixture_embedding(4),
                )
                .await
            }));
        }

        // 50 concurrent readers, each issuing a count query.
        let mut read_tasks = Vec::new();
        for _ in 0..50 {
            let p = &pool;
            read_tasks.push(p.interact(|conn| {
                conn.query_row("SELECT COUNT(*) FROM episodes", [], |r| r.get::<_, i64>(0))
            }));
        }

        // Drain writers.
        for t in tasks {
            t.await.unwrap().expect("write must succeed");
        }
        // Drain readers.
        for r in read_tasks {
            let _: i64 = r.await.expect("read must not surface SQLITE_BUSY");
        }
    });

    drop(handle);
    join.join().expect("writer thread joined cleanly");
}

/// Inserting an episode with a duplicate memory_id violates the UNIQUE
/// constraint on episodes.memory_id. The writer should surface this as
/// a clear error and the underlying SQL state should remain clean
/// (no half-written rows; pending_index doesn't have a stranded row).
#[test]
fn duplicate_memory_id_is_rejected_with_clean_state() {
    let (conn, _tmp) = open_test_db();
    let stub = std::sync::Arc::new(StubVectorIndex::new(4));
    let WriterSpawn { handle, join } = WriterActor::spawn(conn, stub.clone());

    let runtime = rt_multi(2);
    let mid = solo_core::MemoryId::new();
    runtime.block_on(async {
        // First insert: build episode with the same memory_id.
        let mut e1 = fixture_episode("first");
        e1.memory_id = mid;
        handle
            .remember(e1, fixture_embedding(4))
            .await
            .expect("first remember succeeds");

        // Second insert with same memory_id: must fail.
        let mut e2 = fixture_episode("dup");
        e2.memory_id = mid;
        let err = handle
            .remember(e2, fixture_embedding(4))
            .await
            .expect_err("duplicate memory_id must fail");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("unique") || msg.to_lowercase().contains("constraint"),
            "expected unique-constraint message, got: {msg}"
        );
    });

    drop(handle);
    join.join().expect("writer thread joined cleanly");

    // hnsw should have been called exactly once (for the successful insert).
    assert_eq!(stub.add_count(), 1);
}

/// `forget` on a memory while a write is pending should serialize via the
/// actor — no race-window where the forget UPDATE runs before the
/// remember INSERT or vice versa. Test the simple sequential case.
#[test]
fn forget_after_remember_is_consistent() {
    let (conn, _tmp) = open_test_db();
    let stub = std::sync::Arc::new(StubVectorIndex::new(4));
    let WriterSpawn { handle, join } = WriterActor::spawn(conn, stub);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let ep = fixture_episode("to forget");
        let mid = ep.memory_id;
        handle
            .remember(ep, fixture_embedding(4))
            .await
            .expect("remember succeeds");
        // Sequential forget — same actor.
        handle
            .forget(mid, "test".into())
            .await
            .expect("forget succeeds");
        // Idempotent re-forget.
        handle
            .forget(mid, "test".into())
            .await
            .expect("re-forget is Ok (idempotent)");
    });

    drop(handle);
    join.join().expect("writer thread joined cleanly");
}

/// Per the embeddings-table-writes commit: when the writer is given a
/// cached `embedder_id`, every `remember` also INSERTs an
/// `embeddings` row. Without `embedder_id` (test-default spawn), the
/// row is skipped and only `pending_index` gets written. Verifies
/// both branches.
#[test]
fn remember_persists_to_embeddings_when_embedder_id_is_set() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::test_support::open_test_db_at;
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");

    // Register an embedder + open the writer with a real embedder_id.
    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: 4,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    let conn = open_test_db_at(&path);
    let stub = std::sync::Arc::new(StubVectorIndex::new(4));
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub.clone(), tmp.path().to_path_buf(), embedder_id);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let ep = fixture_episode("with-embeddings-row");
        let mid = ep.memory_id;
        handle
            .remember(ep, fixture_embedding(4))
            .await
            .expect("remember");

        // Verify the embeddings row is there.
        let read_conn = open_test_db_at(&path);
        let n: i64 = read_conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE memory_id = ?",
                rusqlite::params![mid.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "embeddings row missing for {mid}");

        let (eid_stored, dim_stored, dtype_stored): (i64, i64, String) = read_conn
            .query_row(
                "SELECT embedder_id, dim, dtype FROM embeddings WHERE memory_id = ?",
                rusqlite::params![mid.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(eid_stored, embedder_id);
        assert_eq!(dim_stored, 4);
        assert_eq!(dtype_stored, "f32");
    });

    drop(handle);
    join.join().expect("writer thread joined cleanly");
}

#[test]
fn remember_skips_embeddings_when_embedder_id_is_none() {
    let (conn, _tmp) = open_test_db();
    let stub = std::sync::Arc::new(StubVectorIndex::new(4));
    // Default spawn — no embedder_id.
    let WriterSpawn { handle, join } = WriterActor::spawn(conn, stub.clone());

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let ep = fixture_episode("without-eid");
        let mid = ep.memory_id;
        handle
            .remember(ep, fixture_embedding(4))
            .await
            .expect("remember");
        // pending_index drained → 0 rows there. embeddings has 0 rows
        // because the writer skipped the INSERT.
        let _ = mid;
    });

    drop(handle);
    join.join().expect("writer thread joined cleanly");
    // No DB read here — open_test_db gave us a one-shot conn that the
    // writer owns; verifying state would mean opening another conn to
    // the same file. The test_support helper uses a tempdir, so this
    // is doable, but the test's job is just to confirm "no panic /
    // error on the no-embedder-id path", which the await-unwrap
    // covers.
}

/// Regression for the forget-tombstone bug found in the third audit
/// pass. After `forget`, the HNSW must have a tombstone for the
/// rowid so `index.len()` reflects only active vectors at runtime —
/// without this, drift detection fires spurious warnings and recall
/// responses report a misleading `index_len`.
#[test]
fn forget_tombstones_the_hnsw_at_runtime() {
    let (conn, _tmp) = open_test_db();
    let stub = std::sync::Arc::new(StubVectorIndex::new(4));
    let WriterSpawn { handle, join } = WriterActor::spawn(conn, stub.clone());

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let ep = fixture_episode("to forget at runtime");
        let mid = ep.memory_id;
        handle
            .remember(ep, fixture_embedding(4))
            .await
            .expect("remember");
        // Pre-forget: add_count = 1, remove_count = 0.
        assert_eq!(stub.add_count(), 1);
        assert_eq!(stub.remove_count(), 0);

        handle.forget(mid, "test".into()).await.expect("forget");

        // Post-forget: remove_count = 1 (handle_forget called hnsw.remove).
        assert_eq!(stub.remove_count(), 1);
    });

    drop(handle);
    join.join().expect("writer thread joined cleanly");
}

/// Multiple WriteHandles cloned from the same WriterSpawn all reach the
/// same actor. Drop them in any order; the actor only exits when the
/// LAST one drops.
#[test]
fn multiple_clones_keep_actor_alive_until_last_drop() {
    let (conn, _tmp) = open_test_db();
    let stub = std::sync::Arc::new(StubVectorIndex::new(4));
    let WriterSpawn { handle, join } = WriterActor::spawn(conn, stub.clone());

    let h2 = handle.clone();
    let h3 = h2.clone();

    let runtime = rt_multi(2);
    runtime.block_on(async {
        // h3 writes → succeeds (actor alive).
        h3.remember(fixture_episode("via clone"), fixture_embedding(4))
            .await
            .expect("write through clone");
    });

    // Drop two of three handles — actor still alive.
    drop(h2);
    drop(h3);
    runtime.block_on(async {
        // The original handle still works.
        handle
            .remember(fixture_episode("after partial drop"), fixture_embedding(4))
            .await
            .expect("write after dropping clones");
    });

    // Final drop closes the channel.
    drop(handle);
    join.join().expect("writer thread joined cleanly");

    assert_eq!(stub.add_count(), 2);
}

/// Sanity: an embedder returning a non-F32 dtype is rejected by the
/// writer (since the trait says HNSW requires F32). Ensures the
/// `as_f32_slice().ok_or_else` branch in `dispatch_remember` is real.
#[test]
fn writer_rejects_non_f32_embedding_with_clear_error() {
    let (conn, _tmp) = open_test_db();
    let stub = Arc::new(StubVectorIndex::new(4));
    let WriterSpawn { handle, join: _ } = WriterActor::spawn(conn, stub);

    // Construct an F16 embedding by hand.
    let bad = Embedding {
        dtype: EmbeddingDtype::F16,
        dim: 4,
        data: vec![0u8; 4 * 2],
    };
    let runtime = rt_multi(1);
    let res: Result<solo_core::MemoryId> =
        runtime.block_on(async { handle.remember(fixture_episode("non-f32"), bad).await });
    let err = res.unwrap_err();
    assert!(err.to_string().contains("HNSW expects F32"), "got: {err}");
}

// ---------------------------------------------------------------------------
// `solo reembed` — handle_reembed property tests
// ---------------------------------------------------------------------------

/// Helper that pre-populates `path` with `n` episodes whose `embeddings`
/// rows reference `old_embedder_id`. Returns the memory_ids in insertion
/// order. Uses the no-embedder writer path because we don't need the
/// runtime hook for plain remembering.
fn seed_episodes_under_embedder(
    path: &std::path::Path,
    snapshot_dir: &std::path::Path,
    old_embedder_id: i64,
    contents: &[&str],
) -> Vec<solo_core::MemoryId> {
    let conn = open_test_db_at(path);
    let stub = Arc::new(StubVectorIndex::new(4));
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub, snapshot_dir.to_path_buf(), old_embedder_id);
    let runtime = rt_multi(2);
    let mids = runtime.block_on(async {
        let mut mids = Vec::with_capacity(contents.len());
        for c in contents {
            let ep = fixture_episode(c);
            mids.push(ep.memory_id);
            handle
                .remember(ep, fixture_embedding(4))
                .await
                .expect("seed remember");
        }
        mids
    });
    drop(handle);
    join.join().expect("seed writer joined cleanly");
    mids
}

/// Default reembed (no `--gc`): every memory whose existing row is
/// non-current gets a fresh row under the current embedder_id; the old
/// rows stay put. `rows_seen == rows_reembedded` for the happy path.
#[test]
fn reembed_inserts_new_rows_without_gc() {
    use crate::embedder::StubEmbedder;
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ReembedScope;
    use solo_core::Embedder;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let snap = tmp.path().to_path_buf();

    // Two embedders registered: an "old" stub and a "new" stub.
    let (old_id, new_id) = {
        let conn = open_test_db_at(&path);
        let old = get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub-old".into(),
                version: "v1".into(),
                dim: 4,
                dtype: "f32".into(),
            },
        )
        .unwrap();
        let new = get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub-new".into(),
                version: "v1".into(),
                dim: 4,
                dtype: "f32".into(),
            },
        )
        .unwrap();
        (old, new)
    };

    let _mids = seed_episodes_under_embedder(&path, &snap, old_id, &["alpha", "beta", "gamma"]);

    // Run reembed under the new embedder_id.
    let runtime = rt_multi(2);
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub = Arc::new(StubVectorIndex::new(4));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub-new", "v1", 4));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder(conn, stub, snap.clone(), new_id, embedder);
        let report = handle
            .reembed(ReembedScope::default())
            .await
            .expect("reembed dispatch");
        assert_eq!(report.rows_seen, 3, "all 3 stale memories selected");
        assert_eq!(report.rows_reembedded, 3);
        assert_eq!(report.rows_failed, 0);
        assert_eq!(report.rows_gc_deleted, 0);
        assert!(!report.dry_run);
        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Verify: 3 old + 3 new = 6 total rows.
    let read = open_test_db_at(&path);
    let total: i64 = read
        .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 6, "without --gc, old rows are retained");
    let new_count: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE embedder_id = ?",
            rusqlite::params![new_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_count, 3);
    let old_count: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE embedder_id = ?",
            rusqlite::params![old_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_count, 3);
}

/// `--gc`: stale rows DELETEd after the new row is committed. End-state
/// has only `embedder_id == current` rows.
#[test]
fn reembed_with_gc_drops_stale_rows() {
    use crate::embedder::StubEmbedder;
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ReembedScope;
    use solo_core::Embedder;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let snap = tmp.path().to_path_buf();

    let (old_id, new_id) = {
        let conn = open_test_db_at(&path);
        (
            get_or_insert_embedder_id(
                &conn,
                &EmbedderIdentity {
                    name: "stub-old".into(),
                    version: "v1".into(),
                    dim: 4,
                    dtype: "f32".into(),
                },
            )
            .unwrap(),
            get_or_insert_embedder_id(
                &conn,
                &EmbedderIdentity {
                    name: "stub-new".into(),
                    version: "v1".into(),
                    dim: 4,
                    dtype: "f32".into(),
                },
            )
            .unwrap(),
        )
    };
    let _mids = seed_episodes_under_embedder(&path, &snap, old_id, &["a", "b"]);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub = Arc::new(StubVectorIndex::new(4));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub-new", "v1", 4));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder(conn, stub, snap.clone(), new_id, embedder);
        let report = handle
            .reembed(ReembedScope {
                from: None,
                dry_run: false,
                gc: true,
            })
            .await
            .expect("reembed dispatch");
        assert_eq!(report.rows_seen, 2);
        assert_eq!(report.rows_reembedded, 2);
        assert_eq!(report.rows_gc_deleted, 2, "two stale rows DELETEd");
        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    let read = open_test_db_at(&path);
    let total: i64 = read
        .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2, "only the new rows remain");
    let only_current: i64 = read
        .query_row(
            "SELECT COUNT(DISTINCT embedder_id) FROM embeddings",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(only_current, 1);
}

/// `--dry-run` reports the candidate count and writes nothing.
#[test]
fn reembed_dry_run_writes_nothing() {
    use crate::embedder::StubEmbedder;
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ReembedScope;
    use solo_core::Embedder;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let snap = tmp.path().to_path_buf();

    let (old_id, new_id) = {
        let conn = open_test_db_at(&path);
        (
            get_or_insert_embedder_id(
                &conn,
                &EmbedderIdentity {
                    name: "stub-old".into(),
                    version: "v1".into(),
                    dim: 4,
                    dtype: "f32".into(),
                },
            )
            .unwrap(),
            get_or_insert_embedder_id(
                &conn,
                &EmbedderIdentity {
                    name: "stub-new".into(),
                    version: "v1".into(),
                    dim: 4,
                    dtype: "f32".into(),
                },
            )
            .unwrap(),
        )
    };
    let _mids = seed_episodes_under_embedder(&path, &snap, old_id, &["x", "y"]);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub = Arc::new(StubVectorIndex::new(4));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub-new", "v1", 4));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder(conn, stub, snap.clone(), new_id, embedder);
        let report = handle
            .reembed(ReembedScope {
                from: None,
                dry_run: true,
                gc: false,
            })
            .await
            .expect("reembed dispatch");
        assert_eq!(report.rows_seen, 2);
        assert_eq!(report.rows_reembedded, 0, "dry-run writes nothing");
        assert!(report.dry_run);
        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Embeddings table untouched: still 2 rows, all old.
    let read = open_test_db_at(&path);
    let total: i64 = read
        .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2);
    let with_new: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE embedder_id = ?",
            rusqlite::params![new_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(with_new, 0);
}

/// Idempotency: running reembed twice is safe. The second pass sees
/// zero stale candidates (the SELECT excludes embedder_id == current),
/// so it's a no-op.
#[test]
fn reembed_is_idempotent_when_run_twice() {
    use crate::embedder::StubEmbedder;
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ReembedScope;
    use solo_core::Embedder;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let snap = tmp.path().to_path_buf();

    let (old_id, new_id) = {
        let conn = open_test_db_at(&path);
        (
            get_or_insert_embedder_id(
                &conn,
                &EmbedderIdentity {
                    name: "stub-old".into(),
                    version: "v1".into(),
                    dim: 4,
                    dtype: "f32".into(),
                },
            )
            .unwrap(),
            get_or_insert_embedder_id(
                &conn,
                &EmbedderIdentity {
                    name: "stub-new".into(),
                    version: "v1".into(),
                    dim: 4,
                    dtype: "f32".into(),
                },
            )
            .unwrap(),
        )
    };
    let _mids = seed_episodes_under_embedder(&path, &snap, old_id, &["only"]);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub = Arc::new(StubVectorIndex::new(4));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub-new", "v1", 4));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder(conn, stub, snap.clone(), new_id, embedder);
        let r1 = handle
            .reembed(ReembedScope {
                gc: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r1.rows_reembedded, 1);
        let r2 = handle
            .reembed(ReembedScope {
                gc: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r2.rows_seen, 0, "no candidates remain after first pass");
        assert_eq!(r2.rows_reembedded, 0);
        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });
}

// ---------------------------------------------------------------------------
// `WriteCommand::Consolidate` — handle_consolidate property tests
// ---------------------------------------------------------------------------

/// Build an Episode at a specific `ts_ms` (test-only — `fixture_episode`
/// uses `Utc::now()` which is unhelpful for clustering tests that
/// depend on UTC-day bucketing).
fn ep_at(ts_ms: i64, content: &str) -> solo_core::Episode {
    solo_core::Episode {
        memory_id: solo_core::MemoryId::new(),
        ts_ms,
        source_type: "user_message".into(),
        source_id: None,
        content: content.into(),
        encoding_context: solo_core::EncodingContext::default(),
        provenance: None,
        confidence: solo_core::Confidence::new(0.9).unwrap(),
        strength: 0.5,
        salience: 0.5,
        tier: solo_core::Tier::Hot,
    }
}

/// Build a unit-norm F32 Embedding from sparse `(index, value)` pairs.
fn unit_emb(dim: usize, components: &[(usize, f32)]) -> Embedding {
    let mut v = vec![0.0f32; dim];
    for &(i, x) in components {
        v[i] = x;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    Embedding {
        dtype: EmbeddingDtype::F32,
        dim,
        data: bytemuck::cast_slice(&v).to_vec(),
    }
}

/// End-to-end: 6 hand-crafted memories (two themes, same UTC day) →
/// `WriteCommand::Consolidate` produces 2 clusters of 3, persists them
/// to `clusters` + `cluster_episodes`. Same-shape coverage as the
/// `cluster::tests::two_clusters_per_bucket_when_two_themes` unit
/// test, but exercising the full writer + SQL path.
#[test]
fn consolidate_clusters_two_themes_into_two_persisted_clusters() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let snap = tmp.path().to_path_buf();
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    let conn = open_test_db_at(&path);
    let stub = Arc::new(StubVectorIndex::new(dim));
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub.clone(), snap, embedder_id);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let day_a = 1_700_000_000_000i64;
        // Theme A: dim 0 cluster, 3 episodes
        // Theme B: dim 2 cluster, 3 episodes
        let inputs = [
            (ep_at(day_a, "a1"), unit_emb(dim, &[(0, 1.0)])),
            (
                ep_at(day_a + 1000, "a2"),
                unit_emb(dim, &[(0, 0.99), (1, 0.01)]),
            ),
            (
                ep_at(day_a + 2000, "a3"),
                unit_emb(dim, &[(0, 0.98), (1, 0.02)]),
            ),
            (ep_at(day_a + 3000, "b1"), unit_emb(dim, &[(2, 1.0)])),
            (
                ep_at(day_a + 4000, "b2"),
                unit_emb(dim, &[(2, 0.99), (3, 0.01)]),
            ),
            (
                ep_at(day_a + 5000, "b3"),
                unit_emb(dim, &[(2, 0.98), (3, 0.02)]),
            ),
        ];
        for (ep, emb) in &inputs {
            handle
                .remember(ep.clone(), emb.clone())
                .await
                .expect("remember");
        }

        let report = handle
            .consolidate(ConsolidationScope::default())
            .await
            .expect("consolidate");
        assert_eq!(report.episodes_seen, 6);
        assert_eq!(report.clusters_built, 2);
        assert_eq!(report.episodes_clustered, 6);
        assert_eq!(report.abstractions_built, 0);
        assert_eq!(report.contradictions_found, 0);

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Verify persistence.
    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 2);
    let n_links: i64 = read
        .query_row("SELECT COUNT(*) FROM cluster_episodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_links, 6);

    // Centroids round-tripped.
    let with_centroid: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM clusters WHERE centroid IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(with_centroid, 2);
}

/// Empty database → consolidate is a clean no-op (no rows in
/// `clusters` / `cluster_episodes`, report all zeros).
#[test]
fn consolidate_no_op_when_no_episodes() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: 4,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };
    let conn = open_test_db_at(&path);
    let stub = Arc::new(StubVectorIndex::new(4));
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub, tmp.path().to_path_buf(), embedder_id);
    let runtime = rt_multi(1);
    runtime.block_on(async {
        let report = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(report.episodes_seen, 0);
        assert_eq!(report.clusters_built, 0);
        assert_eq!(report.episodes_clustered, 0);
    });
    drop(handle);
    join.join().unwrap();
}

/// **v0.9.0 P4b update**: `WriteCommand::Consolidate` no longer
/// runs the LLM-driven abstraction step inline — that work moved
/// to the daemon-side background-batch path (plan §4 P4 / brief
/// test #6 `triple extraction does NOT happen in the writer-actor's
/// command path`). The cheap clustering pass still runs; the
/// report's `abstractions_built` + `triples_built` counters stay at
/// 0 from the writer-actor's perspective. Downstream tests for the
/// actual abstraction-producing behavior live in the consolidate-
/// timer batch path (see [`crate::commands::daemon`] in `solo-cli`).
#[test]
fn consolidate_with_steward_persists_abstractions() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    let runtime = rt_multi(2);
    // Canned response with two triples so we can verify persistence
    // of the `triples` table alongside `semantic_abstractions`.
    let canned = r#"{
        "content": "Three abstract events about widgets.",
        "confidence": 0.8,
        "triples": [
            { "subject_id": "Widget", "predicate": "is", "object_id": "thing", "object_kind": "entity" },
            { "subject_id": "Widget", "predicate": "color", "object_id": "blue", "object_kind": "literal" }
        ]
    }"#;
    let stub = Arc::new(StubLlmClient::with_canned("stub-llm", canned));
    let steward_for_writer = Arc::new(Steward::new(stub, StewardConfig::default()));
    let steward_for_assert = steward_for_writer.clone();

    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub_idx = Arc::new(StubVectorIndex::new(dim));
        // Need an embedder Arc for the constructor; the Stub one
        // is fine — it's not used by the Remember path here (we
        // pre-compute embeddings in the test fixtures).
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));

        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder_and_optional_steward(
                conn,
                stub_idx,
                tmp.path().to_path_buf(),
                embedder_id,
                embedder,
                Some(steward_for_writer),
            );

        let day_a = 1_700_000_000_000i64;
        let inputs = [
            (ep_at(day_a, "abstract-1"), unit_emb(dim, &[(0, 1.0)])),
            (
                ep_at(day_a + 1000, "abstract-2"),
                unit_emb(dim, &[(0, 1.0)]),
            ),
            (
                ep_at(day_a + 2000, "abstract-3"),
                unit_emb(dim, &[(0, 1.0)]),
            ),
        ];
        for (ep, emb) in &inputs {
            handle.remember(ep.clone(), emb.clone()).await.unwrap();
        }

        let report = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(report.clusters_built, 1);
        // v0.9.0 P4b: writer-actor no longer runs abstract_cluster
        // inline; the cluster row lands but the abstraction +
        // triples are now produced by the daemon-side background
        // batch (plan §4 P4). From the writer-actor's view, the
        // counters stay at 0.
        assert_eq!(
            report.abstractions_built, 0,
            "v0.9.0 P4b: abstraction step moved out of writer-actor"
        );
        assert_eq!(
            report.triples_built, 0,
            "v0.9.0 P4b: triple persistence moved out of writer-actor"
        );

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Storage state (v0.9.0 P4b): cluster persisted; abstractions +
    // triples deferred to the daemon-side background batch.
    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 1);
    let n_abs: i64 = read
        .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        n_abs, 0,
        "v0.9.0 P4b: writer-actor's consolidate no longer writes \
         semantic_abstractions; the daemon batch path handles it"
    );
    let n_triples: i64 = read
        .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        n_triples, 0,
        "v0.9.0 P4b: writer-actor's consolidate no longer writes triples; \
         the daemon batch path handles it"
    );

    let _ = steward_for_assert; // keep the second Arc alive long enough
    // for assertions; not otherwise read.
}

/// Y.4.2 — consolidate's contradiction sweep. Two consecutive
/// consolidate runs surface a contradiction across runs:
///
///   Run 1: cluster of 3 episodes → abstraction with one triple
///          (Sam, lives_in, Paris, valid_from=now1, valid_to=None).
///   Run 2: cluster of 3 episodes → abstraction with one triple
///          (Sam, lives_in, Berlin, valid_from=now2, valid_to=None).
///          Validity windows overlap → rule filter passes →
///          LLM judge says "yes, contradiction" via canned response.
///          Persisted to `contradictions` table; report counts 1.
///
/// We can't easily trigger contradictions WITHIN one consolidate
/// run because all clusters in a run inherit the same `now_ms` ts
/// for their triples → identical validity windows but the LLM judge
/// has nothing to disagree about (clusters are coherent by
/// construction). Cross-run is the realistic case.
#[test]
fn consolidate_with_steward_persists_contradictions_across_runs() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    // Stub canned responses queue — drained FIFO. We push enough
    // for: run-1 abstract_cluster (1) + run-2 abstract_cluster (1)
    // + run-2 detect_contradiction × N candidates. The LLM judge
    // gets ONE pair (run-2 new triple × run-1 stored triple), so
    // exactly one canned judge response.
    //
    // `pretend_real_llm(true)` makes `Steward::has_llm()` report
    // `true` for this stub. Without it, the writer's contradiction-
    // sweep gate (v0.5.0 sub-step 2B) early-returns and run-2
    // never reaches the judge — defeating the test's purpose.
    let stub = Arc::new(StubLlmClient::default_stub().pretend_real_llm(true));
    // Run 1 abstraction: one triple (Sam, lives_in, Paris).
    stub.push_canned(
        r#"{
            "content": "Sam settled in Paris.",
            "confidence": 0.9,
            "triples": [
                { "subject_id": "Sam", "predicate": "lives_in",
                  "object_id": "Paris", "object_kind": "entity" }
            ]
        }"#,
    );
    // Run 2 abstraction: one triple (Sam, lives_in, Berlin).
    stub.push_canned(
        r#"{
            "content": "Sam moved to Berlin.",
            "confidence": 0.9,
            "triples": [
                { "subject_id": "Sam", "predicate": "lives_in",
                  "object_id": "Berlin", "object_kind": "entity" }
            ]
        }"#,
    );
    // Run 2 contradiction judge for the (new × existing) pair.
    stub.push_canned(
        r#"{
            "is_contradiction": true,
            "kind": "overlapping_single_valued_predicate",
            "explanation": "Sam can't live in both Paris and Berlin at the same time."
        }"#,
    );

    let steward = Arc::new(Steward::new(stub.clone(), StewardConfig::default()));

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub_idx = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder_and_optional_steward(
                conn,
                stub_idx,
                tmp.path().to_path_buf(),
                embedder_id,
                embedder,
                Some(steward),
            );

        // Run 1: 3 identical-content episodes → 1 cluster → 1
        // abstraction → 1 triple about Paris.
        let day_a = 1_700_000_000_000i64;
        for i in 0..3 {
            handle
                .remember(
                    ep_at(day_a + i * 1000, "sam-paris"),
                    unit_emb(dim, &[(0, 1.0)]),
                )
                .await
                .unwrap();
        }
        let r1 = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r1.clusters_built, 1);
        // v0.9.0 P4b: writer-actor's consolidate no longer runs the
        // abstraction / contradiction-sweep loops inline. The cluster
        // row lands; everything LLM-driven is deferred to the daemon-
        // side background batch (plan §4 P4).
        assert_eq!(r1.abstractions_built, 0);
        assert_eq!(r1.triples_built, 0);
        assert_eq!(r1.contradictions_found, 0);

        // Run 2: 3 different-content episodes (still cluster
        // because identical vectors via repeated content) → 1
        // cluster. Abstraction / contradiction work moved to the
        // background batch.
        let day_b = day_a + 86_400_000 * 2; // +2 days, separate UTC bucket
        for i in 0..3 {
            handle
                .remember(
                    ep_at(day_b + i * 1000, "sam-berlin"),
                    unit_emb(dim, &[(1, 1.0)]),
                )
                .await
                .unwrap();
        }
        let r2 = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r2.clusters_built, 1);
        assert_eq!(r2.abstractions_built, 0);
        assert_eq!(r2.triples_built, 0);
        assert_eq!(r2.contradictions_found, 0);

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // v0.9.0 P4b: writer-actor's consolidate no longer writes
    // contradictions; the contradictions table stays empty until the
    // daemon-side batch path runs.
    let read = open_test_db_at(&path);
    let n_contras: i64 = read
        .query_row("SELECT COUNT(*) FROM contradictions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_contras, 0);

    // Idempotency: running consolidate a third time produces the same
    // result.
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub_idx = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
        // Re-create steward; the Arc<Steward> from above was moved.
        let s2 = Arc::new(Steward::new(stub.clone(), StewardConfig::default()));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder_and_optional_steward(
                conn,
                stub_idx,
                tmp.path().to_path_buf(),
                embedder_id,
                embedder,
                Some(s2),
            );
        let r3 = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r3.episodes_seen, 0, "no new candidates");
        assert_eq!(r3.contradictions_found, 0);
        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Still no contradictions written by the writer-actor.
    let read = open_test_db_at(&path);
    let n_contras: i64 = read
        .query_row("SELECT COUNT(*) FROM contradictions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_contras, 0);
}

/// v0.5.0 sub-step 2B: a `Steward` wrapping a stub LLM (i.e.
/// `has_llm() == false`) MUST skip the contradiction sweep entirely.
/// Reproduces the cross-run "Paris vs Berlin" setup from
/// `consolidate_with_steward_persists_contradictions_across_runs` —
/// but WITHOUT the `pretend_real_llm(true)` toggle. The expected
/// outcome flips: `contradictions_found` stays 0, the
/// `contradictions` table stays empty, and the consolidate run
/// completes without panic or error. The cluster + abstraction
/// stages still run (they tolerate a stub via canned responses);
/// only the sweep is gated.
///
/// We don't assert on the `tracing::warn!` text because Solo doesn't
/// pull in `tracing-subscriber::test` infra (test-log only enables
/// output, doesn't capture). The behavioural assertion — sweep
/// produces no work, no error — is sufficient.
#[test]
fn contradiction_sweep_skipped_when_no_llm_client() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    // Default stub: `is_real_llm()` returns `false`. No
    // `pretend_real_llm(true)` here — that's the point of this test.
    // Queue canned abstractions for both runs. We deliberately do
    // NOT queue a contradiction judge response, because the gate
    // should prevent any judge call from happening.
    let stub = Arc::new(StubLlmClient::default_stub());
    stub.push_canned(
        r#"{
            "content": "Sam settled in Paris.",
            "confidence": 0.9,
            "triples": [
                { "subject_id": "Sam", "predicate": "lives_in",
                  "object_id": "Paris", "object_kind": "entity" }
            ]
        }"#,
    );
    stub.push_canned(
        r#"{
            "content": "Sam moved to Berlin.",
            "confidence": 0.9,
            "triples": [
                { "subject_id": "Sam", "predicate": "lives_in",
                  "object_id": "Berlin", "object_kind": "entity" }
            ]
        }"#,
    );

    let steward = Arc::new(Steward::new(stub.clone(), StewardConfig::default()));
    // Pre-condition: confirm the steward reports no LLM.
    assert!(
        !steward.has_llm(),
        "default stub must report has_llm() == false; without the gate this test reduces to the existing cross-runs test"
    );

    let runtime = rt_multi(2);
    let call_count_before_runs = stub.call_count();
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub_idx = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder_and_optional_steward(
                conn,
                stub_idx,
                tmp.path().to_path_buf(),
                embedder_id,
                embedder,
                Some(steward),
            );

        let day_a = 1_700_000_000_000i64;
        for i in 0..3 {
            handle
                .remember(
                    ep_at(day_a + i * 1000, "sam-paris"),
                    unit_emb(dim, &[(0, 1.0)]),
                )
                .await
                .unwrap();
        }
        let r1 = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        // v0.9.0 P4b: writer-actor no longer runs abstract_cluster
        // or the contradiction sweep inline. Everything LLM-driven
        // moved to the daemon-side background batch.
        assert_eq!(r1.abstractions_built, 0);
        assert_eq!(r1.triples_built, 0);
        assert_eq!(r1.contradictions_found, 0);

        let day_b = day_a + 86_400_000 * 2;
        for i in 0..3 {
            handle
                .remember(
                    ep_at(day_b + i * 1000, "sam-berlin"),
                    unit_emb(dim, &[(1, 1.0)]),
                )
                .await
                .unwrap();
        }
        let r2 = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r2.abstractions_built, 0);
        assert_eq!(r2.triples_built, 0);
        assert_eq!(r2.contradictions_found, 0);

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // v0.9.0 P4b: the stub is never called from the writer-actor's
    // consolidate path. Pre-P4 the count would have been 2 (one per
    // abstraction); now it's 0.
    let calls_after = stub.call_count();
    assert_eq!(
        calls_after - call_count_before_runs,
        0,
        "v0.9.0 P4b: writer-actor's consolidate doesn't call the LLM"
    );

    // Storage state: contradictions table empty.
    let read = open_test_db_at(&path);
    let n_contras: i64 = read
        .query_row("SELECT COUNT(*) FROM contradictions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        n_contras, 0,
        "no contradictions persisted when writer-actor consolidate runs"
    );
}

/// Without a Steward, `consolidate` runs the clustering step but
/// abstractions_built stays 0 and `semantic_abstractions` is empty.
#[test]
fn consolidate_without_steward_skips_abstraction_step() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };
    let conn = open_test_db_at(&path);
    let stub = Arc::new(StubVectorIndex::new(dim));
    // `spawn_full` does NOT supply a steward.
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub, tmp.path().to_path_buf(), embedder_id);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let day_a = 1_700_000_000_000i64;
        for i in 0..3 {
            handle
                .remember(
                    ep_at(day_a + i * 1000, &format!("noabs-{i}")),
                    unit_emb(dim, &[(0, 1.0)]),
                )
                .await
                .unwrap();
        }
        let report = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(report.clusters_built, 1);
        assert_eq!(
            report.abstractions_built, 0,
            "no steward → abstraction step is a no-op"
        );

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    let read = open_test_db_at(&path);
    let n_abs: i64 = read
        .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n_abs, 0);
}

/// Idempotency: running `consolidate` twice on the same data must not
/// create duplicate clusters. The second pass sees zero candidates
/// (every active+hot memory is already in `cluster_episodes`), so
/// nothing new is built.
#[test]
fn consolidate_is_idempotent_on_repeated_runs() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };
    let conn = open_test_db_at(&path);
    let stub = Arc::new(StubVectorIndex::new(dim));
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub, tmp.path().to_path_buf(), embedder_id);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let day_a = 1_700_000_000_000i64;
        for (i, ts_offset) in (0..3i64).enumerate() {
            let ep = ep_at(day_a + ts_offset * 1000, &format!("idem-{i}"));
            handle
                .remember(ep, unit_emb(dim, &[(0, 1.0)]))
                .await
                .unwrap();
        }
        let r1 = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r1.clusters_built, 1);
        assert_eq!(r1.episodes_clustered, 3);

        let r2 = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r2.episodes_seen, 0, "second pass: no candidates left");
        assert_eq!(r2.clusters_built, 0);

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Storage state: still exactly 1 cluster + 3 cluster_episodes rows.
    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 1, "no duplicate cluster from repeated run");
    let n_links: i64 = read
        .query_row("SELECT COUNT(*) FROM cluster_episodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_links, 3);
}

/// `window_days` filter: episodes outside the window are excluded
/// from the candidate set, so they can't drag a cluster below
/// threshold or pollute a different theme.
#[test]
fn consolidate_window_days_filters_old_episodes() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };
    let conn = open_test_db_at(&path);
    let stub = Arc::new(StubVectorIndex::new(dim));
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub, tmp.path().to_path_buf(), embedder_id);

    let runtime = rt_multi(2);
    runtime.block_on(async {
        // 3 recent (today) + 3 old (10 days ago, well outside any
        // small window).
        let now = chrono::Utc::now().timestamp_millis();
        let recent = [
            (ep_at(now - 1000, "r1"), unit_emb(dim, &[(0, 1.0)])),
            (ep_at(now - 2000, "r2"), unit_emb(dim, &[(0, 1.0)])),
            (ep_at(now - 3000, "r3"), unit_emb(dim, &[(0, 1.0)])),
        ];
        let old_ts = now - 10 * 86_400_000;
        let old = [
            (ep_at(old_ts, "o1"), unit_emb(dim, &[(0, 1.0)])),
            (ep_at(old_ts + 1000, "o2"), unit_emb(dim, &[(0, 1.0)])),
            (ep_at(old_ts + 2000, "o3"), unit_emb(dim, &[(0, 1.0)])),
        ];
        for (ep, emb) in recent.iter().chain(old.iter()) {
            handle
                .remember(ep.clone(), emb.clone())
                .await
                .expect("remember");
        }

        // window=2 days → only the recent batch is eligible.
        let report = handle
            .consolidate(ConsolidationScope {
                window_days: Some(2),
                force_merge: false,
            })
            .await
            .unwrap();
        assert_eq!(report.episodes_seen, 3, "old episodes excluded");
        assert_eq!(report.clusters_built, 1);
        assert_eq!(report.episodes_clustered, 3);

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });
}

/// Cross-run absorb: a freshly-built cluster with a centroid
/// similar to a pre-existing DB cluster gets folded into the
/// existing one — its episodes link under the existing cluster_id,
/// no new `clusters` row is created, and the existing cluster's
/// centroid + coherence refresh.
///
/// Setup: two consolidate runs on the same DB, with the second run
/// adding new pasta-themed episodes that would form their own
/// cluster under v0.2's NOT-IN guard. With cross-run absorb, the
/// new cluster is detected as similar to the day-A cluster's
/// centroid and absorbed.
#[test]
fn consolidate_cross_run_absorb_folds_into_existing_cluster() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    // ---------- Run 1: remember 3 pasta episodes on day A and consolidate.
    let conn = open_test_db_at(&path);
    let stub = Arc::new(StubVectorIndex::new(dim));
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub.clone(), tmp.path().to_path_buf(), embedder_id);

    let day_a = 1_700_000_000_000i64;
    let runtime = rt_multi(2);
    runtime.block_on(async {
        for (i, ts_offset) in (0..3i64).enumerate() {
            let ep = ep_at(day_a + ts_offset * 1000, &format!("pa{i}"));
            // All three near-identical "dim 0" centroids → 1 cluster.
            handle
                .remember(ep, unit_emb(dim, &[(0, 1.0)]))
                .await
                .unwrap();
        }
        let r = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r.clusters_built, 1, "run 1: one fresh cluster");
        assert_eq!(r.clusters_absorbed, 0, "run 1: nothing to absorb into yet");
        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Snapshot the post-run-1 cluster id + centroid bytes for later
    // comparison (proves the UPDATE actually changed the centroid).
    let (run1_cluster_id, run1_centroid_bytes): (String, Vec<u8>) = {
        let read = open_test_db_at(&path);
        read.query_row("SELECT cluster_id, centroid FROM clusters", [], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
    };

    // ---------- Run 2: 3 more pasta episodes (similar centroid) on a
    // later day. Without absorb they'd form a brand-new cluster.
    let conn2 = open_test_db_at(&path);
    let stub2 = Arc::new(StubVectorIndex::new(dim));
    let WriterSpawn {
        handle: handle2,
        join: join2,
    } = WriterActor::spawn_full(conn2, stub2.clone(), tmp.path().to_path_buf(), embedder_id);

    let day_b = day_a + 86_400_000 * 5; // 5 days later
    let runtime2 = rt_multi(2);
    runtime2.block_on(async {
        for (i, ts_offset) in (0..3i64).enumerate() {
            let ep = ep_at(day_b + ts_offset * 1000, &format!("pb{i}"));
            // Similar but not identical centroid; cosine ≈ 0.99 vs run-1.
            handle2
                .remember(ep, unit_emb(dim, &[(0, 0.99), (1, 0.01)]))
                .await
                .unwrap();
        }
        let r = handle2
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        // Brand-new cluster count is 0 — the freshly-built cluster
        // got absorbed before the INSERT into `clusters`.
        assert_eq!(r.clusters_built, 0, "run 2: no fresh cluster row");
        assert_eq!(r.clusters_absorbed, 1, "run 2: absorbed into run-1 cluster");
        // The episodes still count as "clustered" — they landed in
        // cluster_episodes under the existing id.
        assert_eq!(r.episodes_clustered, 3);
        drop(handle2);
        tokio::task::spawn_blocking(move || join2.join().unwrap())
            .await
            .unwrap();
    });

    // Final state assertions.
    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 1, "still exactly one cluster row");

    let n_links: i64 = read
        .query_row("SELECT COUNT(*) FROM cluster_episodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_links, 6, "all 6 episodes linked to one cluster");

    // All cluster_episodes rows point at the original (run-1) cluster_id.
    let n_under_run1: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM cluster_episodes WHERE cluster_id = ?1",
            params![run1_cluster_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_under_run1, 6);

    // Centroid bytes changed after absorb.
    let new_centroid_bytes: Vec<u8> = read
        .query_row(
            "SELECT centroid FROM clusters WHERE cluster_id = ?1",
            params![run1_cluster_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(
        new_centroid_bytes, run1_centroid_bytes,
        "absorb refreshed centroid"
    );
    assert_eq!(
        new_centroid_bytes.len(),
        run1_centroid_bytes.len(),
        "centroid dim unchanged"
    );
}

/// Cross-run absorb + abstraction regeneration: when an absorb
/// happens, the existing cluster's stale `semantic_abstractions`
/// + linked `triples` are dropped and a fresh abstraction is
/// generated from the cluster's full (post-absorb) episode set.
///
/// Stub LLM is fed two canned responses: run 1's original
/// abstraction (consumed by the in-run abstraction loop) and
/// run 2's regenerated abstraction (consumed by the regen pass —
/// run 2's in-run abstraction loop skips the absorbed cluster).
#[test]
fn consolidate_cross_run_absorb_regenerates_abstraction() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    // Two canned LLM responses. Run 1 consumes the first (original
    // abstraction), run 2's regen pass consumes the second.
    let original = r#"{
        "content": "Original pasta thoughts.",
        "confidence": 0.7,
        "triples": [
            { "subject_id": "user", "predicate": "likes", "object_id": "pasta", "object_kind": "literal" }
        ]
    }"#;
    let regenerated = r#"{
        "content": "Regenerated pasta thoughts (now incl. day-B episodes).",
        "confidence": 0.85,
        "triples": [
            { "subject_id": "user", "predicate": "likes", "object_id": "pasta", "object_kind": "literal" },
            { "subject_id": "user", "predicate": "frequency", "object_id": "weekly", "object_kind": "literal" }
        ]
    }"#;
    let stub = Arc::new(StubLlmClient::with_canned("stub-llm", original));
    stub.push_canned(regenerated);
    let steward = Arc::new(Steward::new(stub.clone(), StewardConfig::default()));

    let day_a = 1_700_000_000_000i64;
    let day_b = day_a + 86_400_000 * 5;

    let runtime = rt_multi(2);
    runtime.block_on(async {
        // ---- Run 1
        let conn = open_test_db_at(&path);
        let stub_idx = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder_and_optional_steward(
                conn,
                stub_idx,
                tmp.path().to_path_buf(),
                embedder_id,
                embedder,
                Some(steward.clone()),
            );

        for (i, ts_offset) in (0..3i64).enumerate() {
            let ep = ep_at(day_a + ts_offset * 1000, &format!("pa{i}"));
            handle
                .remember(ep, unit_emb(dim, &[(0, 1.0)]))
                .await
                .unwrap();
        }
        let r1 = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r1.clusters_built, 1);
        // v0.9.0 P4b: writer-actor no longer runs abstract_cluster
        // inline; the cluster row lands but the abstraction +
        // triples are deferred to the daemon-side background batch.
        assert_eq!(r1.abstractions_built, 0);
        assert_eq!(r1.abstractions_regenerated, 0);
        assert_eq!(r1.triples_built, 0);
        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();

        // ---- Run 2: absorb (no inline regen in v0.9.0 P4b)
        let conn2 = open_test_db_at(&path);
        let stub_idx2 = Arc::new(StubVectorIndex::new(dim));
        let embedder2: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
        let WriterSpawn {
            handle: handle2,
            join: join2,
        } = WriterActor::spawn_full_with_embedder_and_optional_steward(
            conn2,
            stub_idx2,
            tmp.path().to_path_buf(),
            embedder_id,
            embedder2,
            Some(steward.clone()),
        );

        for (i, ts_offset) in (0..3i64).enumerate() {
            let ep = ep_at(day_b + ts_offset * 1000, &format!("pb{i}"));
            handle2
                .remember(ep, unit_emb(dim, &[(0, 0.99), (1, 0.01)]))
                .await
                .unwrap();
        }
        let r2 = handle2
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(
            r2.clusters_built, 0,
            "run 2: absorbed, no fresh cluster row"
        );
        assert_eq!(r2.clusters_absorbed, 1);
        // v0.9.0 P4b: regen abstraction moved to daemon batch path.
        assert_eq!(r2.abstractions_built, 0);
        assert_eq!(r2.abstractions_regenerated, 0);
        assert_eq!(r2.triples_built, 0);
        drop(handle2);
        tokio::task::spawn_blocking(move || join2.join().unwrap())
            .await
            .unwrap();
    });

    // v0.9.0 P4b storage state: clusters persisted (absorb worked)
    // but no abstractions or triples written by the writer-actor —
    // both come from the daemon-side batch path.
    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 1, "still one cluster row");

    let abs_count: i64 = read
        .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        abs_count, 0,
        "v0.9.0 P4b: writer-actor doesn't write abstractions"
    );

    let n_triples: i64 = read
        .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        n_triples, 0,
        "v0.9.0 P4b: writer-actor doesn't write triples"
    );
}

/// Existing-vs-existing cluster merge: two pre-existing DB clusters
/// with similar centroids coalesce on the next consolidate. The
/// loser's `cluster_episodes` rows reassign to the survivor; the
/// loser's `clusters` row is DELETEd (cascading its abstraction +
/// triples); the survivor's centroid + coherence refresh; the
/// regen pass replaces the survivor's stale abstraction.
///
/// Because the cross-run absorb pass would normally fold a new
/// pasta cluster into an existing one (preventing the
/// "two-similar-existing-clusters" state from arising via the
/// public API), this test seeds the DB directly to construct the
/// scenario.
#[test]
fn consolidate_existing_vs_existing_merge_coalesces_drifted_clusters() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    // Seed: two existing clusters with similar centroids (cosine
    // ≈ 0.99). Cluster A has 5 episodes, B has 3 — A is the
    // survivor by "most episodes" rule. Episodes are seeded via
    // raw SQL with embeddings + cluster_episodes rows.
    let cluster_a_id = "00000000-0000-0000-0000-0000000000aa";
    let cluster_b_id = "00000000-0000-0000-0000-0000000000bb";
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Helper to make an embedding blob from a sparse vec.
    fn emb_bytes(dim: usize, components: &[(usize, f32)]) -> Vec<u8> {
        let mut v = vec![0.0f32; dim];
        for &(i, x) in components {
            v[i] = x;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        bytemuck::cast_slice(&v).to_vec()
    }

    let centroid_a = emb_bytes(dim, &[(0, 1.0)]);
    let centroid_b = emb_bytes(dim, &[(0, 0.99), (1, 0.01)]);

    {
        let conn = open_test_db_at(&path);
        // Cluster A
        conn.execute(
            "INSERT INTO clusters (cluster_id, centroid, centroid_dtype, centroid_dim, coherence, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
            params![cluster_a_id, centroid_a, dim as i64, 0.95, now_ms],
        )
        .unwrap();
        // Cluster B
        conn.execute(
            "INSERT INTO clusters (cluster_id, centroid, centroid_dtype, centroid_dim, coherence, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
            params![cluster_b_id, centroid_b, dim as i64, 0.93, now_ms],
        )
        .unwrap();
        // 5 episodes + embeddings + cluster_episodes for A.
        for i in 0..5 {
            let mid = format!("00000000-0000-0000-0000-00000000a{i:03}");
            conn.execute(
                "INSERT INTO episodes (memory_id, ts_ms, source_type, content, encoding_context_json, confidence, strength, salience, tier, created_at_ms, updated_at_ms) VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5, 'hot', ?, ?)",
                params![mid, now_ms - (5 - i as i64) * 1000, format!("a-ep-{i}"), now_ms, now_ms],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
                params![mid, embedder_id, dim as i64, &centroid_a, now_ms],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
                params![cluster_a_id, mid],
            )
            .unwrap();
        }
        // 3 episodes + embeddings + cluster_episodes for B.
        for i in 0..3 {
            let mid = format!("00000000-0000-0000-0000-00000000b{i:03}");
            conn.execute(
                "INSERT INTO episodes (memory_id, ts_ms, source_type, content, encoding_context_json, confidence, strength, salience, tier, created_at_ms, updated_at_ms) VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5, 'hot', ?, ?)",
                params![mid, now_ms - (3 - i as i64) * 1000, format!("b-ep-{i}"), now_ms, now_ms],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
                params![mid, embedder_id, dim as i64, &centroid_b, now_ms],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
                params![cluster_b_id, mid],
            )
            .unwrap();
        }
        // Pre-condition: 2 clusters, 8 cluster_episodes rows.
        let n_pre: i64 = conn
            .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pre, 2);
    }

    // The merge pass needs an LLM steward (regen replaces the
    // survivor's abstraction). Queue one canned response for the
    // regen call.
    let regen_response = r#"{
        "content": "Merged cluster (drifted A+B coalesced).",
        "confidence": 0.9,
        "triples": []
    }"#;
    let stub = Arc::new(StubLlmClient::with_canned("stub-llm", regen_response));
    let steward = Arc::new(Steward::new(stub.clone(), StewardConfig::default()));

    // The candidate-empty early return triggers if there are no
    // unclustered episodes — we'd skip the whole pipeline. Add one
    // dangling episode that won't cluster (size < min_size) so
    // candidates is non-empty.
    let runtime = rt_multi(2);
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub_idx = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder_and_optional_steward(
                conn,
                stub_idx,
                tmp.path().to_path_buf(),
                embedder_id,
                embedder,
                Some(steward.clone()),
            );

        // Single trigger episode with an unrelated centroid (dim 3)
        // — won't cluster (size 1 < min_size 3) and won't absorb
        // into A or B. Just bypasses the empty-candidates early
        // return so the merge pass downstream gets a chance.
        let trigger_ep = ep_at(now_ms + 1000, "trigger");
        handle
            .remember(trigger_ep, unit_emb(dim, &[(3, 1.0)]))
            .await
            .unwrap();

        let report = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        // The trigger doesn't cluster. 0 new built. 0 absorbed
        // (no new cluster to absorb).
        assert_eq!(report.clusters_built, 0);
        assert_eq!(report.clusters_absorbed, 0);
        // The merge fires: cluster_b absorbs into cluster_a.
        assert_eq!(
            report.existing_clusters_merged, 1,
            "expected one existing cluster absorbed into another"
        );
        // v0.9.0 P4b: regen abstraction moved to daemon batch path.
        assert_eq!(
            report.abstractions_regenerated, 0,
            "v0.9.0 P4b: regen abstraction moved to daemon batch path"
        );

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Final state: cluster_b deleted, all 8 of its+a's episodes
    // under cluster_a. v0.9.0 P4b: the survivor's regenerated
    // abstraction lands in the daemon-side background batch, not
    // here.
    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 1, "loser cluster row dropped");

    let surviving_id: String = read
        .query_row("SELECT cluster_id FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        surviving_id, cluster_a_id,
        "A (most episodes) must be the survivor"
    );

    let n_links: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM cluster_episodes WHERE cluster_id = ?",
            params![cluster_a_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_links, 8, "all 8 episodes now linked under cluster_a");

    // v0.9.0 P4b: writer-actor's consolidate no longer writes
    // semantic_abstractions rows; the merge persists structurally
    // but the regenerated abstraction comes from the daemon batch.
    let n_abs: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM semantic_abstractions WHERE cluster_id = ?",
            params![cluster_a_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        n_abs, 0,
        "v0.9.0 P4b: writer-actor doesn't write abstractions inline"
    );
}

/// `force_merge: true` runs the existing-vs-existing merge + regen
/// passes even with **zero unclustered candidates**. Sibling to
/// `consolidate_existing_vs_existing_merge_coalesces_drifted_clusters`,
/// but here we omit the trigger episode entirely. Without
/// force_merge the consolidate would early-return on empty
/// candidates and the drifted clusters would stay parallel
/// indefinitely.
#[test]
fn consolidate_force_merge_fires_with_no_candidates() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    // Seed: two pre-existing clusters with similar centroids,
    // mirroring the existing-vs-existing merge test.
    let cluster_a_id = "00000000-0000-0000-0000-0000000000aa";
    let cluster_b_id = "00000000-0000-0000-0000-0000000000bb";
    let now_ms = chrono::Utc::now().timestamp_millis();

    fn emb_bytes(dim: usize, components: &[(usize, f32)]) -> Vec<u8> {
        let mut v = vec![0.0f32; dim];
        for &(i, x) in components {
            v[i] = x;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        bytemuck::cast_slice(&v).to_vec()
    }

    let centroid_a = emb_bytes(dim, &[(0, 1.0)]);
    let centroid_b = emb_bytes(dim, &[(0, 0.99), (1, 0.01)]);

    {
        let conn = open_test_db_at(&path);
        conn.execute(
            "INSERT INTO clusters (cluster_id, centroid, centroid_dtype, centroid_dim, coherence, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
            params![cluster_a_id, centroid_a, dim as i64, 0.95, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO clusters (cluster_id, centroid, centroid_dtype, centroid_dim, coherence, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
            params![cluster_b_id, centroid_b, dim as i64, 0.93, now_ms],
        )
        .unwrap();
        for i in 0..5 {
            let mid = format!("00000000-0000-0000-0000-00000000a{i:03}");
            conn.execute(
                "INSERT INTO episodes (memory_id, ts_ms, source_type, content, encoding_context_json, confidence, strength, salience, tier, created_at_ms, updated_at_ms) VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5, 'hot', ?, ?)",
                params![mid, now_ms - (5 - i as i64) * 1000, format!("a-ep-{i}"), now_ms, now_ms],
            ).unwrap();
            conn.execute(
                "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
                params![mid, embedder_id, dim as i64, &centroid_a, now_ms],
            ).unwrap();
            conn.execute(
                "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
                params![cluster_a_id, mid],
            )
            .unwrap();
        }
        for i in 0..3 {
            let mid = format!("00000000-0000-0000-0000-00000000b{i:03}");
            conn.execute(
                "INSERT INTO episodes (memory_id, ts_ms, source_type, content, encoding_context_json, confidence, strength, salience, tier, created_at_ms, updated_at_ms) VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5, 'hot', ?, ?)",
                params![mid, now_ms - (3 - i as i64) * 1000, format!("b-ep-{i}"), now_ms, now_ms],
            ).unwrap();
            conn.execute(
                "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
                params![mid, embedder_id, dim as i64, &centroid_b, now_ms],
            ).unwrap();
            conn.execute(
                "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
                params![cluster_b_id, mid],
            )
            .unwrap();
        }
        // CRITICAL: also need to mark these episodes as already
        // clustered (they are — they're in cluster_episodes), so
        // the candidate SELECT sees zero candidates. The
        // `NOT IN cluster_episodes` filter handles this naturally.
    }

    // Seed-only: NO `handle.remember(...)` call. The candidate
    // SELECT will return 0 rows because every episode in the DB
    // is already linked via cluster_episodes (ergo NOT IN…).
    let regen_response = r#"{
        "content": "force-merged.",
        "confidence": 0.9,
        "triples": []
    }"#;
    let stub = Arc::new(StubLlmClient::with_canned("stub-llm", regen_response));
    let steward = Arc::new(Steward::new(stub.clone(), StewardConfig::default()));

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub_idx = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder_and_optional_steward(
                conn,
                stub_idx,
                tmp.path().to_path_buf(),
                embedder_id,
                embedder,
                Some(steward.clone()),
            );

        let report = handle
            .consolidate(ConsolidationScope {
                window_days: None,
                force_merge: true,
            })
            .await
            .unwrap();

        // Zero new candidates — but the merge fires anyway.
        assert_eq!(report.episodes_seen, 0, "no unclustered episodes to feed");
        assert_eq!(report.clusters_built, 0);
        assert_eq!(report.clusters_absorbed, 0);
        assert_eq!(
            report.existing_clusters_merged, 1,
            "force_merge=true should still trigger existing-vs-existing merge"
        );
        // v0.9.0 P4b: writer-actor no longer runs the regen
        // abstraction step inline; the merge persists (loser deleted,
        // survivor's centroid + coherence updated) but the survivor's
        // fresh abstraction lands in the daemon-side background batch.
        assert_eq!(
            report.abstractions_regenerated, 0,
            "v0.9.0 P4b: regen abstraction moved to daemon batch path"
        );

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Final state: one cluster (A absorbed B); 8 episodes under A.
    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 1);
    let n_links: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM cluster_episodes WHERE cluster_id = ?",
            params![cluster_a_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_links, 8);
}

/// Negative control: force_merge=false (default) on the same DB
/// state DOESN'T fire the merge — the empty-candidates early
/// return still applies. Confirms force_merge is the gating flag,
/// not a side effect of the seeded state.
#[test]
fn consolidate_no_force_merge_skips_merge_on_empty_candidates() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    // Same seed as above: 2 similar-centroid clusters that *would*
    // merge if force_merge were on.
    let cluster_a_id = "00000000-0000-0000-0000-0000000000aa";
    let cluster_b_id = "00000000-0000-0000-0000-0000000000bb";
    let now_ms = chrono::Utc::now().timestamp_millis();

    fn emb_bytes(dim: usize, components: &[(usize, f32)]) -> Vec<u8> {
        let mut v = vec![0.0f32; dim];
        for &(i, x) in components {
            v[i] = x;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        bytemuck::cast_slice(&v).to_vec()
    }

    let centroid_a = emb_bytes(dim, &[(0, 1.0)]);
    let centroid_b = emb_bytes(dim, &[(0, 0.99), (1, 0.01)]);
    {
        let conn = open_test_db_at(&path);
        conn.execute(
            "INSERT INTO clusters (cluster_id, centroid, centroid_dtype, centroid_dim, coherence, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
            params![cluster_a_id, centroid_a, dim as i64, 0.95, now_ms],
        ).unwrap();
        conn.execute(
            "INSERT INTO clusters (cluster_id, centroid, centroid_dtype, centroid_dim, coherence, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
            params![cluster_b_id, centroid_b, dim as i64, 0.93, now_ms],
        ).unwrap();
        for i in 0..3 {
            for (cluster, prefix, centroid) in &[
                (cluster_a_id, "a", &centroid_a),
                (cluster_b_id, "b", &centroid_b),
            ] {
                let mid = format!("00000000-0000-0000-0000-00000000{prefix}{i:03}");
                conn.execute(
                    "INSERT INTO episodes (memory_id, ts_ms, source_type, content, encoding_context_json, confidence, strength, salience, tier, created_at_ms, updated_at_ms) VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5, 'hot', ?, ?)",
                    params![mid, now_ms - (3 - i as i64) * 1000, format!("{prefix}-ep-{i}"), now_ms, now_ms],
                ).unwrap();
                conn.execute(
                    "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms) VALUES (?, ?, 'f32', ?, ?, ?)",
                    params![mid, embedder_id, dim as i64, *centroid, now_ms],
                ).unwrap();
                conn.execute(
                    "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
                    params![cluster, mid],
                )
                .unwrap();
            }
        }
    }

    let stub = Arc::new(StubLlmClient::default_stub());
    let steward = Arc::new(Steward::new(stub.clone(), StewardConfig::default()));

    let runtime = rt_multi(2);
    runtime.block_on(async {
        let conn = open_test_db_at(&path);
        let stub_idx = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
        let WriterSpawn { handle, join } =
            WriterActor::spawn_full_with_embedder_and_optional_steward(
                conn,
                stub_idx,
                tmp.path().to_path_buf(),
                embedder_id,
                embedder,
                Some(steward.clone()),
            );

        let report = handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();

        // Default scope = force_merge: false. Empty candidates →
        // early return. Drifted clusters stay parallel.
        assert_eq!(report.episodes_seen, 0);
        assert_eq!(report.existing_clusters_merged, 0);
        assert_eq!(report.abstractions_regenerated, 0);

        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 2, "no merge → both clusters still present");
}

/// Cross-run absorb edge case: when the new cluster's centroid is
/// orthogonal to all existing clusters', the new cluster is NOT
/// absorbed and lands as a fresh row.
#[test]
fn consolidate_cross_run_no_absorb_when_themes_unrelated() {
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::writer::ConsolidationScope;

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("test.db");
    let dim = 4usize;

    let embedder_id = {
        let conn = open_test_db_at(&path);
        get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: dim as u32,
                dtype: "f32".into(),
            },
        )
        .unwrap()
    };

    // Run 1: pasta theme (dim 0).
    let conn = open_test_db_at(&path);
    let stub = Arc::new(StubVectorIndex::new(dim));
    let WriterSpawn { handle, join } =
        WriterActor::spawn_full(conn, stub.clone(), tmp.path().to_path_buf(), embedder_id);
    let day_a = 1_700_000_000_000i64;
    let runtime = rt_multi(2);
    runtime.block_on(async {
        for (i, ts_offset) in (0..3i64).enumerate() {
            let ep = ep_at(day_a + ts_offset * 1000, &format!("pasta{i}"));
            handle
                .remember(ep, unit_emb(dim, &[(0, 1.0)]))
                .await
                .unwrap();
        }
        handle
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        drop(handle);
        tokio::task::spawn_blocking(move || join.join().unwrap())
            .await
            .unwrap();
    });

    // Run 2: completely unrelated theme (dim 2). Should NOT absorb.
    let conn2 = open_test_db_at(&path);
    let stub2 = Arc::new(StubVectorIndex::new(dim));
    let WriterSpawn {
        handle: handle2,
        join: join2,
    } = WriterActor::spawn_full(conn2, stub2.clone(), tmp.path().to_path_buf(), embedder_id);
    let day_b = day_a + 86_400_000 * 5;
    let runtime2 = rt_multi(2);
    runtime2.block_on(async {
        for (i, ts_offset) in (0..3i64).enumerate() {
            let ep = ep_at(day_b + ts_offset * 1000, &format!("rust{i}"));
            handle2
                .remember(ep, unit_emb(dim, &[(2, 1.0)]))
                .await
                .unwrap();
        }
        let r = handle2
            .consolidate(ConsolidationScope::default())
            .await
            .unwrap();
        assert_eq!(r.clusters_built, 1, "fresh unrelated cluster persisted");
        assert_eq!(
            r.clusters_absorbed, 0,
            "no absorb when themes are orthogonal"
        );
        drop(handle2);
        tokio::task::spawn_blocking(move || join2.join().unwrap())
            .await
            .unwrap();
    });

    let read = open_test_db_at(&path);
    let n_clusters: i64 = read
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_clusters, 2);
    let n_links: i64 = read
        .query_row("SELECT COUNT(*) FROM cluster_episodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_links, 6);
}

/// Without an embedder/runtime/embedder_id, handle_reembed returns a
/// clear error rather than panicking.
#[test]
fn reembed_without_embedder_returns_clear_error() {
    use crate::writer::ReembedScope;
    let (conn, _tmp) = open_test_db();
    let stub = Arc::new(StubVectorIndex::new(4));
    // spawn_full provides embedder_id but no embedder/handle.
    let WriterSpawn { handle, join: _ } =
        WriterActor::spawn_full(conn, stub, std::env::temp_dir(), 1);

    let runtime = rt_multi(1);
    let err = runtime
        .block_on(async { handle.reembed(ReembedScope::default()).await })
        .unwrap_err();
    assert!(
        err.to_string().contains("spawn_full_with_embedder"),
        "expected guidance pointing at the right constructor; got: {err}"
    );
}
