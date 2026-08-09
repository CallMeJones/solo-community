// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;

pub(crate) const GRAPH_RELATIONSHIP_EVIDENCE_PREVIEW_CHARS: i64 = 240;
pub(crate) const GRAPH_RELATIONSHIP_EVIDENCE_LIMIT: i64 = 100;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphRelationshipInspectResponse {
    pub(crate) edge: GraphRelationshipEdgeDetail,
    pub(crate) evidence: Vec<GraphRelationshipEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphRelationshipEdgeDetail {
    pub(crate) edge_id: String,
    pub(crate) subject_entity_id: String,
    pub(crate) predicate: String,
    pub(crate) object_entity_id: Option<String>,
    pub(crate) object_literal: Option<String>,
    pub(crate) object_kind: String,
    pub(crate) confidence: f32,
    pub(crate) strength: f32,
    pub(crate) evidence_count: i64,
    pub(crate) valid_from_ms: i64,
    pub(crate) valid_to_ms: Option<i64>,
    pub(crate) status: String,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphRelationshipEvidence {
    pub(crate) evidence_id: String,
    pub(crate) triple_id: String,
    pub(crate) memory_id: Option<String>,
    pub(crate) source_episode_id: Option<i64>,
    pub(crate) doc_id: Option<String>,
    pub(crate) chunk_id: Option<String>,
    pub(crate) cluster_id: Option<String>,
    pub(crate) extraction_confidence: f32,
    pub(crate) created_at_ms: i64,
    pub(crate) preview: Option<String>,
}

pub(crate) fn inspect_graph_relationship(
    conn: &rusqlite::Connection,
    edge_id: &str,
) -> rusqlite::Result<Option<GraphRelationshipInspectResponse>> {
    let edge: Option<GraphRelationshipEdgeDetail> = match conn.query_row(
        "SELECT edge_id,
                subject_entity_id,
                predicate,
                object_entity_id,
                object_literal,
                object_kind,
                confidence,
                strength,
                evidence_count,
                valid_from_ms,
                valid_to_ms,
                status,
                created_at_ms,
                updated_at_ms
           FROM relationship_edges
          WHERE edge_id = ?1
            AND status = 'active'",
        rusqlite::params![edge_id],
        |r| {
            Ok(GraphRelationshipEdgeDetail {
                edge_id: r.get(0)?,
                subject_entity_id: r.get(1)?,
                predicate: r.get(2)?,
                object_entity_id: r.get(3)?,
                object_literal: r.get(4)?,
                object_kind: r.get(5)?,
                confidence: r.get(6)?,
                strength: r.get(7)?,
                evidence_count: r.get(8)?,
                valid_from_ms: r.get(9)?,
                valid_to_ms: r.get(10)?,
                status: r.get(11)?,
                created_at_ms: r.get(12)?,
                updated_at_ms: r.get(13)?,
            })
        },
    ) {
        Ok(edge) => Some(edge),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(err),
    };
    let Some(edge) = edge else {
        return Ok(None);
    };

    let mut stmt = conn.prepare(
        "SELECT ev.evidence_id,
                ev.triple_id,
                ev.memory_id,
                ev.source_episode_id,
                ev.doc_id,
                ev.chunk_id,
                ev.cluster_id,
                ev.extraction_confidence,
                ev.created_at_ms,
                substr(ep.content, 1, ?2) AS preview
           FROM relationship_evidence ev
           LEFT JOIN episodes ep
             ON ep.status = 'active'
            AND (
                (ev.source_episode_id IS NOT NULL
                 AND ep.rowid = ev.source_episode_id)
                OR (ev.source_episode_id IS NULL
                    AND ev.memory_id IS NOT NULL
                    AND ep.memory_id = ev.memory_id)
            )
          WHERE ev.edge_id = ?1
          ORDER BY ev.extraction_confidence DESC,
                   ev.created_at_ms DESC,
                   ev.evidence_id ASC
          LIMIT ?3",
    )?;
    let evidence = stmt
        .query_map(
            rusqlite::params![
                &edge.edge_id,
                GRAPH_RELATIONSHIP_EVIDENCE_PREVIEW_CHARS,
                GRAPH_RELATIONSHIP_EVIDENCE_LIMIT
            ],
            |r| {
                Ok(GraphRelationshipEvidence {
                    evidence_id: r.get(0)?,
                    triple_id: r.get(1)?,
                    memory_id: r.get(2)?,
                    source_episode_id: r.get(3)?,
                    doc_id: r.get(4)?,
                    chunk_id: r.get(5)?,
                    cluster_id: r.get(6)?,
                    extraction_confidence: r.get(7)?,
                    created_at_ms: r.get(8)?,
                    preview: r.get(9)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Some(GraphRelationshipInspectResponse { edge, evidence }))
}
