// SPDX-License-Identifier: Apache-2.0

//! Read-side queries for the **derived layer** — the Steward's outputs
//! (`semantic_abstractions`, `triples`, `contradictions`) that, until
//! v0.4.0, were written by `solo consolidate` but had no first-class
//! reader.
//!
//! Pipelines, each one (or a small set) a single SQL query against the
//! existing tables (no new schema, no migration):
//!
//!   - [`themes`] — list cluster abstractions, optionally windowed
//!     by recency. Surfaces "what has the user been thinking about
//!     lately" to agents that previously could only retrieve raw
//!     episodes.
//!   - [`facts_about`] — query the triple knowledge graph by
//!     subject + optional predicate + optional time window. The
//!     SPO shape lets agents ground answers on structured facts the
//!     Steward extracted from clusters.
//!   - [`contradictions`] — list pairs of conflicting triples the
//!     Steward flagged (rule-filter + LLM judge), with each side's
//!     triple LEFT JOIN'd in for readable context.
//!   - [`inspect_cluster`] — drill into a single cluster: its row,
//!     its (optional) abstraction, and its source episodes with
//!     content truncated by default. Lets agents answer "what's
//!     actually in cluster X" without round-tripping per-episode
//!     `memory_inspect` calls. Added v0.5.0.
//!
//! All four are pure read-only via [`ReaderPool`]. They never write,
//! never invoke the LLM, never load the embedder. The MCP and HTTP
//! transports in `solo-api` wrap each one as a tool / endpoint.

use serde::Serialize;
use solo_core::{Error, Result};
use solo_storage::{
    AuditOperation, AuditWriter, ReaderPool, ResolveContradictionReport, WriteHandle,
    triple_quality,
};

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// One theme = one cluster (and its abstraction, if the Steward got far
/// enough to write one). Returned by [`themes`].
///
/// `abstraction_*` are `Option` because the cluster pass runs without
/// an LLM (writes the cluster row but not the abstraction); a Steward
/// failure mid-consolidate could also leave a cluster uncovered. For
/// agent UX, surface both fields and let the consumer decide whether
/// to skip uncovered clusters.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThemeHit {
    pub cluster_id: String,
    pub abstraction_id: Option<String>,
    pub abstraction_text: Option<String>,
    pub episode_count: i64,
    pub coherence: f32,
    pub created_at_ms: i64,
}

/// One triple from the SPO knowledge graph. Returned by [`facts_about`].
///
/// `cluster_id` is `Option` because the column was added in migration
/// 0002 and pre-0002 rows have NULL there. `valid_to_ms` is `Option`
/// because the bi-temporal model uses NULL to mean "still valid (open
/// upper bound)".
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FactHit {
    pub triple_id: String,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: String,
    pub valid_from_ms: i64,
    pub valid_to_ms: Option<i64>,
    pub confidence: f32,
    pub cluster_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryQualityAuditConfig {
    pub low_confidence_below: f32,
    pub low_coherence_below: f32,
    pub long_literal_chars: usize,
    pub sample_limit: usize,
}

impl Default for MemoryQualityAuditConfig {
    fn default() -> Self {
        Self {
            low_confidence_below: 0.85,
            low_coherence_below: 0.72,
            long_literal_chars: 70,
            sample_limit: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryQualityTotals {
    pub active_episodes: i64,
    pub clustered_episodes: i64,
    pub clusters: i64,
    pub abstractions: i64,
    pub active_triples: i64,
    pub entity_triples: i64,
    pub literal_triples: i64,
    pub triple_reviews_needs_review: i64,
    pub distinct_entities: i64,
    pub contradictions: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryQualityHealth {
    pub score: i64,
    pub grade: String,
    pub critical_issues: i64,
    pub warning_issues: i64,
    pub info_issues: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryQualityIssue {
    pub severity: String,
    pub code: String,
    pub count: i64,
    pub summary: String,
    pub samples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryQualityAliasGroup {
    pub canonical_key: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryQualityAuditReport {
    pub generated_at_ms: i64,
    pub config: MemoryQualityAuditConfig,
    pub totals: MemoryQualityTotals,
    pub health: MemoryQualityHealth,
    pub issues: Vec<MemoryQualityIssue>,
    pub alias_groups: Vec<MemoryQualityAliasGroup>,
}

/// One discovered entity-like identifier from the triples graph.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EntityHit {
    pub entity_id: String,
    pub subject_count: i64,
    pub object_count: i64,
    pub fact_count: i64,
    pub predicates: Vec<String>,
    pub match_score: i64,
}

/// Compact summary of a triple for embedding in a [`ContradictionHit`].
/// Skips `confidence` + `cluster_id` (caller can join again if they
/// need them) to keep the contradiction wire payload tight.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TripleSummary {
    pub triple_id: String,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: String,
    pub valid_from_ms: i64,
    pub valid_to_ms: Option<i64>,
}

/// One contradiction the Steward flagged. Returned by [`contradictions`].
///
/// `a_triple` / `b_triple` are LEFT JOIN'd from the `triples` table —
/// they're `Option` because the join misses if the underlying triple
/// has been deleted or status='superseded' since the contradiction was
/// detected. Most consumer UX will want to render the SPO of each
/// side; the join saves an extra round-trip.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ContradictionHit {
    pub a_id: String,
    pub b_id: String,
    pub kind: String,
    pub explanation: String,
    pub detected_at_ms: i64,
    pub status: String,
    pub resolved_at_ms: Option<i64>,
    pub resolution_note: Option<String>,
    pub winning_triple_id: Option<String>,
    pub a_triple: Option<TripleSummary>,
    pub b_triple: Option<TripleSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ContradictionResolution {
    pub a_id: String,
    pub b_id: String,
    pub kind: String,
    pub status: String,
    pub resolved_at_ms: Option<i64>,
    pub resolution_note: Option<String>,
    pub winning_triple_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Pipelines
// ---------------------------------------------------------------------------

/// Limit clamp shared by all three pipelines. Matches the convention
/// recall uses — caps at 100 to keep wire payloads bounded; minimum 1
/// because returning zero-on-purpose isn't useful (caller would just
/// not call).
fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, 100)
}

/// List recent cluster themes with their (optional) abstractions.
///
/// `window_days` filters on `clusters.created_at_ms` — `Some(7)` means
/// "themes from the last week"; `None` means no time filter (returns
/// up to `limit` most-recent across all time). Uses
/// `chrono::Utc::now()` for the cutoff anchor — same time source the
/// rest of Solo writes with, so window math stays consistent.
///
/// Ordered by cluster `created_at_ms` descending (newest first).
/// v0.8.0 P4 audit-aware wrapper around [`themes_inner`]. Emits an
/// `audit_events` row after returning. See [`crate::recall::run_recall`]
/// for the conventions.
pub async fn themes(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    window_days: Option<i64>,
    limit: usize,
) -> Result<Vec<ThemeHit>> {
    let result = themes_inner(pool, window_days, limit).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryThemes, None),
        Err(e) => audit.emit_error(audit_principal, AuditOperation::MemoryThemes, None, e),
    }
    result
}

#[doc(hidden)]
pub async fn themes_inner(
    pool: &ReaderPool,
    window_days: Option<i64>,
    limit: usize,
) -> Result<Vec<ThemeHit>> {
    let limit = clamp_limit(limit);
    let cutoff_ms: Option<i64> = window_days.and_then(|days| {
        const MS_PER_DAY: i64 = 86_400_000;
        let now_ms = chrono::Utc::now().timestamp_millis();
        days.checked_mul(MS_PER_DAY).map(|w| now_ms - w)
    });

    pool.interact(move |conn| {
        // Dev-log 0152 M5: COUNT only active episodes. The previous
        // SUBQUERY counted every `cluster_episodes` row regardless of
        // the underlying episode's status, so clusters whose members
        // were all soft-deleted (forget) still surfaced as themes with
        // a non-zero `episode_count`. CASCADE FK doesn't help because
        // forget is a soft-delete, not a DELETE. Filtering here keeps
        // the count honest without touching the schema.
        let mut stmt = conn.prepare(
            "SELECT
                 c.cluster_id,
                 sa.abstraction_id,
                 sa.content,
                 (SELECT COUNT(*)
                    FROM cluster_episodes ce
                    JOIN episodes e ON e.memory_id = ce.memory_id
                   WHERE ce.cluster_id = c.cluster_id
                     AND e.status = 'active') AS episode_count,
                 c.coherence,
                 c.created_at_ms
               FROM clusters c
               LEFT JOIN semantic_abstractions sa
                      ON sa.cluster_id = c.cluster_id
              WHERE (?1 IS NULL OR c.created_at_ms >= ?1)
              ORDER BY c.created_at_ms DESC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![cutoff_ms, limit as i64], |r| {
            Ok(ThemeHit {
                cluster_id: r.get(0)?,
                abstraction_id: r.get(1)?,
                abstraction_text: r.get(2)?,
                episode_count: r.get(3)?,
                coherence: r.get(4)?,
                created_at_ms: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .await
}

/// Canonical subject id the Steward writes when no named entity is
/// present in a cluster. v0.5.0 Priority 1 sub-step 1A's prompt change
/// teaches the Steward to prefer named entities; this constant names
/// the legacy fallback so the read path can bridge historical rows.
pub const CANONICAL_USER_SUBJECT: &str = "user";

/// Query the triple knowledge graph by subject + optional predicate +
/// optional time window. All filters are exact-match (no LIKE, no
/// fuzzy). `subject_id` is required because predicate-only queries
/// would scan the whole table; if you really want that, add a separate
/// pipeline (deferred — no current consumer).
///
/// `since_ms` filters on `valid_from_ms >= since_ms`.
/// `until_ms` filters on `valid_to_ms IS NULL OR valid_to_ms <= until_ms`
/// (open-upper-bound triples are considered "still valid" and pass any
/// `until_ms` filter).
///
/// `user_aliases` is the read-path bridge for v0.5.0 Priority 1 sub-step
/// 1C. When the requested `subject` is exactly the canonical
/// [`CANONICAL_USER_SUBJECT`] or matches any entry in `user_aliases`,
/// the SQL `subject_id` filter expands to `IN (?, ?, …)` over the
/// canonical subject plus every alias. Symmetric: a query for either
/// side of the equivalence class returns the union of both. When the
/// requested subject does not match either, behaviour is identical to
/// today (single-equality filter). Pass `&[]` to disable expansion;
/// `"user"` itself still resolves on its own without an explicit alias
/// entry (callers don't have to add `"user"` to their list).
///
/// `include_as_object` (v0.5.1 Priority 8) widens the WHERE clause to
/// also match rows where the requested subject appears in `object_id`.
/// Default `false` preserves v0.5.0 behaviour (subject-position only).
/// When `true`, both sides expand symmetrically: with `user_aliases =
/// ["alex"]` and `subject = "user"`, the filter becomes
/// `subject_id IN ("user", "alex") OR object_id IN ("user", "alex")`.
/// `SELECT DISTINCT` dedupes self-loop rows (where subject_id ==
/// object_id) and the rare case where both sides of an alias-expanded
/// equivalence class match. Index-covered: `idx_triples_object` (in
/// migration 0001) makes the object-position scan cheap.
///
/// Skips `status != 'active'` rows. Ordered by `valid_from_ms`
/// descending (most-recent fact first).
/// v0.8.0 P4 audit-aware wrapper around [`facts_about_inner`]. Emits an
/// `audit_events` row after returning.
#[allow(clippy::too_many_arguments)]
pub async fn facts_about(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    subject: &str,
    user_aliases: &[String],
    include_as_object: bool,
    predicate: Option<&str>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    limit: usize,
) -> Result<Vec<FactHit>> {
    let target = Some(subject.to_string());
    let result = facts_about_inner(
        pool,
        subject,
        user_aliases,
        include_as_object,
        predicate,
        since_ms,
        until_ms,
        limit,
    )
    .await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryFactsAbout, target),
        Err(e) => audit.emit_error(audit_principal, AuditOperation::MemoryFactsAbout, target, e),
    }
    result
}

pub async fn entities(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    query: &str,
    limit: usize,
) -> Result<Vec<EntityHit>> {
    let result = entities_inner(pool, query, limit).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryEntities, None),
        Err(e) => audit.emit_error(audit_principal, AuditOperation::MemoryEntities, None, e),
    }
    result
}

pub async fn memory_quality_audit(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    config: MemoryQualityAuditConfig,
) -> Result<MemoryQualityAuditReport> {
    let result = memory_quality_audit_inner(pool, config).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryQualityAudit, None),
        Err(e) => audit.emit_error(audit_principal, AuditOperation::MemoryQualityAudit, None, e),
    }
    result
}

#[doc(hidden)]
pub async fn entities_inner(
    pool: &ReaderPool,
    query: &str,
    limit: usize,
) -> Result<Vec<EntityHit>> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::invalid_input("entity query must not be empty"));
    }
    let limit = clamp_limit(limit);
    let exact = query.to_lowercase();
    let prefix = format!("{exact}%");
    let contains = format!("%{exact}%");

    pool.interact(move |conn| {
        let mut stmt = conn.prepare(
            "WITH entity_refs AS (
                SELECT subject_id AS entity_id, 1 AS subject_hit, 0 AS object_hit, predicate
                  FROM triples
                 WHERE status = 'active'
                UNION ALL
                SELECT object_id AS entity_id, 0 AS subject_hit, 1 AS object_hit, predicate
                  FROM triples
                 WHERE status = 'active'
                   AND object_kind = 'entity'
             )
             SELECT entity_id,
                    SUM(subject_hit) AS subject_count,
                    SUM(object_hit) AS object_count,
                    COUNT(*) AS fact_count,
                    group_concat(DISTINCT predicate) AS predicates,
                    MIN(CASE
                        WHEN lower(entity_id) = ?1 THEN 0
                        WHEN lower(entity_id) LIKE ?2 THEN 1
                        ELSE 2
                    END) AS match_score
               FROM entity_refs
              WHERE lower(entity_id) LIKE ?3
              GROUP BY entity_id
              ORDER BY match_score ASC, fact_count DESC, entity_id ASC
              LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![exact, prefix, contains, limit as i64],
            |r| {
                let predicates: Option<String> = r.get(4)?;
                Ok(EntityHit {
                    entity_id: r.get(0)?,
                    subject_count: r.get(1)?,
                    object_count: r.get(2)?,
                    fact_count: r.get(3)?,
                    predicates: predicates
                        .unwrap_or_default()
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                    match_score: r.get(5)?,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .await
}

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn facts_about_inner(
    pool: &ReaderPool,
    subject: &str,
    user_aliases: &[String],
    include_as_object: bool,
    predicate: Option<&str>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    limit: usize,
) -> Result<Vec<FactHit>> {
    let limit = clamp_limit(limit);
    let subject_owned = subject.to_string();
    let predicate = predicate.map(|s| s.to_string());

    // Build the subject-filter set. If the query subject is the
    // canonical "user" or any configured alias, expand to the union
    // {"user", ...aliases}; otherwise pass through the single subject
    // exactly. Always parameterised — never string-concatenated.
    let is_user_class =
        subject_owned == CANONICAL_USER_SUBJECT || user_aliases.iter().any(|a| a == &subject_owned);
    let subjects: Vec<String> = if is_user_class {
        let mut out = Vec::with_capacity(user_aliases.len() + 1);
        out.push(CANONICAL_USER_SUBJECT.to_string());
        for a in user_aliases {
            // Skip empties (defensive — a stray empty string in the
            // alias config would otherwise widen the filter to all
            // empty-subject rows, which shouldn't exist post-1B but
            // costs nothing to guard).
            if !a.is_empty() && a != CANONICAL_USER_SUBJECT {
                out.push(a.clone());
            }
        }
        out
    } else {
        vec![subject_owned]
    };

    pool.interact(move |conn| {
        // Build the placeholder list for the IN clause: `?1, ?2, …, ?N`.
        // `subjects` is guaranteed non-empty (it always contains at
        // least the queried subject or "user").
        let placeholders = (1..=subjects.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let n = subjects.len();
        // ?N+1 = predicate, ?N+2 = since_ms, ?N+3 = until_ms, ?N+4 = limit.
        //
        // When `include_as_object` is true, widen the subject filter to
        // also match rows where the queried subject appears as the
        // object — reusing the SAME positional params `?1..?N` on both
        // sides of the OR (SQLite allows positional-param reuse, so we
        // bind once and reference twice). `SELECT DISTINCT` dedupes
        // self-loop rows (subject_id == object_id == queried) and the
        // alias-expansion edge case where the same row matches via
        // both sides of the equivalence class. Index-covered by
        // `idx_triples_object` (object_id, object_kind) from migration
        // 0001.
        let subject_filter = if include_as_object {
            format!("(subject_id IN ({placeholders}) OR object_id IN ({placeholders}))")
        } else {
            format!("subject_id IN ({placeholders})")
        };
        let sql = format!(
            "SELECT DISTINCT
                 triple_id, subject_id, predicate, object_id, object_kind,
                 valid_from_ms, valid_to_ms, confidence, cluster_id
               FROM triples
              WHERE status = 'active'
                AND {subject_filter}
                AND (?{p_pred} IS NULL OR predicate = ?{p_pred})
                AND (?{p_since} IS NULL OR valid_from_ms >= ?{p_since})
                AND (?{p_until} IS NULL OR valid_to_ms IS NULL OR valid_to_ms <= ?{p_until})
              ORDER BY valid_from_ms DESC
              LIMIT ?{p_lim}",
            p_pred = n + 1,
            p_since = n + 2,
            p_until = n + 3,
            p_lim = n + 4,
        );
        let mut stmt = conn.prepare(&sql)?;
        // Build the param list dynamically. `subjects` are bound as
        // owned strings; tail params bind predicate/since/until/limit.
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(subjects.len() + 4);
        for s in &subjects {
            params.push(Box::new(s.clone()));
        }
        params.push(Box::new(predicate.clone()));
        params.push(Box::new(since_ms));
        params.push(Box::new(until_ms));
        params.push(Box::new(limit as i64));
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |r| {
                Ok(FactHit {
                    triple_id: r.get(0)?,
                    subject_id: r.get(1)?,
                    predicate: r.get(2)?,
                    object_id: r.get(3)?,
                    object_kind: r.get(4)?,
                    valid_from_ms: r.get(5)?,
                    valid_to_ms: r.get(6)?,
                    confidence: r.get(7)?,
                    cluster_id: r.get(8)?,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .await
}

#[derive(Debug, Clone)]
struct QualityTripleRow {
    triple_id: String,
    subject_id: String,
    predicate: String,
    object_id: String,
    object_kind: String,
    confidence: f32,
    source_episode_id: Option<i64>,
}

#[derive(Debug, Clone)]
struct QualityClusterRow {
    cluster_id: String,
    coherence: f32,
    abstraction_text: Option<String>,
    episode_count: i64,
}

#[derive(Debug, Default)]
struct EntityQualityStats {
    refs: i64,
    entity_edge_refs: i64,
}

#[doc(hidden)]
pub async fn memory_quality_audit_inner(
    pool: &ReaderPool,
    mut config: MemoryQualityAuditConfig,
) -> Result<MemoryQualityAuditReport> {
    if !(0.0..=1.0).contains(&config.low_confidence_below) {
        return Err(Error::invalid_input(
            "low_confidence_below must be between 0.0 and 1.0",
        ));
    }
    if !(0.0..=1.0).contains(&config.low_coherence_below) {
        return Err(Error::invalid_input(
            "low_coherence_below must be between 0.0 and 1.0",
        ));
    }
    config.long_literal_chars = config.long_literal_chars.clamp(20, 500);
    config.sample_limit = config.sample_limit.clamp(1, 50);
    let generated_at_ms = chrono::Utc::now().timestamp_millis();

    pool.interact(move |conn| {
        let active_episodes: i64 =
            conn.query_row("SELECT COUNT(*) FROM episodes WHERE status = 'active'", [], |r| {
                r.get(0)
            })?;
        let clustered_episodes: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT ce.memory_id)
               FROM cluster_episodes ce
               JOIN episodes e ON e.memory_id = ce.memory_id
              WHERE e.status = 'active'",
            [],
            |r| r.get(0),
        )?;
        let clusters_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))?;
        let abstractions_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
                r.get(0)
            })?;
        let contradictions_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM contradictions", [], |r| r.get(0))?;
        let triple_reviews_needs_review: i64 = conn.query_row(
            "SELECT COUNT(*) FROM triple_reviews WHERE status = 'needs_review'",
            [],
            |r| r.get(0),
        )?;

        let triples = fetch_quality_triples(conn)?;
        let clusters = fetch_quality_clusters(conn)?;
        let entity_triples = triples
            .iter()
            .filter(|t| t.object_kind == "entity")
            .count() as i64;
        let literal_triples = triples
            .iter()
            .filter(|t| t.object_kind == "literal")
            .count() as i64;

        let mut entity_stats: std::collections::BTreeMap<String, EntityQualityStats> =
            std::collections::BTreeMap::new();
        for t in &triples {
            entity_stats.entry(t.subject_id.clone()).or_default().refs += 1;
            if t.object_kind == "entity" {
                entity_stats.entry(t.object_id.clone()).or_default().refs += 1;
                entity_stats
                    .entry(t.subject_id.clone())
                    .or_default()
                    .entity_edge_refs += 1;
                entity_stats
                    .entry(t.object_id.clone())
                    .or_default()
                    .entity_edge_refs += 1;
            }
        }
        let distinct_entities = entity_stats.len() as i64;

        let totals = MemoryQualityTotals {
            active_episodes,
            clustered_episodes,
            clusters: clusters_count,
            abstractions: abstractions_count,
            active_triples: triples.len() as i64,
            entity_triples,
            literal_triples,
            triple_reviews_needs_review,
            distinct_entities,
            contradictions: contradictions_count,
        };

        let mut issues = Vec::new();
        let sample_limit = config.sample_limit;

        if totals.active_episodes > 0 && totals.clusters == 0 {
            push_quality_issue(
                &mut issues,
                "warning",
                "no_clusters",
                totals.active_episodes,
                "Active episodes exist, but no clusters have been built yet.",
                Vec::new(),
            );
        }
        if totals.triple_reviews_needs_review > 0 {
            push_quality_issue(
                &mut issues,
                "warning",
                "triple_review_queue",
                totals.triple_reviews_needs_review,
                "Some extracted facts were held back by the quality gate and need review.",
                fetch_review_samples(conn, sample_limit)?,
            );
        }
        if totals.active_triples == 0 {
            push_quality_issue(
                &mut issues,
                "info",
                "no_active_triples",
                0,
                "No active fact triples are available yet.",
                Vec::new(),
            );
        }

        let unclustered = totals
            .active_episodes
            .saturating_sub(totals.clustered_episodes);
        if unclustered > 0 {
            push_quality_issue(
                &mut issues,
                "info",
                "unclustered_episodes",
                unclustered,
                "Some active episodes are not currently attached to a cluster.",
                Vec::new(),
            );
        }

        let literal_ratio = if totals.active_triples > 0 {
            totals.literal_triples as f32 / totals.active_triples as f32
        } else {
            0.0
        };
        if totals.active_triples >= 10 && literal_ratio >= 0.70 {
            push_quality_issue(
                &mut issues,
                "warning",
                "literal_fact_dominance",
                totals.literal_triples,
                "Most extracted facts are literals, which limits relationship graph usefulness.",
                triples
                    .iter()
                    .filter(|t| t.object_kind == "literal")
                    .take(sample_limit)
                    .map(quality_triple_sample)
                    .collect(),
            );
        }

        let long_literals = triples
            .iter()
            .filter(|t| {
                t.object_kind == "literal" && t.object_id.chars().count() > config.long_literal_chars
            })
            .collect::<Vec<_>>();
        if !long_literals.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "long_literal_objects",
                long_literals.len() as i64,
                "Some literal fact objects are long blobs; they should usually become shorter claims or evidence text.",
                long_literals
                    .iter()
                    .take(sample_limit)
                    .map(|t| quality_triple_sample(t))
                    .collect(),
            );
        }

        let low_confidence = triples
            .iter()
            .filter(|t| t.confidence < config.low_confidence_below)
            .collect::<Vec<_>>();
        if !low_confidence.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "low_confidence_facts",
                low_confidence.len() as i64,
                "Some active facts fall below the configured confidence threshold.",
                low_confidence
                    .iter()
                    .take(sample_limit)
                    .map(|t| quality_triple_sample(t))
                    .collect(),
            );
        }

        let orphan_entity_triples = triples
            .iter()
            .filter(|t| t.object_kind == "entity" && t.source_episode_id.is_none())
            .collect::<Vec<_>>();
        if !orphan_entity_triples.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "entity_triples_without_source_episode",
                orphan_entity_triples.len() as i64,
                "Some entity relationship facts cannot be anchored to a source episode for graph display.",
                orphan_entity_triples
                    .iter()
                    .take(sample_limit)
                    .map(|t| quality_triple_sample(t))
                    .collect(),
            );
        }

        let literal_only_entities = entity_stats
            .iter()
            .filter(|(_, stats)| stats.entity_edge_refs == 0)
            .map(|(entity, stats)| format!("{entity} ({} fact refs)", stats.refs))
            .collect::<Vec<_>>();
        if !literal_only_entities.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "literal_only_entities",
                literal_only_entities.len() as i64,
                "Some entity nodes only have literal facts, so they appear weakly connected in the relationship graph.",
                literal_only_entities.into_iter().take(sample_limit).collect(),
            );
        }

        let alias_groups = quality_alias_groups(entity_stats.keys());
        if !alias_groups.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "possible_entity_aliases",
                alias_groups.len() as i64,
                "Some entity labels normalize to the same key and are likely aliases.",
                alias_groups
                    .iter()
                    .take(sample_limit)
                    .map(|g| g.labels.join(" | "))
                    .collect(),
            );
        }

        let path_entities = entity_stats
            .keys()
            .filter(|s| looks_like_path_or_url(s))
            .cloned()
            .collect::<Vec<_>>();
        if !path_entities.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "path_like_entities",
                path_entities.len() as i64,
                "Some file paths, branch paths, or URLs became entities; these are often better as metadata or literal evidence.",
                path_entities.into_iter().take(sample_limit).collect(),
            );
        }

        let commit_entities = entity_stats
            .keys()
            .filter(|s| looks_like_commit_ref(s))
            .cloned()
            .collect::<Vec<_>>();
        if !commit_entities.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "commit_like_entities",
                commit_entities.len() as i64,
                "Some commit hashes became standalone entities; they should usually be typed artifacts or literal evidence.",
                commit_entities.into_iter().take(sample_limit).collect(),
            );
        }

        let machine_entities = entity_stats
            .keys()
            .filter(|s| triple_quality::looks_like_machine_identifier(s))
            .cloned()
            .collect::<Vec<_>>();
        if !machine_entities.is_empty() {
            push_quality_issue(
                &mut issues,
                "info",
                "machine_identifier_entities",
                machine_entities.len() as i64,
                "Some entity labels are machine identifiers; review whether they need human display aliases.",
                machine_entities.into_iter().take(sample_limit).collect(),
            );
        }

        let low_coherence_clusters = clusters
            .iter()
            .filter(|c| c.coherence < config.low_coherence_below)
            .collect::<Vec<_>>();
        if !low_coherence_clusters.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "low_coherence_clusters",
                low_coherence_clusters.len() as i64,
                "Some clusters are below the configured coherence threshold and may mix topics.",
                low_coherence_clusters
                    .iter()
                    .take(sample_limit)
                    .map(|c| quality_cluster_sample(c))
                    .collect(),
            );
        }

        let missing_abstractions = clusters
            .iter()
            .filter(|c| c.episode_count > 0 && c.abstraction_text.is_none())
            .collect::<Vec<_>>();
        if !missing_abstractions.is_empty() {
            push_quality_issue(
                &mut issues,
                "warning",
                "clusters_missing_abstractions",
                missing_abstractions.len() as i64,
                "Some populated clusters do not have an abstraction yet.",
                missing_abstractions
                    .iter()
                    .take(sample_limit)
                    .map(|c| quality_cluster_sample(c))
                    .collect(),
            );
        }

        let low_signal_abstractions = clusters
            .iter()
            .filter(|c| {
                c.abstraction_text
                    .as_deref()
                    .map(solo_steward::abstraction::is_low_signal_abstraction)
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        if !low_signal_abstractions.is_empty() {
            push_quality_issue(
                &mut issues,
                "critical",
                "low_signal_abstractions",
                low_signal_abstractions.len() as i64,
                "Some abstractions look like assistant/refusal chatter instead of durable memory.",
                low_signal_abstractions
                    .iter()
                    .take(sample_limit)
                    .map(|c| quality_cluster_sample(c))
                    .collect(),
            );
        }

        let health = quality_health(&issues);
        Ok::<_, rusqlite::Error>(MemoryQualityAuditReport {
            generated_at_ms,
            config,
            totals,
            health,
            issues,
            alias_groups,
        })
    })
    .await
}

fn fetch_quality_triples(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<QualityTripleRow>> {
    let mut stmt = conn.prepare(
        "SELECT triple_id, subject_id, predicate, object_id, object_kind,
                confidence, source_episode_id
           FROM triples
          WHERE status = 'active'
          ORDER BY valid_from_ms DESC, triple_id ASC",
    )?;
    stmt.query_map([], |r| {
        Ok(QualityTripleRow {
            triple_id: r.get(0)?,
            subject_id: r.get(1)?,
            predicate: r.get(2)?,
            object_id: r.get(3)?,
            object_kind: r.get(4)?,
            confidence: r.get(5)?,
            source_episode_id: r.get(6)?,
        })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()
}

fn fetch_quality_clusters(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<QualityClusterRow>> {
    let mut stmt = conn.prepare(
        "SELECT c.cluster_id,
                c.coherence,
                sa.content,
                (SELECT COUNT(*)
                   FROM cluster_episodes ce
                   JOIN episodes e ON e.memory_id = ce.memory_id
                  WHERE ce.cluster_id = c.cluster_id
                    AND e.status = 'active') AS episode_count
           FROM clusters c
           LEFT JOIN semantic_abstractions sa ON sa.cluster_id = c.cluster_id
          ORDER BY c.created_at_ms DESC, c.cluster_id ASC",
    )?;
    stmt.query_map([], |r| {
        Ok(QualityClusterRow {
            cluster_id: r.get(0)?,
            coherence: r.get(1)?,
            abstraction_text: r.get(2)?,
            episode_count: r.get(3)?,
        })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()
}

fn fetch_review_samples(
    conn: &rusqlite::Connection,
    limit: usize,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT reason_code, subject_id, predicate, object_id
           FROM triple_reviews
          WHERE status = 'needs_review'
          ORDER BY created_at_ms DESC
          LIMIT ?1",
    )?;
    stmt.query_map(rusqlite::params![limit as i64], |r| {
        let reason_code: String = r.get(0)?;
        let subject_id: String = r.get(1)?;
        let predicate: String = r.get(2)?;
        let object_id: String = r.get(3)?;
        Ok(format!(
            "{reason_code}: {subject_id} {predicate} {object_id}"
        ))
    })?
    .collect::<rusqlite::Result<Vec<_>>>()
}

fn push_quality_issue(
    issues: &mut Vec<MemoryQualityIssue>,
    severity: &str,
    code: &str,
    count: i64,
    summary: &str,
    samples: Vec<String>,
) {
    issues.push(MemoryQualityIssue {
        severity: severity.to_string(),
        code: code.to_string(),
        count,
        summary: summary.to_string(),
        samples,
    });
}

fn quality_health(issues: &[MemoryQualityIssue]) -> MemoryQualityHealth {
    let critical_issues = issues.iter().filter(|i| i.severity == "critical").count() as i64;
    let warning_issues = issues.iter().filter(|i| i.severity == "warning").count() as i64;
    let info_issues = issues.iter().filter(|i| i.severity == "info").count() as i64;
    let score = (100 - critical_issues * 20 - warning_issues * 7 - info_issues * 2).clamp(0, 100);
    let grade = if score >= 90 {
        "excellent"
    } else if score >= 75 {
        "good"
    } else if score >= 60 {
        "needs_review"
    } else {
        "weak"
    };
    MemoryQualityHealth {
        score,
        grade: grade.to_string(),
        critical_issues,
        warning_issues,
        info_issues,
    }
}

fn quality_alias_groups<'a>(
    labels: impl Iterator<Item = &'a String>,
) -> Vec<MemoryQualityAliasGroup> {
    let mut groups: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for label in labels {
        if let Some(key) = normalized_entity_key(label) {
            groups.entry(key).or_default().insert(label.clone());
        }
    }
    groups
        .into_iter()
        .filter_map(|(canonical_key, labels)| {
            if labels.len() <= 1 {
                return None;
            }
            Some(MemoryQualityAliasGroup {
                canonical_key,
                labels: labels.into_iter().collect(),
            })
        })
        .collect()
}

fn normalized_entity_key(label: &str) -> Option<String> {
    let key = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect::<String>();
    (!key.is_empty()).then_some(key)
}

fn looks_like_path_or_url(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.contains(":\\")
        || lower.contains('\\')
        || lower.contains('/')
}

fn looks_like_commit_ref(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let value = lower
        .strip_prefix("commit_")
        .or_else(|| lower.strip_prefix("commit-"))
        .unwrap_or(&lower);
    (6..=40).contains(&value.len()) && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn quality_triple_sample(t: &QualityTripleRow) -> String {
    format!(
        "{} --{}--> {} [{} {:.2}] ({})",
        t.subject_id,
        t.predicate,
        truncate_for_quality_sample(&t.object_id, 120),
        t.object_kind,
        t.confidence,
        t.triple_id
    )
}

fn quality_cluster_sample(c: &QualityClusterRow) -> String {
    let text = c
        .abstraction_text
        .as_deref()
        .map(|s| truncate_for_quality_sample(s, 140))
        .unwrap_or_else(|| "no abstraction".to_string());
    format!(
        "{} coherence={:.3} episodes={} {}",
        c.cluster_id, c.coherence, c.episode_count, text
    )
}

fn truncate_for_quality_sample(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

/// List flagged contradictions, with each side's triple LEFT JOIN'd
/// in for readable context. Ordered by `detected_at_ms` descending
/// (most-recent flag first).
///
/// The schema (see migration 0001) does not track resolution state,
/// so there is no `unresolved_only` filter today — every flagged
/// contradiction is returned. A `resolution_at_ms` column + an
/// `unresolved` filter is a reasonable post-v0.4.0 addition once
/// real consumer feedback shows it's needed.
/// v0.8.0 P4 audit-aware wrapper around [`contradictions_inner`].
pub async fn contradictions(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    limit: usize,
) -> Result<Vec<ContradictionHit>> {
    let result = contradictions_inner(pool, limit).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryContradictions, None),
        Err(e) => audit.emit_error(
            audit_principal,
            AuditOperation::MemoryContradictions,
            None,
            e,
        ),
    }
    result
}

/// Mark a contradiction resolved / unresolved / reopened.
///
/// **v0.11.2 (dev-log 0152 H1)**: now routes through the writer actor.
/// The previous implementation used the reader pool directly, which
/// violated ADR-0003 (single-writer invariant) — multiple reader
/// connections could race on `UPDATE contradictions`, and the audit
/// row was emitted *outside* the transaction. The new path commits the
/// UPDATE + audit row atomically.
///
/// The `_audit` and `_pool` parameters are kept for API stability but
/// are unused — the writer actor owns its own audit emit and connection.
/// They will be removed in a future major bump.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_contradiction(
    write: &WriteHandle,
    _pool: &ReaderPool,
    _audit: &AuditWriter,
    audit_principal: Option<String>,
    a_id: &str,
    b_id: &str,
    kind: &str,
    status: &str,
    resolution_note: Option<&str>,
    winning_triple_id: Option<&str>,
) -> Result<ContradictionResolution> {
    let report = write
        .resolve_contradiction_as(
            audit_principal,
            a_id.to_string(),
            b_id.to_string(),
            kind.to_string(),
            status.to_string(),
            resolution_note.map(str::to_string),
            winning_triple_id.map(str::to_string),
        )
        .await?;
    Ok(report_to_resolution(report))
}

/// Convert the writer-actor's `ResolveContradictionReport` to the
/// read-side `ContradictionResolution` shape. Field-for-field copy;
/// kept as a function so the conversion lives in one place.
fn report_to_resolution(r: ResolveContradictionReport) -> ContradictionResolution {
    ContradictionResolution {
        a_id: r.a_id,
        b_id: r.b_id,
        kind: r.kind,
        status: r.status,
        resolved_at_ms: r.resolved_at_ms,
        resolution_note: r.resolution_note,
        winning_triple_id: r.winning_triple_id,
    }
}

/// Test-only direct-pool path. Production code paths must go through
/// the writer-actor variant [`resolve_contradiction`] (dev-log 0152 H1
/// restored ADR-0003 by routing the UPDATE + audit row through the
/// writer). Kept here so the lifecycle test
/// `resolve_contradiction_updates_lifecycle_fields` can exercise the
/// SQL-level update semantics without standing up a full writer-actor.
#[cfg(test)]
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn resolve_contradiction_inner(
    pool: &ReaderPool,
    a_id: &str,
    b_id: &str,
    kind: &str,
    status: &str,
    resolution_note: Option<&str>,
    winning_triple_id: Option<&str>,
) -> Result<ContradictionResolution> {
    let status = status.trim();
    if !matches!(status, "unresolved" | "resolved" | "reopened") {
        return Err(Error::invalid_input(
            "contradiction status must be unresolved, resolved, or reopened",
        ));
    }
    let a_id = a_id.to_string();
    let b_id = b_id.to_string();
    let kind = kind.to_string();
    let note = resolution_note
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let winning = winning_triple_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let resolved_at_ms = if status == "resolved" {
        Some(chrono::Utc::now().timestamp_millis())
    } else {
        None
    };
    let status = status.to_string();

    let resolution = pool
        .interact(move |conn| {
            let changed = conn.execute(
                "UPDATE contradictions
                SET status = ?4,
                    resolved_at_ms = ?5,
                    resolution_note = ?6,
                    winning_triple_id = ?7
              WHERE a_memory_id = ?1
                AND b_memory_id = ?2
                AND kind = ?3",
                rusqlite::params![a_id, b_id, kind, status, resolved_at_ms, note, winning],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            Ok(Some(ContradictionResolution {
                a_id,
                b_id,
                kind,
                status,
                resolved_at_ms,
                resolution_note: note,
                winning_triple_id: winning,
            }))
        })
        .await?;
    resolution.ok_or_else(|| Error::not_found("contradiction not found"))
}

#[doc(hidden)]
pub async fn contradictions_inner(
    pool: &ReaderPool,
    limit: usize,
) -> Result<Vec<ContradictionHit>> {
    let limit = clamp_limit(limit);

    pool.interact(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT
                 c.a_memory_id, c.b_memory_id, c.kind, c.explanation,
                 c.detected_at_ms, c.status, c.resolved_at_ms,
                 c.resolution_note, c.winning_triple_id,
                 a.triple_id, a.subject_id, a.predicate, a.object_id,
                     a.object_kind, a.valid_from_ms, a.valid_to_ms,
                 b.triple_id, b.subject_id, b.predicate, b.object_id,
                     b.object_kind, b.valid_from_ms, b.valid_to_ms
               FROM contradictions c
               LEFT JOIN triples a ON a.triple_id = c.a_memory_id
               LEFT JOIN triples b ON b.triple_id = c.b_memory_id
              ORDER BY c.detected_at_ms DESC
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |r| {
            // Helper to read an Option<TripleSummary> starting at column
            // `start`. The seven LEFT JOIN columns either ALL bind as
            // Some (join hit) or ALL bind as None (join miss). We probe
            // the first column (triple_id) to decide.
            let read_summary =
                |r: &rusqlite::Row, start: usize| -> rusqlite::Result<Option<TripleSummary>> {
                    let triple_id: Option<String> = r.get(start)?;
                    if let Some(triple_id) = triple_id {
                        Ok(Some(TripleSummary {
                            triple_id,
                            subject_id: r.get(start + 1)?,
                            predicate: r.get(start + 2)?,
                            object_id: r.get(start + 3)?,
                            object_kind: r.get(start + 4)?,
                            valid_from_ms: r.get(start + 5)?,
                            valid_to_ms: r.get(start + 6)?,
                        }))
                    } else {
                        Ok(None)
                    }
                };
            Ok(ContradictionHit {
                a_id: r.get(0)?,
                b_id: r.get(1)?,
                kind: r.get(2)?,
                explanation: r.get(3)?,
                detected_at_ms: r.get(4)?,
                status: r.get(5)?,
                resolved_at_ms: r.get(6)?,
                resolution_note: r.get(7)?,
                winning_triple_id: r.get(8)?,
                a_triple: read_summary(r, 9)?,
                b_triple: read_summary(r, 16)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .await
}

// ---------------------------------------------------------------------------
// inspect_cluster — drill into one cluster + its abstraction + its episodes
// ---------------------------------------------------------------------------

/// Default per-episode content cap in chars when `full_content=false`.
/// Total payload (truncated content + trailing ellipsis) is exactly
/// [`EPISODE_TRUNCATE_CHARS`] Unicode scalar values. Locked at 200 in
/// 0071-v0.5.x-roadmap; see also the per-episode truncation in
/// `solo_steward::abstraction::truncate` (500-char prompt window) for
/// the sibling cap inside the LLM path.
pub const EPISODE_TRUNCATE_CHARS: usize = 200;

/// One row from the `clusters` table, surfaced to callers of
/// [`inspect_cluster`]. Mirrors the columns directly; flat shape so the
/// MCP / HTTP wire encoding is unambiguous (no nested `solo_core::Cluster`
/// with UUID-newtype-shaped `MemoryId`s).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClusterRow {
    pub cluster_id: String,
    pub coherence: f32,
    pub created_at_ms: i64,
    /// `centroid_dim` from the schema. `Option` because the column is
    /// nullable in `clusters` (older / hand-seeded rows may not carry
    /// one). Surfacing it lets callers sanity-check the embedder
    /// generation behind a cluster without re-reading the BLOB.
    pub centroid_dim: Option<i64>,
}

/// One row from `semantic_abstractions` — the Steward's LLM-generated
/// summary of a cluster. `None` from [`inspect_cluster`] means either
/// (a) the cluster pass ran but the abstraction pass hadn't (no LLM
/// configured at consolidate time) or (b) the abstraction was dropped
/// for regeneration but hasn't been rewritten yet.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Abstraction {
    pub abstraction_id: String,
    pub content: String,
    pub confidence: f32,
    pub created_at_ms: i64,
}

/// One source episode belonging to a cluster, with its content optionally
/// truncated. The fields are a subset of [`crate::EpisodeRecord`] — we
/// skip the JSON blobs (encoding context, provenance) here because the
/// purpose of `inspect_cluster` is to give an agent a quick scan, not a
/// forensic dump. Callers wanting a full record can follow up with
/// `inspect_one(memory_id)` from `solo_query::inspect`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EpisodeSummary {
    pub memory_id: String,
    /// `content` either verbatim from the row (when `full_content=true`
    /// was passed) or truncated to [`EPISODE_TRUNCATE_CHARS`] Unicode
    /// scalars with a trailing `…` ellipsis. Truncation is char-aware
    /// (never splits UTF-8).
    pub content: String,
    pub ts_ms: i64,
    pub source_type: String,
    pub tier: String,
    pub status: String,
}

/// Bundle returned by [`inspect_cluster`]. All three lists (`cluster`,
/// `abstraction`, `episodes`) are point-in-time snapshots from a single
/// reader interaction; no atomicity guarantee across them is offered,
/// but since the reader holds one pool connection for the body of the
/// call, intra-call drift is bounded to anything a concurrent writer
/// commits between statements.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClusterRecord {
    pub cluster: ClusterRow,
    pub abstraction: Option<Abstraction>,
    pub episodes: Vec<EpisodeSummary>,
}

/// UTF-8-safe truncation. Takes up to `max - 1` Unicode scalar values
/// then pushes a single `'…'` to land at exactly `max` chars. Returns
/// the input verbatim when it's already within budget. Never splits a
/// codepoint (chars iterator, not bytes).
fn truncate_chars(s: &str, max: usize) -> String {
    debug_assert!(max >= 1, "max must leave room for at least the ellipsis");
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Inspect a single cluster: return its row, its abstraction (if any),
/// and its source episodes with content optionally truncated.
///
/// `full_content = false` (the common case for agents browsing): each
/// episode's `content` is truncated to [`EPISODE_TRUNCATE_CHARS`] chars
/// with a trailing `…`. UTF-8-safe (operates on Unicode scalars, never
/// byte-splits multi-byte codepoints).
///
/// `full_content = true`: each episode's `content` is returned verbatim.
/// Use when the caller has already narrowed to a specific cluster and
/// wants the raw episodes for a forensic answer.
///
/// Returns `Err(Error::NotFound)` when no `clusters.cluster_id` row
/// matches the requested id. Distinguished from "cluster exists but is
/// empty" — that case returns `Ok` with `episodes: vec![]`.
///
/// Skips forgotten episodes (`episodes.status != 'active'`). The
/// `cluster_episodes` table still references them via FK; we just don't
/// surface them in the snapshot.
///
/// Episode ordering: ascending by `ts_ms` (oldest event first), which
/// matches the reading order users expect when reviewing a thread.
/// v0.8.0 P4 audit-aware wrapper around [`inspect_cluster_inner`].
pub async fn inspect_cluster(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    cluster_id: &str,
    full_content: bool,
) -> Result<ClusterRecord> {
    let target = Some(cluster_id.to_string());
    let result = inspect_cluster_inner(pool, cluster_id, full_content).await;
    match &result {
        Ok(_) => audit.emit_ok(
            audit_principal,
            AuditOperation::MemoryInspectCluster,
            target,
        ),
        Err(e) => audit.emit_error(
            audit_principal,
            AuditOperation::MemoryInspectCluster,
            target,
            e,
        ),
    }
    result
}

#[doc(hidden)]
pub async fn inspect_cluster_inner(
    pool: &ReaderPool,
    cluster_id: &str,
    full_content: bool,
) -> Result<ClusterRecord> {
    let cluster_id_owned = cluster_id.to_string();
    let cluster_id_for_err = cluster_id_owned.clone();

    let record: Option<ClusterRecord> = pool
        .interact(move |conn| {
            // Step 1: the cluster row itself. Returning `None` from
            // the closure when no row matches keeps NotFound mapping
            // outside the closure (mirrors `inspect_one`'s pattern).
            let cluster_opt: Option<ClusterRow> = conn
                .query_row(
                    "SELECT cluster_id, coherence, created_at_ms, centroid_dim
                       FROM clusters
                      WHERE cluster_id = ?1",
                    rusqlite::params![&cluster_id_owned],
                    |r| {
                        Ok(ClusterRow {
                            cluster_id: r.get(0)?,
                            coherence: r.get(1)?,
                            created_at_ms: r.get(2)?,
                            centroid_dim: r.get(3)?,
                        })
                    },
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            let cluster = match cluster_opt {
                Some(c) => c,
                None => return Ok(None),
            };

            // Step 2: the (optional) abstraction. Schema permits
            // multiple rows per cluster_id over time, but the writer
            // treats it as 1:1 and the read path mirrors that by
            // taking the most-recent.
            let abstraction: Option<Abstraction> = conn
                .query_row(
                    "SELECT abstraction_id, content, confidence, created_at_ms
                       FROM semantic_abstractions
                      WHERE cluster_id = ?1
                      ORDER BY created_at_ms DESC
                      LIMIT 1",
                    rusqlite::params![&cluster.cluster_id],
                    |r| {
                        Ok(Abstraction {
                            abstraction_id: r.get(0)?,
                            content: r.get(1)?,
                            confidence: r.get(2)?,
                            created_at_ms: r.get(3)?,
                        })
                    },
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;

            // Step 3: source episodes. Join cluster_episodes →
            // episodes, filter to active rows, order by ts_ms
            // ascending (oldest first — matches conversational
            // reading order).
            let mut stmt = conn.prepare(
                "SELECT e.memory_id, e.content, e.ts_ms,
                        e.source_type, e.tier, e.status
                   FROM cluster_episodes ce
                   JOIN episodes e ON e.memory_id = ce.memory_id
                  WHERE ce.cluster_id = ?1
                    AND e.status = 'active'
                  ORDER BY e.ts_ms ASC",
            )?;
            let raw_rows = stmt.query_map(rusqlite::params![&cluster.cluster_id], |r| {
                let content: String = r.get(1)?;
                let content = if full_content {
                    content
                } else {
                    truncate_chars(&content, EPISODE_TRUNCATE_CHARS)
                };
                Ok(EpisodeSummary {
                    memory_id: r.get(0)?,
                    content,
                    ts_ms: r.get(2)?,
                    source_type: r.get(3)?,
                    tier: r.get(4)?,
                    status: r.get(5)?,
                })
            })?;
            let episodes: Vec<EpisodeSummary> = raw_rows.collect::<rusqlite::Result<Vec<_>>>()?;

            Ok(Some(ClusterRecord {
                cluster,
                abstraction,
                episodes,
            }))
        })
        .await?;
    record.ok_or_else(|| Error::not_found(format!("cluster_id {cluster_id_for_err} not found")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use solo_storage::{ReaderPool, test_support::open_test_db_at};

    /// Open a ReaderPool against a tempdir DB seeded by `seed`.
    /// Returns (pool, tempdir, db_path) — caller keeps tempdir alive.
    fn pool_with_seed(seed: impl FnOnce(&rusqlite::Connection)) -> (ReaderPool, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        // Apply migrations to a fresh DB.
        let conn = open_test_db_at(&db_path);
        seed(&conn);
        drop(conn);
        let pool = ReaderPool::new(&db_path, None, fake_hnsw()).expect("pool");
        (pool, tmp)
    }

    /// ReaderPool needs a VectorIndex even though derived queries
    /// don't touch it. Use the existing StubVectorIndex.
    fn fake_hnsw() -> std::sync::Arc<dyn solo_core::VectorIndex + Send + Sync> {
        std::sync::Arc::new(solo_storage::test_support::StubVectorIndex::new(1024))
    }

    /// Insert a cluster row directly. No FK to episodes — clusters are
    /// independent. Returns the cluster_id (ULIDish; tests use the
    /// param value verbatim for simpler assertions).
    fn seed_cluster(
        conn: &rusqlite::Connection,
        cluster_id: &str,
        coherence: f32,
        created_at_ms: i64,
    ) {
        conn.execute(
            "INSERT INTO clusters (cluster_id, coherence, created_at_ms)
                  VALUES (?1, ?2, ?3)",
            rusqlite::params![cluster_id, coherence, created_at_ms],
        )
        .expect("seed cluster");
    }

    fn seed_abstraction(
        conn: &rusqlite::Connection,
        abstraction_id: &str,
        cluster_id: &str,
        content: &str,
        created_at_ms: i64,
    ) {
        conn.execute(
            "INSERT INTO semantic_abstractions
                 (abstraction_id, cluster_id, content, provenance_json,
                  confidence, created_at_ms)
                 VALUES (?1, ?2, ?3, '{}', 1.0, ?4)",
            rusqlite::params![abstraction_id, cluster_id, content, created_at_ms],
        )
        .expect("seed abstraction");
    }

    fn seed_triple(
        conn: &rusqlite::Connection,
        triple_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        valid_from_ms: i64,
        cluster_id: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO triples
                 (triple_id, subject_id, predicate, object_id, object_kind,
                  valid_from_ms, valid_to_ms, confidence, provenance_json,
                  status, created_at_ms, updated_at_ms, cluster_id)
                 VALUES (?1, ?2, ?3, ?4, 'literal', ?5, NULL, 1.0, '{}',
                         'active', ?5, ?5, ?6)",
            rusqlite::params![
                triple_id,
                subject,
                predicate,
                object,
                valid_from_ms,
                cluster_id,
            ],
        )
        .expect("seed triple");
    }

    fn seed_entity_object_triple(
        conn: &rusqlite::Connection,
        triple_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        valid_from_ms: i64,
    ) {
        conn.execute(
            "INSERT INTO triples
                 (triple_id, subject_id, predicate, object_id, object_kind,
                  valid_from_ms, valid_to_ms, confidence, provenance_json,
                  status, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 'entity', ?5, NULL, 1.0, '{}',
                         'active', ?5, ?5)",
            rusqlite::params![triple_id, subject, predicate, object, valid_from_ms],
        )
        .expect("seed entity object triple");
    }

    fn seed_contradiction(
        conn: &rusqlite::Connection,
        a: &str,
        b: &str,
        kind: &str,
        explanation: &str,
        detected_at_ms: i64,
    ) {
        conn.execute(
            "INSERT INTO contradictions
                 (a_memory_id, b_memory_id, kind, explanation, detected_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![a, b, kind, explanation, detected_at_ms],
        )
        .expect("seed contradiction");
    }

    fn seed_cluster_episode(conn: &rusqlite::Connection, cluster_id: &str, memory_id: &str) {
        // cluster_episodes FK references episodes, but for derived
        // queries we only need the count; insert a stub episode row
        // first to satisfy the FK.
        conn.execute(
            "INSERT OR IGNORE INTO episodes
                 (memory_id, ts_ms, source_type, content,
                  encoding_context_json, tier, status,
                  confidence, strength, salience,
                  created_at_ms, updated_at_ms)
                 VALUES (?1, 0, 'user_message', 'stub',
                         '{}', 'hot', 'active',
                         1.0, 1.0, 1.0, 0, 0)",
            rusqlite::params![memory_id],
        )
        .expect("seed stub episode");
        conn.execute(
            "INSERT INTO cluster_episodes (cluster_id, memory_id)
                 VALUES (?1, ?2)",
            rusqlite::params![cluster_id, memory_id],
        )
        .expect("seed cluster_episode");
    }

    // ---- themes ----

    #[tokio::test]
    async fn themes_returns_empty_on_empty_db() {
        let (pool, _tmp) = pool_with_seed(|_| {});
        let hits = themes_inner(&pool, None, 10).await.expect("ok");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn themes_orders_by_created_at_desc() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c-old", 0.5, 1_000);
            seed_cluster(conn, "c-mid", 0.6, 2_000);
            seed_cluster(conn, "c-new", 0.7, 3_000);
        });
        let hits = themes_inner(&pool, None, 10).await.expect("ok");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].cluster_id, "c-new");
        assert_eq!(hits[1].cluster_id, "c-mid");
        assert_eq!(hits[2].cluster_id, "c-old");
    }

    #[tokio::test]
    async fn themes_join_brings_abstraction_when_present() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c1", 0.5, 1_000);
            seed_abstraction(conn, "a1", "c1", "Sam works at Stripe in Berlin", 1_500);
            // Cluster with no abstraction
            seed_cluster(conn, "c2", 0.5, 2_000);
        });
        let hits = themes_inner(&pool, None, 10).await.expect("ok");
        assert_eq!(hits.len(), 2);
        let h_c2 = hits.iter().find(|h| h.cluster_id == "c2").unwrap();
        assert_eq!(h_c2.abstraction_id, None);
        assert_eq!(h_c2.abstraction_text, None);
        let h_c1 = hits.iter().find(|h| h.cluster_id == "c1").unwrap();
        assert_eq!(h_c1.abstraction_id.as_deref(), Some("a1"));
        assert_eq!(
            h_c1.abstraction_text.as_deref(),
            Some("Sam works at Stripe in Berlin"),
        );
    }

    #[tokio::test]
    async fn themes_episode_count_is_subquery_correct() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c1", 0.5, 1_000);
            for i in 0..3 {
                seed_cluster_episode(conn, "c1", &format!("ep{i}"));
            }
            seed_cluster(conn, "c2", 0.5, 2_000);
            // c2 has no episodes
        });
        let hits = themes_inner(&pool, None, 10).await.expect("ok");
        let h_c1 = hits.iter().find(|h| h.cluster_id == "c1").unwrap();
        let h_c2 = hits.iter().find(|h| h.cluster_id == "c2").unwrap();
        assert_eq!(h_c1.episode_count, 3);
        assert_eq!(h_c2.episode_count, 0);
    }

    #[tokio::test]
    async fn themes_window_filter_excludes_old_clusters() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let day_ms: i64 = 86_400_000;
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c-recent", 0.5, now_ms - day_ms);
            seed_cluster(conn, "c-week-ago", 0.5, now_ms - 7 * day_ms);
            seed_cluster(conn, "c-month-ago", 0.5, now_ms - 30 * day_ms);
        });
        // Window 3 days → only c-recent matches
        let hits = themes_inner(&pool, Some(3), 10).await.expect("ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].cluster_id, "c-recent");
        // Window 14 days → c-recent + c-week-ago
        let hits = themes_inner(&pool, Some(14), 10).await.expect("ok");
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn themes_limit_is_clamped_and_honored() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            for i in 0..5 {
                seed_cluster(conn, &format!("c{i}"), 0.5, i as i64);
            }
        });
        let hits = themes_inner(&pool, None, 3).await.expect("ok");
        assert_eq!(hits.len(), 3);
        // limit > 100 clamps; 200 -> 100 (still <= 5 in the DB)
        let hits = themes_inner(&pool, None, 200).await.expect("ok");
        assert_eq!(hits.len(), 5);
    }

    // ---- facts_about ----

    #[tokio::test]
    async fn facts_about_filters_by_subject() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t1", "Sam", "lives_in", "Berlin", 1_000, None);
            seed_triple(conn, "t2", "Alex", "lives_in", "Tokyo", 2_000, None);
            seed_triple(conn, "t3", "Sam", "works_at", "Stripe", 3_000, None);
        });
        let hits = facts_about_inner(&pool, "Sam", &[], false, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.subject_id == "Sam"));
    }

    #[tokio::test]
    async fn facts_about_filters_by_predicate_when_given() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t1", "Sam", "lives_in", "Berlin", 1_000, None);
            seed_triple(conn, "t2", "Sam", "works_at", "Stripe", 2_000, None);
        });
        let hits = facts_about_inner(&pool, "Sam", &[], false, Some("works_at"), None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].predicate, "works_at");
        assert_eq!(hits[0].object_id, "Stripe");
    }

    #[tokio::test]
    async fn entities_discovers_subjects_and_entity_objects() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t1", "Quotient", "status", "launching", 1_000, None);
            seed_triple(conn, "t2", "Quotient", "owner", "Alex", 2_000, None);
            seed_entity_object_triple(conn, "t3", "Roadmap", "mentions", "Quotient", 3_000);
        });
        let hits = entities_inner(&pool, "quot", 10).await.expect("ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, "Quotient");
        assert_eq!(hits[0].subject_count, 2);
        assert_eq!(hits[0].object_count, 1);
        assert_eq!(hits[0].fact_count, 3);
        assert!(hits[0].predicates.contains(&"status".to_string()));
        assert!(hits[0].predicates.contains(&"mentions".to_string()));
    }

    #[tokio::test]
    async fn facts_about_skips_non_active_status() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t1", "Sam", "lives_in", "Berlin", 1_000, None);
            // Mark t1 as superseded
            conn.execute(
                "UPDATE triples SET status = 'superseded' WHERE triple_id = ?1",
                rusqlite::params!["t1"],
            )
            .expect("update");
            seed_triple(conn, "t2", "Sam", "works_at", "Stripe", 2_000, None);
        });
        let hits = facts_about_inner(&pool, "Sam", &[], false, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].triple_id, "t2");
    }

    #[tokio::test]
    async fn facts_about_orders_by_valid_from_ms_desc() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t-old", "Sam", "p", "v1", 1_000, None);
            seed_triple(conn, "t-mid", "Sam", "p", "v2", 2_000, None);
            seed_triple(conn, "t-new", "Sam", "p", "v3", 3_000, None);
        });
        let hits = facts_about_inner(&pool, "Sam", &[], false, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].triple_id, "t-new");
        assert_eq!(hits[2].triple_id, "t-old");
    }

    #[tokio::test]
    async fn facts_about_since_ms_filters_lower_bound() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t1", "Sam", "p", "v", 1_000, None);
            seed_triple(conn, "t2", "Sam", "p", "v", 2_000, None);
            seed_triple(conn, "t3", "Sam", "p", "v", 3_000, None);
        });
        let hits = facts_about_inner(&pool, "Sam", &[], false, None, Some(2_000), None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 2); // t2 + t3
    }

    #[tokio::test]
    async fn facts_about_carries_cluster_id_when_present() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c1", 0.5, 1_000);
            seed_triple(conn, "t1", "Sam", "p", "v", 1_500, Some("c1"));
            seed_triple(conn, "t2", "Sam", "p", "v", 2_500, None);
        });
        let hits = facts_about_inner(&pool, "Sam", &[], false, None, None, None, 10)
            .await
            .expect("ok");
        let t1 = hits.iter().find(|h| h.triple_id == "t1").unwrap();
        let t2 = hits.iter().find(|h| h.triple_id == "t2").unwrap();
        assert_eq!(t1.cluster_id.as_deref(), Some("c1"));
        assert_eq!(t2.cluster_id, None);
    }

    // ---- facts_about user-alias resolution (v0.5.0 sub-step 1C) ----

    #[tokio::test]
    async fn facts_about_alias_resolution_bridges_user_and_alias() {
        // Setup: one row written historically with subject="user"
        // (pre-1A extraction), one row written with subject="alex"
        // (post-1A or hand-seeded). With alias config user_aliases=["alex"],
        // a query for either subject must surface BOTH triples.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(
                conn,
                "t-user",
                "user",
                "started_at",
                "quotient_in_2024",
                1_000,
                None,
            );
            seed_triple(conn, "t-alex", "alex", "prefers", "rust", 2_000, None);
            // Unrelated subject — must not surface.
            seed_triple(conn, "t-bob", "bob", "prefers", "go", 3_000, None);
        });

        let aliases = vec!["alex".to_string()];

        // Query "user" → both user + alex rows.
        let hits = facts_about_inner(&pool, "user", &aliases, false, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 2, "user query should also see alex rows");
        let ids: Vec<&str> = hits.iter().map(|h| h.triple_id.as_str()).collect();
        assert!(ids.contains(&"t-user"));
        assert!(ids.contains(&"t-alex"));

        // Symmetric: query "alex" → same union.
        let hits = facts_about_inner(&pool, "alex", &aliases, false, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 2, "alex query should also see user rows");
        let ids: Vec<&str> = hits.iter().map(|h| h.triple_id.as_str()).collect();
        assert!(ids.contains(&"t-user"));
        assert!(ids.contains(&"t-alex"));

        // Non-matching subject behaves as before — exact equality only.
        let hits = facts_about_inner(&pool, "bob", &aliases, false, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].triple_id, "t-bob");

        // Empty aliases + "user" subject still resolves the canonical
        // "user" row but does NOT widen to alex.
        let hits = facts_about_inner(&pool, "user", &[], false, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].triple_id, "t-user");
    }

    #[tokio::test]
    async fn facts_about_alias_resolution_preserves_predicate_filter() {
        // Regression guard: the IN-clause refactor must not break the
        // predicate filter for the alias-expanded path.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t-u-a", "user", "prefers", "rust", 1_000, None);
            seed_triple(conn, "t-u-b", "user", "lives_in", "Berlin", 1_500, None);
            seed_triple(conn, "t-a-a", "alex", "prefers", "go", 2_000, None);
            seed_triple(conn, "t-a-b", "alex", "lives_in", "Tokyo", 2_500, None);
        });
        let aliases = vec!["alex".to_string()];

        let hits = facts_about_inner(
            &pool,
            "user",
            &aliases,
            false,
            Some("prefers"),
            None,
            None,
            10,
        )
        .await
        .expect("ok");
        assert_eq!(
            hits.len(),
            2,
            "should see both 'prefers' rows across the alias class"
        );
        let ids: Vec<&str> = hits.iter().map(|h| h.triple_id.as_str()).collect();
        assert!(ids.contains(&"t-u-a"));
        assert!(ids.contains(&"t-a-a"));
    }

    #[tokio::test]
    async fn facts_about_alias_resolution_dedupes_user_alias_entry() {
        // Defensive: a user who lists "user" in their aliases (redundant
        // but legal) shouldn't get double-counted rows. The IN clause
        // deduplicates the parameter list before binding.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t1", "user", "p", "v", 1_000, None);
        });
        let aliases = vec!["user".to_string()];
        let hits = facts_about_inner(&pool, "user", &aliases, false, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 1);
    }

    // ---- facts_about include_as_object widening (v0.5.1 sub-step 8) ----

    #[tokio::test]
    async fn facts_about_include_as_object_matches_objects() {
        // The Steward extracts triples like (sam, pushes_back_on_PRs,
        // maya) where the person you're asking about (maya) appears in
        // the OBJECT slot. With `include_as_object = true`, a query
        // for facts_about(subject="maya") must surface that row.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(
                conn,
                "t-spo",
                "sam",
                "pushes_back_on_prs",
                "maya",
                1_000,
                None,
            );
        });
        let hits = facts_about_inner(&pool, "maya", &[], true, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].triple_id, "t-spo");
        // Returned row preserves its canonical shape — the matched
        // row's subject/object are returned as stored. Callers
        // interpret which side mentioned the queried subject.
        assert_eq!(hits[0].subject_id, "sam");
        assert_eq!(hits[0].object_id, "maya");
    }

    #[tokio::test]
    async fn facts_about_include_as_object_false_omits_objects_by_default() {
        // Default `include_as_object = false` preserves v0.5.0
        // behaviour: the same seed query above returns empty because
        // maya only appears in object position.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(
                conn,
                "t-spo",
                "sam",
                "pushes_back_on_prs",
                "maya",
                1_000,
                None,
            );
        });
        let hits = facts_about_inner(&pool, "maya", &[], false, None, None, None, 10)
            .await
            .expect("ok");
        assert!(
            hits.is_empty(),
            "subject-only query must omit object-position rows"
        );
    }

    #[tokio::test]
    async fn facts_about_include_as_object_dedupes_self_loops() {
        // A self-loop triple (subject_id == object_id == queried)
        // would match both sides of the `OR` — `SELECT DISTINCT`
        // collapses it back to one row.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(
                conn,
                "t-self",
                "quotient",
                "references",
                "quotient",
                1_000,
                None,
            );
        });
        let hits = facts_about_inner(&pool, "quotient", &[], true, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 1, "self-loop must surface exactly once");
        assert_eq!(hits[0].triple_id, "t-self");
    }

    #[tokio::test]
    async fn facts_about_include_as_object_works_with_aliases() {
        // Alias expansion + object-position widening compose: with
        // user_aliases=["alex"] and subject="user", the filter must
        // become `subject_id IN ("user", "alex") OR object_id IN
        // ("user", "alex")`. Seed a row where alex is the object —
        // a query for "user" must surface it.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(
                conn,
                "t-obj-alex",
                "bob",
                "mentioned_to",
                "alex",
                1_000,
                None,
            );
            // And a row where user is the subject — must also
            // surface (regression check that subject-side still works).
            seed_triple(conn, "t-subj-user", "user", "likes", "rust", 2_000, None);
        });
        let aliases = vec!["alex".to_string()];
        let hits = facts_about_inner(&pool, "user", &aliases, true, None, None, None, 10)
            .await
            .expect("ok");
        assert_eq!(hits.len(), 2);
        let ids: Vec<&str> = hits.iter().map(|h| h.triple_id.as_str()).collect();
        assert!(ids.contains(&"t-obj-alex"));
        assert!(ids.contains(&"t-subj-user"));
    }

    // ---- memory_quality_audit ----

    #[tokio::test]
    async fn memory_quality_audit_flags_noisy_derived_data() {
        let long_object =
            "let_user_sign_into_relay_site_authenticate_local_solo_memory_mcp_and_receive_url";
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c-low", 0.61, 1_000);
            seed_cluster_episode(conn, "c-low", "ep-a");
            seed_cluster_episode(conn, "c-low", "ep-b");
            seed_abstraction(
                conn,
                "a-low",
                "c-low",
                "The assistant could not provide assistance.",
                1_100,
            );
            seed_triple(
                conn,
                "t-long",
                "solo_relay",
                "product_goal",
                long_object,
                2_000,
                Some("c-low"),
            );
            seed_triple(
                conn,
                "t-low",
                "literal_only",
                "status",
                "tentative",
                2_100,
                None,
            );
            conn.execute(
                "UPDATE triples SET confidence = 0.80 WHERE triple_id = 't-low'",
                [],
            )
            .expect("lower confidence");
            seed_entity_object_triple(
                conn,
                "t-alias",
                "solo_relay",
                "same_as",
                "solo-relay",
                2_200,
            );
            seed_entity_object_triple(
                conn,
                "t-path",
                "dev_session_skill",
                "location",
                "C:\\Users\\Example\\.codex\\skills\\dev-session",
                2_300,
            );
            seed_entity_object_triple(
                conn,
                "t-commit",
                "codex",
                "committed",
                "commit_5a8e688",
                2_400,
            );
        });

        let report = memory_quality_audit_inner(&pool, MemoryQualityAuditConfig::default())
            .await
            .expect("audit");
        let codes = report
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(report.totals.active_triples, 5);
        assert_eq!(report.totals.clustered_episodes, 2);
        assert!(codes.contains("long_literal_objects"));
        assert!(codes.contains("low_confidence_facts"));
        assert!(codes.contains("entity_triples_without_source_episode"));
        assert!(codes.contains("literal_only_entities"));
        assert!(codes.contains("possible_entity_aliases"));
        assert!(codes.contains("path_like_entities"));
        assert!(codes.contains("commit_like_entities"));
        assert!(codes.contains("low_coherence_clusters"));
        assert!(codes.contains("low_signal_abstractions"));
        assert_eq!(report.health.critical_issues, 1);
        assert!(
            report
                .alias_groups
                .iter()
                .any(|g| { g.labels == vec!["solo-relay".to_string(), "solo_relay".to_string()] })
        );
    }

    #[tokio::test]
    async fn memory_quality_audit_reports_empty_derived_layer() {
        let (pool, _tmp) = pool_with_seed(|_| {});
        let report = memory_quality_audit_inner(&pool, MemoryQualityAuditConfig::default())
            .await
            .expect("audit");
        assert_eq!(report.totals.active_triples, 0);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "no_active_triples")
        );
    }

    // ---- contradictions ----

    #[tokio::test]
    async fn contradictions_returns_empty_on_empty_db() {
        let (pool, _tmp) = pool_with_seed(|_| {});
        let hits = contradictions_inner(&pool, 10).await.expect("ok");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn contradictions_left_join_brings_triple_summaries() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t-a", "Sam", "lives_in", "Berlin", 1_000, None);
            seed_triple(conn, "t-b", "Sam", "lives_in", "Tokyo", 2_000, None);
            seed_contradiction(
                conn,
                "t-a",
                "t-b",
                "overlapping_single_valued_predicate",
                "Sam can't live in both",
                3_000,
            );
        });
        let hits = contradictions_inner(&pool, 10).await.expect("ok");
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.a_id, "t-a");
        assert_eq!(h.b_id, "t-b");
        assert_eq!(h.kind, "overlapping_single_valued_predicate");
        assert_eq!(h.status, "unresolved");
        assert!(h.resolved_at_ms.is_none());
        let a = h.a_triple.as_ref().expect("a_triple joined");
        let b = h.b_triple.as_ref().expect("b_triple joined");
        assert_eq!(a.subject_id, "Sam");
        assert_eq!(a.object_id, "Berlin");
        assert_eq!(b.object_id, "Tokyo");
    }

    #[tokio::test]
    async fn contradictions_left_join_misses_for_deleted_triple() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_triple(conn, "t-a", "Sam", "lives_in", "Berlin", 1_000, None);
            // a_id references a triple that doesn't exist
            seed_contradiction(conn, "t-a", "t-ghost", "direct_negation", "ghost", 4_000);
        });
        let hits = contradictions_inner(&pool, 10).await.expect("ok");
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert!(h.a_triple.is_some());
        assert!(h.b_triple.is_none());
    }

    #[tokio::test]
    async fn contradictions_orders_by_detected_at_desc() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_contradiction(conn, "x1", "y1", "other", "old", 1_000);
            seed_contradiction(conn, "x2", "y2", "other", "newer", 2_000);
            seed_contradiction(conn, "x3", "y3", "other", "newest", 3_000);
        });
        let hits = contradictions_inner(&pool, 10).await.expect("ok");
        assert_eq!(hits[0].explanation, "newest");
        assert_eq!(hits[2].explanation, "old");
    }

    #[tokio::test]
    async fn resolve_contradiction_updates_lifecycle_fields() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_contradiction(conn, "t-a", "t-b", "direct_negation", "conflict", 1_000);
        });
        let resolution = resolve_contradiction_inner(
            &pool,
            "t-a",
            "t-b",
            "direct_negation",
            "resolved",
            Some("user picked t-b"),
            Some("t-b"),
        )
        .await
        .expect("resolve");
        assert_eq!(resolution.status, "resolved");
        assert_eq!(resolution.winning_triple_id.as_deref(), Some("t-b"));
        assert!(resolution.resolved_at_ms.is_some());

        let hits = contradictions_inner(&pool, 10).await.expect("ok");
        assert_eq!(hits[0].status, "resolved");
        assert_eq!(hits[0].resolution_note.as_deref(), Some("user picked t-b"));
        assert_eq!(hits[0].winning_triple_id.as_deref(), Some("t-b"));
    }

    #[tokio::test]
    async fn contradictions_limit_clamped_to_100() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            for i in 0..5 {
                seed_contradiction(
                    conn,
                    &format!("a{i}"),
                    &format!("b{i}"),
                    "other",
                    "x",
                    i as i64,
                );
            }
        });
        let hits = contradictions_inner(&pool, 200).await.expect("ok");
        assert_eq!(hits.len(), 5);
    }

    #[tokio::test]
    async fn limit_zero_clamps_to_one() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c1", 0.5, 1);
            seed_cluster(conn, "c2", 0.5, 2);
        });
        let hits = themes_inner(&pool, None, 0).await.expect("ok");
        // 0 → clamped to 1 → returns 1
        assert_eq!(hits.len(), 1);
    }

    // ---- inspect_cluster ----

    /// Richer seed than `seed_cluster_episode`: inserts an episode row
    /// with caller-specified `content` + `ts_ms` so truncation /
    /// ordering tests can assert real string + timestamp behaviour.
    fn seed_episode_in_cluster(
        conn: &rusqlite::Connection,
        cluster_id: &str,
        memory_id: &str,
        content: &str,
        ts_ms: i64,
    ) {
        conn.execute(
            "INSERT OR IGNORE INTO episodes
                 (memory_id, ts_ms, source_type, content,
                  encoding_context_json, tier, status,
                  confidence, strength, salience,
                  created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 'user_message', ?3,
                         '{}', 'hot', 'active',
                         1.0, 1.0, 1.0, ?2, ?2)",
            rusqlite::params![memory_id, ts_ms, content],
        )
        .expect("seed episode");
        conn.execute(
            "INSERT INTO cluster_episodes (cluster_id, memory_id)
                 VALUES (?1, ?2)",
            rusqlite::params![cluster_id, memory_id],
        )
        .expect("seed cluster_episode link");
    }

    #[tokio::test]
    async fn inspect_cluster_returns_full_record() {
        // Happy path: cluster exists, has an abstraction, has episodes.
        // All three lanes should populate.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c1", 0.7, 1_000);
            seed_abstraction(conn, "a1", "c1", "summary text", 1_500);
            seed_episode_in_cluster(conn, "c1", "ep-a", "first event", 1_100);
            seed_episode_in_cluster(conn, "c1", "ep-b", "second event", 1_200);
        });
        let rec = inspect_cluster_inner(&pool, "c1", false).await.expect("ok");
        assert_eq!(rec.cluster.cluster_id, "c1");
        assert_eq!(rec.cluster.coherence, 0.7_f32);
        assert_eq!(rec.cluster.created_at_ms, 1_000);
        let abs = rec.abstraction.as_ref().expect("abstraction joined");
        assert_eq!(abs.abstraction_id, "a1");
        assert_eq!(abs.content, "summary text");
        assert_eq!(rec.episodes.len(), 2);
        // Ascending by ts_ms — ep-a (1100) before ep-b (1200).
        assert_eq!(rec.episodes[0].memory_id, "ep-a");
        assert_eq!(rec.episodes[0].content, "first event");
        assert_eq!(rec.episodes[0].source_type, "user_message");
        assert_eq!(rec.episodes[0].tier, "hot");
        assert_eq!(rec.episodes[0].status, "active");
        assert_eq!(rec.episodes[1].memory_id, "ep-b");
    }

    #[tokio::test]
    async fn inspect_cluster_unknown_id_returns_not_found() {
        let (pool, _tmp) = pool_with_seed(|conn| {
            // A different cluster exists — proves the NotFound is
            // for the requested id specifically, not "no clusters
            // at all".
            seed_cluster(conn, "c-other", 0.5, 1_000);
        });
        let err = inspect_cluster_inner(&pool, "c-missing", false)
            .await
            .expect_err("should be NotFound");
        match err {
            solo_core::Error::NotFound(msg) => {
                assert!(
                    msg.contains("c-missing"),
                    "error message must name the missing id: {msg}"
                );
            }
            other => panic!("expected NotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn inspect_cluster_includes_abstraction_when_present() {
        // Two clusters: one with abstraction, one without. Each must
        // surface its own abstraction-or-not state.
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c-with", 0.5, 1_000);
            seed_abstraction(conn, "a-with", "c-with", "abs body", 1_500);
            seed_cluster(conn, "c-bare", 0.5, 2_000);
        });
        let with = inspect_cluster_inner(&pool, "c-with", false)
            .await
            .expect("ok");
        assert!(
            with.abstraction.is_some(),
            "c-with must surface abstraction"
        );
        assert_eq!(with.abstraction.as_ref().unwrap().abstraction_id, "a-with");

        let bare = inspect_cluster_inner(&pool, "c-bare", false)
            .await
            .expect("ok");
        assert!(
            bare.abstraction.is_none(),
            "c-bare has no semantic_abstractions row; must be None"
        );
        // Still Ok — empty episodes vec but the cluster row resolves.
        assert_eq!(bare.cluster.cluster_id, "c-bare");
        assert_eq!(bare.episodes.len(), 0);
    }

    #[tokio::test]
    async fn inspect_cluster_truncates_episode_content_to_200_chars() {
        // Seed a 500-char content. With full_content=false the result
        // must be <= 200 chars and end in '…'. With full_content=true
        // it must come back verbatim.
        let long = "a".repeat(500);
        let cluster_id = "c-long";
        let mem_id = "ep-long";
        let pool_with = pool_with_seed({
            let long = long.clone();
            move |conn| {
                seed_cluster(conn, cluster_id, 0.5, 1_000);
                seed_episode_in_cluster(conn, cluster_id, mem_id, &long, 1_100);
            }
        });
        let (pool, _tmp) = pool_with;

        // Default (truncated): exactly 200 chars, ends with U+2026.
        let rec = inspect_cluster_inner(&pool, cluster_id, false)
            .await
            .expect("ok");
        assert_eq!(rec.episodes.len(), 1);
        let content = &rec.episodes[0].content;
        let n_chars = content.chars().count();
        assert_eq!(
            n_chars, EPISODE_TRUNCATE_CHARS,
            "truncated content must be exactly {EPISODE_TRUNCATE_CHARS} chars, got {n_chars}"
        );
        assert!(
            content.ends_with('…'),
            "truncated content must end with U+2026 horizontal ellipsis"
        );
        // First 199 chars are still 'a's (no early splitting).
        let prefix: String = content.chars().take(EPISODE_TRUNCATE_CHARS - 1).collect();
        assert_eq!(prefix, "a".repeat(EPISODE_TRUNCATE_CHARS - 1));

        // full_content=true: verbatim, no ellipsis.
        let rec_full = inspect_cluster_inner(&pool, cluster_id, true)
            .await
            .expect("ok");
        assert_eq!(rec_full.episodes[0].content, long);
        assert_eq!(rec_full.episodes[0].content.chars().count(), 500);
        assert!(
            !rec_full.episodes[0].content.ends_with('…'),
            "full content must not gain a trailing ellipsis"
        );
    }

    #[tokio::test]
    async fn inspect_cluster_truncation_is_utf8_safe() {
        // Defense-in-depth: a 500-codepoint string of 4-byte CJK
        // characters truncated by .chars() must produce a string with
        // exactly EPISODE_TRUNCATE_CHARS chars — no byte-level
        // splitting that would corrupt UTF-8.
        let cjk: String = std::iter::repeat('日').take(500).collect();
        let pool_with = pool_with_seed({
            let cjk = cjk.clone();
            move |conn| {
                seed_cluster(conn, "c-cjk", 0.5, 1_000);
                seed_episode_in_cluster(conn, "c-cjk", "ep-cjk", &cjk, 1_100);
            }
        });
        let (pool, _tmp) = pool_with;
        let rec = inspect_cluster_inner(&pool, "c-cjk", false)
            .await
            .expect("ok");
        let content = &rec.episodes[0].content;
        // Char count exactly 200; round-trips through UTF-8 cleanly
        // (panics here would surface a byte-split bug).
        assert_eq!(content.chars().count(), EPISODE_TRUNCATE_CHARS);
        assert!(content.ends_with('…'));
        // The body should be all 日 characters except the trailing
        // ellipsis.
        for c in content.chars().take(EPISODE_TRUNCATE_CHARS - 1) {
            assert_eq!(c, '日');
        }
    }

    #[tokio::test]
    async fn inspect_cluster_short_content_is_not_truncated() {
        // Regression guard: content shorter than the cap returns
        // verbatim with NO trailing ellipsis. Off-by-one on the
        // truncate boundary would surface here.
        let short = "tiny";
        let pool_with = pool_with_seed(|conn| {
            seed_cluster(conn, "c-short", 0.5, 1_000);
            seed_episode_in_cluster(conn, "c-short", "ep-tiny", short, 1_100);
        });
        let (pool, _tmp) = pool_with;
        let rec = inspect_cluster_inner(&pool, "c-short", false)
            .await
            .expect("ok");
        assert_eq!(rec.episodes[0].content, "tiny");
        assert!(!rec.episodes[0].content.ends_with('…'));
    }

    #[tokio::test]
    async fn inspect_cluster_skips_forgotten_episodes() {
        // A forgotten episode in the link table must not surface in
        // the snapshot — same posture as memory_recall (active-only).
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_cluster(conn, "c1", 0.5, 1_000);
            seed_episode_in_cluster(conn, "c1", "ep-keep", "kept", 1_100);
            seed_episode_in_cluster(conn, "c1", "ep-gone", "forgotten", 1_200);
            conn.execute(
                "UPDATE episodes SET status = 'forgotten' WHERE memory_id = ?1",
                rusqlite::params!["ep-gone"],
            )
            .expect("flip status");
        });
        let rec = inspect_cluster_inner(&pool, "c1", false).await.expect("ok");
        assert_eq!(rec.episodes.len(), 1);
        assert_eq!(rec.episodes[0].memory_id, "ep-keep");
    }
}
