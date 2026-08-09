// SPDX-License-Identifier: Apache-2.0

//! Combined memory context for agent clients.
//!
//! `memory_context` is the first read surface designed around the way coding
//! agents use memory: gather the nearest raw episodes, recent themes,
//! structured facts for an optional subject, and known contradictions in one
//! bounded response. Lower-level tools remain available for drill-down.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use solo_core::{Embedder, Error, Result, VectorIndex};
use solo_storage::{AuditOperation, LibraryHandle, MemoryRetrievalLogEntry, ReaderPool};

use crate::derived::{
    ContradictionHit, EntityHit, FactHit, ThemeHit, contradictions_inner, entities_inner,
    facts_about_inner, themes_inner,
};
use crate::recall::{RecallResult, run_recall_inner};

/// One explainable context bundle for a user query.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryContextResult {
    pub query: String,
    pub subject: Option<String>,
    pub resolved_subject: Option<String>,
    pub sections: MemoryContextSections,
    pub recall: RecallResult,
    pub themes: Vec<ThemeHit>,
    pub entities: Vec<EntityHit>,
    pub facts: Vec<FactHit>,
    pub contradictions: Vec<ContradictionHit>,
    pub graph: MemoryGraphContext,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryContextSections {
    pub recall: MemoryContextSectionHealth,
    pub themes: MemoryContextSectionHealth,
    pub entities: MemoryContextSectionHealth,
    pub facts: MemoryContextSectionHealth,
    pub contradictions: MemoryContextSectionHealth,
    pub graph: MemoryContextSectionHealth,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryContextSectionHealth {
    pub status: String,
    pub count: usize,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MemoryGraphContext {
    pub seed_entities: Vec<String>,
    pub aliases: Vec<MemoryGraphAlias>,
    pub relationship_facts: Vec<MemoryGraphFact>,
    pub literal_facts: Vec<MemoryGraphFact>,
    pub relationship_paths: Vec<MemoryGraphPath>,
    pub review_warnings: Vec<MemoryGraphReviewWarning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryGraphAlias {
    pub alias: String,
    pub canonical_id: String,
    pub display_label: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryGraphFact {
    pub edge_id: String,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: String,
    pub direction: String,
    pub confidence: f32,
    pub strength: f32,
    pub score: f32,
    pub evidence_count: i64,
    pub reason_codes: Vec<String>,
    pub valid_from_ms: i64,
    pub valid_to_ms: Option<i64>,
    pub cluster_id: Option<String>,
    pub source_episode_id: Option<i64>,
    pub memory_id: Option<String>,
    pub evidence_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryGraphPath {
    pub nodes: Vec<String>,
    pub edges: Vec<MemoryGraphPathStep>,
    pub hops: u8,
    pub score: f32,
    pub reason_codes: Vec<String>,
    pub path_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryGraphPathStep {
    pub edge_id: String,
    pub source_id: String,
    pub target_id: String,
    pub predicate: String,
    pub confidence: f32,
    pub strength: f32,
    pub evidence_count: i64,
    pub valid_from_ms: i64,
    pub valid_to_ms: Option<i64>,
    pub evidence_memory_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryGraphReviewWarning {
    pub review_id: String,
    pub reason_code: String,
    pub reason: String,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: String,
    pub confidence: f32,
}

#[allow(clippy::too_many_arguments)]
pub async fn memory_context(
    tenant: &LibraryHandle,
    audit_principal: Option<String>,
    query: &str,
    subject: Option<&str>,
    user_aliases: &[String],
    window_days: Option<i64>,
    limit: usize,
) -> Result<MemoryContextResult> {
    let result = memory_context_inner(
        tenant.embedder(),
        tenant.hnsw(),
        tenant.read(),
        query,
        subject,
        user_aliases,
        window_days,
        limit,
    )
    .await;
    match &result {
        Ok(bundle) => {
            tenant
                .audit()
                .emit_ok(audit_principal, AuditOperation::MemoryContext, None);
            let _ = tenant
                .write()
                .record_memory_retrieval(memory_context_retrieval_log_entry(bundle))
                .await;
        }
        Err(e) => {
            tenant
                .audit()
                .emit_error(audit_principal, AuditOperation::MemoryContext, None, e)
        }
    }
    result
}

fn memory_context_retrieval_log_entry(bundle: &MemoryContextResult) -> MemoryRetrievalLogEntry {
    let mut recalled_ids = Vec::new();
    for hit in &bundle.recall.hits {
        if !recalled_ids.iter().any(|id| id == &hit.memory_id) {
            recalled_ids.push(hit.memory_id.clone());
        }
    }
    for fact in bundle
        .graph
        .relationship_facts
        .iter()
        .chain(bundle.graph.literal_facts.iter())
    {
        if !recalled_ids.iter().any(|id| id == &fact.edge_id) {
            recalled_ids.push(fact.edge_id.clone());
        }
    }
    for path in &bundle.graph.relationship_paths {
        for edge in &path.edges {
            if !recalled_ids.iter().any(|id| id == &edge.edge_id) {
                recalled_ids.push(edge.edge_id.clone());
            }
        }
    }

    let mut reason_codes = vec!["memory_context".to_string()];
    if !bundle.recall.hits.is_empty() {
        push_log_reason_code(&mut reason_codes, "semantic_match");
    }
    if bundle
        .recall
        .hits
        .iter()
        .any(|hit| hit.lexical_rank.is_some() || hit.bm25_score.is_some())
    {
        push_log_reason_code(&mut reason_codes, "lexical_match");
    }
    if !bundle.graph.relationship_facts.is_empty()
        || !bundle.graph.literal_facts.is_empty()
        || !bundle.graph.relationship_paths.is_empty()
    {
        push_log_reason_code(&mut reason_codes, "graph_neighbor");
    }
    if !bundle.facts.is_empty() {
        push_log_reason_code(&mut reason_codes, "subject_fact");
    }
    if !bundle.themes.is_empty() {
        push_log_reason_code(&mut reason_codes, "recent_theme");
    }
    if !bundle.contradictions.is_empty() || !bundle.graph.review_warnings.is_empty() {
        push_log_reason_code(&mut reason_codes, "contradiction_warning");
    }

    MemoryRetrievalLogEntry {
        query: bundle.query.clone(),
        recalled_ids,
        reason_codes,
    }
}

fn push_log_reason_code(codes: &mut Vec<String>, code: &str) {
    if !codes.iter().any(|existing| existing == code) {
        codes.push(code.to_string());
    }
}

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn memory_context_inner(
    embedder: &Arc<dyn Embedder>,
    hnsw: &Arc<dyn VectorIndex + Send + Sync>,
    pool: &ReaderPool,
    query: &str,
    subject: Option<&str>,
    user_aliases: &[String],
    window_days: Option<i64>,
    limit: usize,
) -> Result<MemoryContextResult> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::invalid_input(
            "memory_context query must not be empty",
        ));
    }

    let subject = subject
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let (recall, recall_health) = match run_recall_inner(embedder, hnsw, pool, query, limit).await {
        Ok(recall) => {
            let count = recall.hits.len();
            (recall, section_ok(count))
        }
        Err(e) => (
            RecallResult {
                hits: Vec::new(),
                index_len: hnsw.len(),
                candidates_considered: 0,
            },
            section_degraded(format!("recall failed: {e}")),
        ),
    };
    let (themes, themes_health) = match themes_inner(pool, window_days, limit).await {
        Ok(themes) => {
            let count = themes.len();
            (themes, section_ok(count))
        }
        Err(e) => (Vec::new(), section_degraded(format!("themes failed: {e}"))),
    };

    let entity_query = subject.as_deref().unwrap_or(query);
    let (entities, entities_health) = match entities_inner(pool, entity_query, limit).await {
        Ok(entities) => {
            let count = entities.len();
            (entities, section_ok(count))
        }
        Err(e) => (
            Vec::new(),
            section_degraded(format!("entities failed: {e}")),
        ),
    };
    let resolved_subject = subject
        .as_deref()
        .and_then(|s| resolve_subject(s, &entities))
        .or_else(|| subject.clone());
    let (facts, facts_health) = if let Some(resolved) = resolved_subject.as_deref() {
        match facts_about_inner(pool, resolved, user_aliases, true, None, None, None, limit).await {
            Ok(facts) => {
                let count = facts.len();
                let mut health = section_ok(count);
                if let Some(requested) = subject.as_deref() {
                    if requested != resolved {
                        health.warning = Some(format!(
                            "resolved requested subject '{requested}' to '{resolved}'"
                        ));
                    }
                }
                (facts, health)
            }
            Err(e) => (Vec::new(), section_degraded(format!("facts failed: {e}"))),
        }
    } else {
        (Vec::new(), section_skipped("no subject supplied"))
    };
    let (contradictions, contradictions_health) = match contradictions_inner(pool, limit).await {
        Ok(contradictions) => {
            let count = contradictions.len();
            (contradictions, section_ok(count))
        }
        Err(e) => (
            Vec::new(),
            section_degraded(format!("contradictions failed: {e}")),
        ),
    };
    let seed_entities = graph_seed_entities(resolved_subject.as_deref(), &entities, limit);
    let (graph, graph_health) = match graph_context_inner(pool, seed_entities, limit).await {
        Ok(graph) => {
            let count = graph.relationship_facts.len()
                + graph.literal_facts.len()
                + graph.relationship_paths.len()
                + graph.review_warnings.len();
            (graph, section_ok(count))
        }
        Err(e) => (
            MemoryGraphContext::default(),
            section_degraded(format!("graph context failed: {e}")),
        ),
    };

    Ok(MemoryContextResult {
        query: query.to_string(),
        subject,
        resolved_subject,
        sections: MemoryContextSections {
            recall: recall_health,
            themes: themes_health,
            entities: entities_health,
            facts: facts_health,
            contradictions: contradictions_health,
            graph: graph_health,
        },
        recall,
        themes,
        entities,
        facts,
        contradictions,
        graph,
    })
}

fn resolve_subject(subject: &str, entities: &[EntityHit]) -> Option<String> {
    entities
        .iter()
        .find(|hit| hit.entity_id.eq_ignore_ascii_case(subject))
        .or_else(|| entities.first())
        .map(|hit| hit.entity_id.clone())
}

fn graph_seed_entities(
    resolved_subject: Option<&str>,
    entities: &[EntityHit],
    limit: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(subject) = resolved_subject {
        out.push(subject.to_string());
    }
    for entity in entities.iter().take(limit.clamp(1, 8)) {
        if !out.iter().any(|seed| seed == &entity.entity_id) {
            out.push(entity.entity_id.clone());
        }
    }
    out.truncate(limit.clamp(1, 8));
    out
}

async fn graph_context_inner(
    pool: &ReaderPool,
    seed_entities: Vec<String>,
    limit: usize,
) -> Result<MemoryGraphContext> {
    if seed_entities.is_empty() {
        return Ok(MemoryGraphContext::default());
    }
    let limit = limit.clamp(1, 50);
    pool.interact(move |conn| {
        let aliases = fetch_graph_aliases(conn, &seed_entities, limit)?;
        let facts = fetch_graph_facts(conn, &seed_entities, limit)?;
        let relationship_paths = fetch_graph_relationship_paths(conn, &seed_entities, limit)?;
        let review_warnings = fetch_graph_review_warnings(conn, &seed_entities, limit)?;
        let mut relationship_facts = Vec::new();
        let mut literal_facts = Vec::new();
        for fact in facts {
            if fact.object_kind == "entity" {
                relationship_facts.push(fact);
            } else {
                literal_facts.push(fact);
            }
        }
        Ok::<_, rusqlite::Error>(MemoryGraphContext {
            seed_entities,
            aliases,
            relationship_facts,
            literal_facts,
            relationship_paths,
            review_warnings,
        })
    })
    .await
}

fn fetch_graph_aliases(
    conn: &rusqlite::Connection,
    seeds: &[String],
    limit: usize,
) -> rusqlite::Result<Vec<MemoryGraphAlias>> {
    let placeholders = positional_placeholders(seeds.len());
    let sql = format!(
        "SELECT alias, canonical_id, display_label, confidence
           FROM entity_aliases
          WHERE canonical_id IN ({placeholders}) OR alias IN ({placeholders})
          ORDER BY confidence DESC, updated_at_ms DESC, alias ASC
          LIMIT ?{}",
        seeds.len() + 1
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = graph_params(seeds);
    params.push(Box::new(limit as i64));
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(
        rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        |r| {
            Ok(MemoryGraphAlias {
                alias: r.get(0)?,
                canonical_id: r.get(1)?,
                display_label: r.get(2)?,
                confidence: r.get(3)?,
            })
        },
    )?
    .collect()
}

fn fetch_graph_facts(
    conn: &rusqlite::Connection,
    seeds: &[String],
    limit: usize,
) -> rusqlite::Result<Vec<MemoryGraphFact>> {
    let placeholders = positional_placeholders(seeds.len());
    let sql = format!(
        "SELECT re.edge_id,
                re.subject_entity_id,
                re.predicate,
                COALESCE(re.object_entity_id, re.object_literal) AS object_id,
                re.object_kind,
                re.confidence,
                re.strength,
                re.evidence_count,
                re.valid_from_ms,
                re.valid_to_ms,
                ev.cluster_id,
                CASE WHEN e.rowid IS NULL THEN NULL ELSE ev.source_episode_id END AS source_episode_id,
                CASE WHEN e.rowid IS NULL THEN NULL ELSE ev.memory_id END AS memory_id,
                substr(e.content, 1, 240) AS evidence_preview
           FROM relationship_edges re
           LEFT JOIN relationship_evidence ev
             ON ev.evidence_id = (
                SELECT ev2.evidence_id
                  FROM relationship_evidence ev2
                 WHERE ev2.edge_id = re.edge_id
                 ORDER BY ev2.extraction_confidence DESC,
                          ev2.created_at_ms DESC,
                          ev2.evidence_id ASC
                 LIMIT 1
             )
           LEFT JOIN episodes e
             ON e.status = 'active'
            AND (
                (ev.source_episode_id IS NOT NULL AND e.rowid = ev.source_episode_id)
                OR (ev.source_episode_id IS NULL
                    AND ev.memory_id IS NOT NULL
                    AND e.memory_id = ev.memory_id)
            )
          WHERE re.status = 'active'
            AND (
                re.subject_entity_id IN ({placeholders})
                OR re.object_entity_id IN ({placeholders})
            )
          ORDER BY re.object_kind = 'entity' DESC,
                   re.strength DESC,
                   re.updated_at_ms DESC,
                   re.edge_id ASC
          LIMIT ?{}",
        seeds.len() + 1
    );
    let mut params = graph_params(seeds);
    params.push(Box::new(limit as i64));
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(
        rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        |r| {
            let subject_id: String = r.get(1)?;
            let object_id: String = r.get(3)?;
            let object_kind: String = r.get(4)?;
            let confidence: f32 = r.get(5)?;
            let strength: f32 = r.get(6)?;
            let evidence_count: i64 = r.get(7)?;
            let direction = graph_fact_direction(&subject_id, &object_id, &object_kind, seeds);
            let score = graph_item_score(confidence, strength, evidence_count);
            Ok(MemoryGraphFact {
                edge_id: r.get(0)?,
                subject_id,
                predicate: r.get(2)?,
                object_id,
                object_kind: object_kind.clone(),
                direction: direction.clone(),
                confidence,
                strength,
                score,
                evidence_count,
                reason_codes: graph_fact_reason_codes(&object_kind, &direction, evidence_count),
                valid_from_ms: r.get(8)?,
                valid_to_ms: r.get(9)?,
                cluster_id: r.get(10)?,
                source_episode_id: r.get(11)?,
                memory_id: r.get(12)?,
                evidence_preview: r.get(13)?,
            })
        },
    )?
    .collect()
}

fn fetch_graph_relationship_paths(
    conn: &rusqlite::Connection,
    seeds: &[String],
    limit: usize,
) -> rusqlite::Result<Vec<MemoryGraphPath>> {
    let direct_edge_ids = fetch_direct_graph_path_edge_ids(conn, seeds, limit as i64)?;
    let two_hop_edge_ids = fetch_two_hop_graph_path_edge_ids(conn, seeds, limit as i64)?;
    let mut step_edge_ids = direct_edge_ids.clone();
    for (first_edge_id, second_edge_id) in &two_hop_edge_ids {
        if !step_edge_ids.iter().any(|id| id == first_edge_id) {
            step_edge_ids.push(first_edge_id.clone());
        }
        if !step_edge_ids.iter().any(|id| id == second_edge_id) {
            step_edge_ids.push(second_edge_id.clone());
        }
    }
    let steps = fetch_graph_path_steps(conn, &step_edge_ids)?;
    let mut paths = Vec::with_capacity(direct_edge_ids.len() + two_hop_edge_ids.len());

    for edge_id in direct_edge_ids {
        if let Some(step) = steps.get(&edge_id).cloned() {
            paths.push(build_memory_graph_path(vec![step]));
        }
    }
    for (first_edge_id, second_edge_id) in two_hop_edge_ids {
        let Some(first_step) = steps.get(&first_edge_id).cloned() else {
            continue;
        };
        let Some(second_step) = steps.get(&second_edge_id).cloned() else {
            continue;
        };
        paths.push(build_memory_graph_path(vec![first_step, second_step]));
    }

    paths.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.hops.cmp(&b.hops))
            .then_with(|| a.path_text.cmp(&b.path_text))
    });
    paths.truncate(limit);
    Ok(paths)
}

fn fetch_direct_graph_path_edge_ids(
    conn: &rusqlite::Connection,
    seeds: &[String],
    limit: i64,
) -> rusqlite::Result<Vec<String>> {
    let placeholders = positional_placeholders(seeds.len());
    let sql = format!(
        "SELECT re.edge_id
           FROM relationship_edges re
          WHERE re.status = 'active'
            AND re.object_kind = 'entity'
            AND re.subject_entity_id IN ({placeholders})
          ORDER BY (re.strength * re.confidence) DESC,
                   re.updated_at_ms DESC,
                   re.edge_id ASC
          LIMIT ?{}",
        seeds.len() + 1
    );
    let mut params = graph_params(seeds);
    params.push(Box::new(limit));
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(
        rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        |r| r.get(0),
    )?
    .collect()
}

fn fetch_two_hop_graph_path_edge_ids(
    conn: &rusqlite::Connection,
    seeds: &[String],
    limit: i64,
) -> rusqlite::Result<Vec<(String, String)>> {
    let placeholders = positional_placeholders(seeds.len());
    let sql = format!(
        "SELECT a.edge_id,
                b.edge_id
           FROM relationship_edges a
           JOIN relationship_edges b
             ON b.subject_entity_id = a.object_entity_id
          WHERE a.status = 'active'
            AND b.status = 'active'
            AND a.object_kind = 'entity'
            AND b.object_kind = 'entity'
            AND a.subject_entity_id IN ({placeholders})
            AND b.object_entity_id NOT IN ({placeholders})
            AND b.object_entity_id <> a.subject_entity_id
          ORDER BY ((a.strength * a.confidence) * (b.strength * b.confidence)) DESC,
                   a.edge_id ASC,
                   b.edge_id ASC
          LIMIT ?{}",
        seeds.len() + 1
    );
    let mut params = graph_params(seeds);
    params.push(Box::new(limit));
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(
        rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?
    .collect()
}

fn fetch_graph_path_steps(
    conn: &rusqlite::Connection,
    edge_ids: &[String],
) -> rusqlite::Result<HashMap<String, MemoryGraphPathStep>> {
    if edge_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = positional_placeholders(edge_ids.len());
    let sql = format!(
        "SELECT re.edge_id,
                re.subject_entity_id,
                re.object_entity_id,
                re.predicate,
                re.confidence,
                re.strength,
                re.evidence_count,
                re.valid_from_ms,
                re.valid_to_ms,
                CASE WHEN ep.rowid IS NULL THEN NULL ELSE ev.memory_id END AS evidence_memory_id
           FROM relationship_edges re
           LEFT JOIN relationship_evidence ev
             ON ev.evidence_id = (
                SELECT ev2.evidence_id
                  FROM relationship_evidence ev2
                 WHERE ev2.edge_id = re.edge_id
                 ORDER BY ev2.extraction_confidence DESC,
                          ev2.created_at_ms DESC,
                          ev2.evidence_id ASC
                 LIMIT 1
             )
           LEFT JOIN episodes ep
             ON ep.status = 'active'
            AND (
                (ev.source_episode_id IS NOT NULL AND ep.rowid = ev.source_episode_id)
                OR (ev.source_episode_id IS NULL
                    AND ev.memory_id IS NOT NULL
                    AND ep.memory_id = ev.memory_id)
            )
          WHERE re.edge_id IN ({placeholders})
            AND re.status = 'active'
            AND re.object_kind = 'entity'"
    );
    let params: Vec<Box<dyn rusqlite::ToSql>> = edge_ids
        .iter()
        .map(|edge_id| Box::new(edge_id.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        |r| {
            Ok(MemoryGraphPathStep {
                edge_id: r.get(0)?,
                source_id: r.get(1)?,
                target_id: r.get(2)?,
                predicate: r.get(3)?,
                confidence: r.get(4)?,
                strength: r.get(5)?,
                evidence_count: r.get(6)?,
                valid_from_ms: r.get(7)?,
                valid_to_ms: r.get(8)?,
                evidence_memory_id: r.get(9)?,
            })
        },
    )?;
    let mut out = HashMap::with_capacity(edge_ids.len());
    for row in rows {
        let step = row?;
        out.insert(step.edge_id.clone(), step);
    }
    Ok(out)
}

fn build_memory_graph_path(steps: Vec<MemoryGraphPathStep>) -> MemoryGraphPath {
    let mut nodes = Vec::with_capacity(steps.len() + 1);
    if let Some(first) = steps.first() {
        nodes.push(first.source_id.clone());
    }
    for step in &steps {
        nodes.push(step.target_id.clone());
    }

    let score = if steps.is_empty() {
        0.0
    } else {
        let product = steps
            .iter()
            .map(|step| graph_item_score(step.confidence, step.strength, step.evidence_count))
            .fold(1.0_f32, |acc, score| acc * score);
        product.powf(1.0 / steps.len() as f32)
    };
    let hops = steps.len() as u8;
    let path_text = graph_path_text(&steps);
    let reason_codes = graph_path_reason_codes(hops, steps.iter().all(|s| s.evidence_count > 0));

    MemoryGraphPath {
        nodes,
        edges: steps,
        hops,
        score,
        reason_codes,
        path_text,
    }
}

fn graph_path_text(steps: &[MemoryGraphPathStep]) -> String {
    let Some(first) = steps.first() else {
        return String::new();
    };
    let mut text = first.source_id.clone();
    for step in steps {
        text.push_str(" -[");
        text.push_str(&step.predicate);
        text.push_str("]-> ");
        text.push_str(&step.target_id);
    }
    text
}

fn graph_item_score(confidence: f32, strength: f32, evidence_count: i64) -> f32 {
    let evidence_boost = if evidence_count > 0 {
        1.0 + (evidence_count.min(10) as f32).ln_1p() * 0.05
    } else {
        1.0
    };
    confidence.max(0.0) * strength.max(0.0) * evidence_boost
}

fn graph_fact_direction(
    subject_id: &str,
    object_id: &str,
    object_kind: &str,
    seeds: &[String],
) -> String {
    let subject_seed = seeds.iter().any(|seed| seed == subject_id);
    let object_seed = object_kind == "entity" && seeds.iter().any(|seed| seed == object_id);
    match (subject_seed, object_seed) {
        (true, true) if subject_id == object_id => "self".to_string(),
        (true, true) => "between_seed_entities".to_string(),
        (true, false) => "outgoing".to_string(),
        (false, true) => "incoming".to_string(),
        (false, false) => "related".to_string(),
    }
}

fn graph_fact_reason_codes(object_kind: &str, direction: &str, evidence_count: i64) -> Vec<String> {
    let mut codes = Vec::with_capacity(4);
    codes.push("query_entity".to_string());
    codes.push(direction.to_string());
    if object_kind == "entity" {
        codes.push("graph_neighbor".to_string());
    } else {
        codes.push("literal_fact".to_string());
    }
    if evidence_count > 0 {
        codes.push("evidence_backed".to_string());
    }
    codes
}

fn graph_path_reason_codes(hops: u8, evidence_backed: bool) -> Vec<String> {
    let mut codes = Vec::with_capacity(4);
    codes.push("query_entity".to_string());
    codes.push(if hops == 1 {
        "direct_relationship".to_string()
    } else {
        "two_hop_relationship_path".to_string()
    });
    codes.push("graph_neighbor".to_string());
    if evidence_backed {
        codes.push("evidence_backed".to_string());
    }
    codes
}

fn fetch_graph_review_warnings(
    conn: &rusqlite::Connection,
    seeds: &[String],
    limit: usize,
) -> rusqlite::Result<Vec<MemoryGraphReviewWarning>> {
    let placeholders = positional_placeholders(seeds.len());
    let sql = format!(
        "SELECT review_id, reason_code, reason, subject_id, predicate,
                object_id, object_kind, confidence
           FROM triple_reviews
          WHERE status = 'needs_review'
            AND (
                subject_id IN ({placeholders})
                OR (object_kind = 'entity' AND object_id IN ({placeholders}))
            )
          ORDER BY created_at_ms DESC, review_id ASC
          LIMIT ?{}",
        seeds.len() + 1
    );
    let mut params = graph_params(seeds);
    params.push(Box::new(limit as i64));
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(
        rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        |r| {
            Ok(MemoryGraphReviewWarning {
                review_id: r.get(0)?,
                reason_code: r.get(1)?,
                reason: r.get(2)?,
                subject_id: r.get(3)?,
                predicate: r.get(4)?,
                object_id: r.get(5)?,
                object_kind: r.get(6)?,
                confidence: r.get(7)?,
            })
        },
    )?
    .collect()
}

fn positional_placeholders(n: usize) -> String {
    (1..=n)
        .map(|idx| format!("?{idx}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn graph_params(seeds: &[String]) -> Vec<Box<dyn rusqlite::ToSql>> {
    seeds
        .iter()
        .map(|seed| Box::new(seed.clone()) as Box<dyn rusqlite::ToSql>)
        .collect()
}

fn section_ok(count: usize) -> MemoryContextSectionHealth {
    MemoryContextSectionHealth {
        status: "ok".to_string(),
        count,
        warning: None,
    }
}

fn section_skipped(reason: impl Into<String>) -> MemoryContextSectionHealth {
    MemoryContextSectionHealth {
        status: "skipped".to_string(),
        count: 0,
        warning: Some(reason.into()),
    }
}

fn section_degraded(warning: impl Into<String>) -> MemoryContextSectionHealth {
    MemoryContextSectionHealth {
        status: "degraded".to_string(),
        count: 0,
        warning: Some(warning.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_storage::test_support::{StubVectorIndex, open_test_db_at};
    use solo_storage::{ReaderPool, StubEmbedder, WriterActor, WriterSpawn};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    #[allow(clippy::type_complexity)]
    fn fixture(
        runtime: &tokio::runtime::Runtime,
    ) -> (
        Arc<dyn Embedder>,
        Arc<dyn VectorIndex + Send + Sync>,
        ReaderPool,
        solo_storage::WriteHandle,
        tempfile::TempDir,
        std::thread::JoinHandle<()>,
    ) {
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = 16usize;
        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", dim));
        let db_path = tmp.path().join("test.db");
        let conn = open_test_db_at(&db_path);
        let WriterSpawn { handle, join } = WriterActor::spawn(conn, hnsw.clone());
        let pool =
            runtime.block_on(async { ReaderPool::new(&db_path, None, hnsw.clone()).unwrap() });
        (embedder, hnsw, pool, handle, tmp, join)
    }

    fn shutdown(
        runtime: &tokio::runtime::Runtime,
        pool: ReaderPool,
        handle: solo_storage::WriteHandle,
        tmp: tempfile::TempDir,
        join: std::thread::JoinHandle<()>,
    ) {
        runtime.block_on(async move {
            drop(handle);
            drop(pool);
            drop(tmp);
            tokio::task::spawn_blocking(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(join.join());
                });
                rx.recv_timeout(std::time::Duration::from_secs(5))
                    .expect("writer thread did not exit within 5s")
                    .expect("writer thread panicked");
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn memory_context_combines_recall_and_empty_derived_sections() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let ep = solo_storage::test_support::fixture_episode("project context alpha");
            handle
                .remember(ep, embedder.embed("project context alpha").await.unwrap())
                .await
                .unwrap();

            let result = memory_context_inner(
                &embedder,
                &hnsw,
                &pool,
                "project context",
                None,
                &[],
                None,
                5,
            )
            .await
            .unwrap();

            assert_eq!(result.query, "project context");
            assert!(result.subject.is_none());
            assert_eq!(result.recall.hits.len(), 1);
            assert_eq!(result.recall.hits[0].content, "project context alpha");
            assert_eq!(result.sections.recall.status, "ok");
            assert_eq!(result.sections.recall.count, 1);
            assert_eq!(result.sections.facts.status, "skipped");
            assert!(result.themes.is_empty());
            assert!(result.facts.is_empty());
            assert!(result.contradictions.is_empty());
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn memory_context_rejects_blank_query() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let err = memory_context_inner(&embedder, &hnsw, &pool, "   ", None, &[], None, 5)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::InvalidInput(_)), "got: {err:?}");
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn memory_context_includes_graph_relationships_literals_and_review_warnings() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            pool.interact(|conn| {
                let now_ms = chrono::Utc::now().timestamp_millis();
                conn.execute(
                    "INSERT INTO entities
                        (entity_id, canonical_name, entity_type, aliases_json, confidence,
                         first_seen_ms, last_seen_ms, status, created_at_ms, updated_at_ms)
                     VALUES
                        ('solo', 'Solo', 'project', '[]', 1.0, ?1, ?1, 'active', ?1, ?1),
                        ('ollama', 'Ollama', 'tool', '[]', 1.0, ?1, ?1, 'active', ?1, ?1),
                        ('nomic', 'Nomic', 'model', '[]', 1.0, ?1, ?1, 'active', ?1, ?1)",
                    rusqlite::params![now_ms],
                )?;
                conn.execute(
                    "INSERT INTO triples
                        (triple_id, subject_id, predicate, object_id, object_kind,
                         valid_from_ms, valid_to_ms, confidence, provenance_json,
                         status, created_at_ms, updated_at_ms)
                     VALUES
                        ('t-solo-uses-ollama', 'solo', 'uses', 'ollama', 'entity',
                         ?1, NULL, 0.95, '{}', 'active', ?1, ?1),
                        ('t-solo-uses-ollama-2', 'solo', 'uses', 'ollama', 'entity',
                         ?1, NULL, 0.99, '{}', 'active', ?1, ?1),
                        ('t-solo-status', 'solo', 'has_status', 'graph_alive', 'literal',
                         ?1, NULL, 0.90, '{}', 'active', ?1, ?1),
                        ('t-ollama-serves-nomic', 'ollama', 'serves', 'nomic', 'entity',
                         ?1, NULL, 0.90, '{}', 'active', ?1, ?1),
                        ('t-solo-references-nomic', 'solo', 'references', 'nomic', 'entity',
                         ?1, NULL, 0.90, '{}', 'active', ?1, ?1)",
                    rusqlite::params![now_ms],
                )?;
                conn.execute(
                    "INSERT INTO relationship_edges
                        (edge_id, subject_entity_id, predicate, object_entity_id, object_literal,
                         object_kind, valid_from_ms, valid_to_ms, confidence, strength,
                         evidence_count, status, created_at_ms, updated_at_ms)
                     VALUES
                        ('t-solo-uses-ollama', 'solo', 'uses', 'ollama', NULL, 'entity',
                         ?1, NULL, 0.95, 0.95, 0, 'active', ?1, ?1),
                        ('t-solo-status', 'solo', 'has_status', NULL, 'graph_alive', 'literal',
                         ?1, NULL, 0.90, 0.90, 0, 'active', ?1, ?1),
                        ('t-ollama-serves-nomic', 'ollama', 'serves', 'nomic', NULL, 'entity',
                         ?1, NULL, 0.90, 0.90, 0, 'active', ?1, ?1),
                        ('t-solo-references-nomic', 'solo', 'references', 'nomic', NULL, 'entity',
                         ?1, NULL, 0.80, 0.80, 0, 'active', ?1, ?1)",
                    rusqlite::params![now_ms],
                )?;
                conn.execute(
                    "UPDATE relationship_edges
                        SET strength = 0.80, confidence = 0.80
                      WHERE edge_id = 't-solo-references-nomic'",
                    [],
                )?;
                conn.execute(
                    "UPDATE triples
                        SET confidence = 0.80
                      WHERE triple_id = 't-solo-references-nomic'",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO episodes
                        (memory_id, ts_ms, source_type, content, encoding_context_json,
                         confidence, strength, salience, tier, status, created_at_ms, updated_at_ms)
                     VALUES
                        ('mem-low-evidence', ?1, 'user_message',
                         'Low confidence evidence should not be selected.', '{}',
                         1.0, 0.5, 0.5, 'hot', 'active', ?1, ?1),
                        ('mem-high-evidence', ?1, 'user_message',
                         'High confidence evidence should be selected.', '{}',
                         1.0, 0.5, 0.5, 'hot', 'active', ?1, ?1),
                        ('mem-forgotten-evidence', ?1, 'user_message',
                         'Forgotten evidence must not be previewed.', '{}',
                         1.0, 0.5, 0.5, 'hot', 'forgotten', ?1, ?1)",
                    rusqlite::params![now_ms],
                )?;
                conn.execute(
                    "INSERT INTO relationship_evidence
                        (evidence_id, edge_id, triple_id, memory_id, source_episode_id,
                         cluster_id, extraction_confidence, created_at_ms)
                     VALUES
                        ('ev-low', 't-solo-uses-ollama', 't-solo-uses-ollama',
                         'mem-low-evidence',
                         (SELECT rowid FROM episodes WHERE memory_id = 'mem-low-evidence'),
                         NULL, 0.40, ?1),
                        ('ev-high', 't-solo-uses-ollama', 't-solo-uses-ollama-2',
                         'mem-high-evidence',
                         (SELECT rowid FROM episodes WHERE memory_id = 'mem-high-evidence'),
                         NULL, 0.99, ?1),
                        ('ev-forgotten-literal', 't-solo-status', 't-solo-status',
                         'mem-forgotten-evidence',
                         (SELECT rowid FROM episodes WHERE memory_id = 'mem-forgotten-evidence'),
                         NULL, 0.90, ?1),
                        ('ev-nomic', 't-ollama-serves-nomic', 't-ollama-serves-nomic',
                         'mem-high-evidence',
                         (SELECT rowid FROM episodes WHERE memory_id = 'mem-high-evidence'),
                         NULL, 0.90, ?1),
                        ('ev-forgotten-path', 't-solo-references-nomic', 't-solo-references-nomic',
                         'mem-forgotten-evidence',
                         (SELECT rowid FROM episodes WHERE memory_id = 'mem-forgotten-evidence'),
                         NULL, 0.90, ?1)",
                    rusqlite::params![now_ms],
                )?;
                conn.execute(
                    "INSERT INTO triple_reviews
                        (review_id, candidate_fingerprint, subject_id, predicate,
                         object_id, object_kind, confidence, reason_code, reason,
                         provenance_json, status, created_at_ms, updated_at_ms)
                     VALUES
                        ('review-solo-weak', 'fp-solo-weak', 'solo', 'has',
                         'weak literal', 'literal', 0.5, 'weak_literal_claim',
                         'needs rewrite', '{}', 'needs_review', ?1, ?1)",
                    rusqlite::params![now_ms],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .await
            .unwrap();

            let result = memory_context_inner(
                &embedder,
                &hnsw,
                &pool,
                "solo graph",
                Some("solo"),
                &[],
                None,
                5,
            )
            .await
            .unwrap();

            assert_eq!(result.resolved_subject.as_deref(), Some("solo"));
            assert_eq!(result.sections.graph.status, "ok");
            assert_eq!(result.graph.seed_entities[0], "solo");
            let ollama_fact = result
                .graph
                .relationship_facts
                .iter()
                .find(|fact| fact.subject_id == "solo" && fact.object_id == "ollama")
                .expect("ollama graph fact");
            assert_eq!(ollama_fact.evidence_count, 2);
            assert_eq!(ollama_fact.direction, "outgoing");
            assert!(ollama_fact.score > 0.0);
            assert!(
                ollama_fact
                    .reason_codes
                    .iter()
                    .any(|code| code == "graph_neighbor")
            );
            assert!(
                ollama_fact
                    .reason_codes
                    .iter()
                    .any(|code| code == "evidence_backed")
            );
            assert_eq!(
                ollama_fact.evidence_preview.as_deref(),
                Some("High confidence evidence should be selected.")
            );
            assert!(
                result
                    .graph
                    .relationship_facts
                    .iter()
                    .any(|fact| fact.subject_id == "solo" && fact.object_id == "ollama")
            );
            assert!(
                result
                    .graph
                    .literal_facts
                    .iter()
                    .any(|fact| fact.object_id == "graph_alive")
            );
            let literal_fact = result
                .graph
                .literal_facts
                .iter()
                .find(|fact| fact.object_id == "graph_alive")
                .expect("literal graph fact");
            assert_eq!(literal_fact.evidence_preview, None);
            assert_eq!(literal_fact.memory_id, None);
            assert_eq!(literal_fact.source_episode_id, None);
            assert!(
                literal_fact
                    .reason_codes
                    .iter()
                    .any(|code| code == "literal_fact")
            );
            assert!(
                result
                    .graph
                    .relationship_paths
                    .iter()
                    .any(|path| { path.hops == 1 && path.path_text == "solo -[uses]-> ollama" }),
                "expected direct relationship path: {:?}",
                result.graph.relationship_paths
            );
            assert!(
                result.graph.relationship_paths.iter().any(|path| {
                    path.hops == 2 && path.path_text == "solo -[uses]-> ollama -[serves]-> nomic"
                }),
                "expected two-hop relationship path: {:?}",
                result.graph.relationship_paths
            );
            let forgotten_path = result
                .graph
                .relationship_paths
                .iter()
                .find(|path| path.path_text == "solo -[references]-> nomic")
                .expect("path backed only by forgotten evidence");
            assert_eq!(forgotten_path.edges[0].evidence_memory_id, None);
            let two_hop = result
                .graph
                .relationship_paths
                .iter()
                .find(|path| {
                    path.hops == 2 && path.path_text == "solo -[uses]-> ollama -[serves]-> nomic"
                })
                .expect("two-hop path");
            assert!(
                two_hop
                    .reason_codes
                    .iter()
                    .any(|code| code == "two_hop_relationship_path")
            );
            assert_eq!(result.graph.review_warnings.len(), 1);
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }
}
