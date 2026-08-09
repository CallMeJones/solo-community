// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;

pub(crate) const GRAPH_PATHS_DEFAULT_LIMIT: u32 = 25;
pub(crate) const GRAPH_PATHS_MAX_LIMIT: u32 = 100;
pub(crate) const GRAPH_PATHS_DEFAULT_MAX_HOPS: u8 = 2;
pub(crate) const GRAPH_PATHS_MAX_HOPS: u8 = 2;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphPath {
    pub(crate) nodes: Vec<String>,
    pub(crate) edges: Vec<GraphPathStep>,
    pub(crate) hops: u8,
    pub(crate) score: f32,
    pub(crate) reason_codes: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphPathStep {
    pub(crate) edge_id: String,
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) predicate: String,
    pub(crate) confidence: f32,
    pub(crate) strength: f32,
    pub(crate) evidence_count: i64,
    pub(crate) valid_from_ms: i64,
    pub(crate) valid_to_ms: Option<i64>,
    pub(crate) status: String,
    pub(crate) evidence_memory_id: Option<String>,
}

pub(crate) fn parse_graph_path_entity_param(name: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let (prefix, value) = trimmed
        .split_once(':')
        .ok_or_else(|| format!("{name} must be an entity node id (`ent:<value>`); got {raw:?}"))?;
    if prefix != "ent" || value.is_empty() {
        return Err(format!(
            "{name} must be an entity node id (`ent:<value>`); got {raw:?}"
        ));
    }
    Ok(value.to_string())
}

fn graph_path_step_score(step: &GraphPathStep) -> f32 {
    step.confidence.max(0.0) * step.strength.max(0.0)
}

fn build_graph_path(steps: Vec<GraphPathStep>, temporal: bool) -> GraphPath {
    let mut nodes = Vec::with_capacity(steps.len() + 1);
    if let Some(first) = steps.first() {
        nodes.push(first.source.clone());
    }
    for step in &steps {
        nodes.push(step.target.clone());
    }

    let score = if steps.is_empty() {
        0.0
    } else {
        let product = steps
            .iter()
            .map(graph_path_step_score)
            .fold(1.0_f32, |acc, score| acc * score);
        product.powf(1.0 / steps.len() as f32)
    };
    let hops = steps.len() as u8;
    let mut reason_codes = Vec::with_capacity(3);
    reason_codes.push(if hops == 1 {
        "direct_relationship"
    } else {
        "two_hop_relationship_path"
    });
    reason_codes.push("evidence_backed");
    if temporal {
        reason_codes.push("temporal_match");
    }

    GraphPath {
        nodes,
        edges: steps,
        hops,
        score,
        reason_codes,
    }
}

fn fetch_graph_path_step(
    conn: &rusqlite::Connection,
    edge_id: &str,
) -> rusqlite::Result<Option<GraphPathStep>> {
    match conn.query_row(
        "SELECT re.edge_id,
                re.subject_entity_id,
                re.object_entity_id,
                re.predicate,
                re.confidence,
                re.strength,
                re.evidence_count,
                re.valid_from_ms,
                re.valid_to_ms,
                re.status,
                ev.memory_id AS evidence_memory_id
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
          WHERE re.edge_id = ?1
            AND re.status = 'active'
            AND re.object_kind = 'entity'",
        rusqlite::params![edge_id],
        |r| {
            let source: String = r.get(1)?;
            let target: String = r.get(2)?;
            Ok(GraphPathStep {
                edge_id: r.get(0)?,
                source: format!("ent:{source}"),
                target: format!("ent:{target}"),
                predicate: r.get(3)?,
                confidence: r.get(4)?,
                strength: r.get(5)?,
                evidence_count: r.get(6)?,
                valid_from_ms: r.get(7)?,
                valid_to_ms: r.get(8)?,
                status: r.get(9)?,
                evidence_memory_id: r.get(10)?,
            })
        },
    ) {
        Ok(step) => Ok(Some(step)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(err),
    }
}

fn fetch_direct_graph_path_edge_ids(
    conn: &rusqlite::Connection,
    from_entity_id: &str,
    to_entity_id: &str,
    as_of_ms: Option<i64>,
    limit: i64,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT re.edge_id
           FROM relationship_edges re
          WHERE re.status = 'active'
            AND re.object_kind = 'entity'
            AND re.subject_entity_id = ?1
            AND re.object_entity_id = ?2
            AND (?3 IS NULL OR (
                re.valid_from_ms <= ?3
                AND (re.valid_to_ms IS NULL OR re.valid_to_ms > ?3)
            ))
          ORDER BY (re.strength * re.confidence) DESC,
                   re.edge_id ASC
          LIMIT ?4",
    )?;
    stmt.query_map(
        rusqlite::params![from_entity_id, to_entity_id, as_of_ms, limit],
        |r| r.get(0),
    )?
    .collect()
}

fn fetch_two_hop_graph_path_edge_ids(
    conn: &rusqlite::Connection,
    from_entity_id: &str,
    to_entity_id: &str,
    as_of_ms: Option<i64>,
    limit: i64,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT a.edge_id,
                b.edge_id
           FROM relationship_edges a
           JOIN relationship_edges b
             ON b.subject_entity_id = a.object_entity_id
          WHERE a.status = 'active'
            AND b.status = 'active'
            AND a.object_kind = 'entity'
            AND b.object_kind = 'entity'
            AND a.subject_entity_id = ?1
            AND b.object_entity_id = ?2
            AND a.object_entity_id <> ?1
            AND a.object_entity_id <> ?2
            AND (?3 IS NULL OR (
                a.valid_from_ms <= ?3
                AND (a.valid_to_ms IS NULL OR a.valid_to_ms > ?3)
                AND b.valid_from_ms <= ?3
                AND (b.valid_to_ms IS NULL OR b.valid_to_ms > ?3)
            ))
          ORDER BY ((a.strength * a.confidence) * (b.strength * b.confidence)) DESC,
                   a.edge_id ASC,
                   b.edge_id ASC
          LIMIT ?4",
    )?;
    stmt.query_map(
        rusqlite::params![from_entity_id, to_entity_id, as_of_ms, limit],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?
    .collect()
}

pub(crate) fn fetch_graph_relationship_paths(
    conn: &rusqlite::Connection,
    from_entity_id: &str,
    to_entity_id: &str,
    max_hops: u8,
    as_of_ms: Option<i64>,
    limit: u32,
) -> rusqlite::Result<Vec<GraphPath>> {
    let mut paths = Vec::new();
    let limit_i64 = i64::from(limit);
    for edge_id in
        fetch_direct_graph_path_edge_ids(conn, from_entity_id, to_entity_id, as_of_ms, limit_i64)?
    {
        if let Some(step) = fetch_graph_path_step(conn, &edge_id)? {
            paths.push(build_graph_path(vec![step], as_of_ms.is_some()));
        }
    }

    if max_hops >= 2 {
        for (first_edge_id, second_edge_id) in fetch_two_hop_graph_path_edge_ids(
            conn,
            from_entity_id,
            to_entity_id,
            as_of_ms,
            limit_i64,
        )? {
            let Some(first_step) = fetch_graph_path_step(conn, &first_edge_id)? else {
                continue;
            };
            let Some(second_step) = fetch_graph_path_step(conn, &second_edge_id)? else {
                continue;
            };
            paths.push(build_graph_path(
                vec![first_step, second_step],
                as_of_ms.is_some(),
            ));
        }
    }

    paths.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.hops.cmp(&b.hops))
            .then_with(|| {
                let a_key = a
                    .edges
                    .iter()
                    .map(|edge| edge.edge_id.as_str())
                    .collect::<Vec<_>>()
                    .join("\u{1f}");
                let b_key = b
                    .edges
                    .iter()
                    .map(|edge| edge.edge_id.as_str())
                    .collect::<Vec<_>>()
                    .join("\u{1f}");
                a_key.cmp(&b_key)
            })
    });
    paths.truncate(limit as usize);
    Ok(paths)
}
