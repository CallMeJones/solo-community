// SPDX-License-Identifier: Apache-2.0

//! Quality gate for derived semantic triples.
//!
//! This module is deliberately deterministic and conservative: raw memories
//! remain untouched, while weak extracted triples are either normalized into
//! active graph facts or quarantined for review.

use rusqlite::{Transaction, params};
use solo_core::{Triple, TripleObjectKind};

pub const MIN_ACTIVE_TRIPLE_CONFIDENCE: f32 = 0.85;

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveTriple {
    pub triple_id: String,
    pub subject_id: String,
    pub subject_alias: String,
    pub predicate: String,
    pub object_id: String,
    pub object_alias: Option<String>,
    pub object_kind: &'static str,
    pub valid_from_ms: i64,
    pub valid_to_ms: Option<i64>,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TripleReviewCandidate {
    pub review_id: String,
    pub candidate_fingerprint: String,
    pub triple_id: String,
    pub cluster_id: String,
    pub source_episode_id: Option<i64>,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: &'static str,
    pub confidence: f32,
    pub reason_code: &'static str,
    pub reason: String,
    pub provenance_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TripleQualityDecision {
    Active(ActiveTriple),
    Review(TripleReviewCandidate),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TripleQualityReviewReason {
    pub reason_code: &'static str,
    pub reason: String,
}

pub fn evaluate_extracted_triple(
    triple: &Triple,
    cluster_id: &str,
    source_episode_id: Option<i64>,
    provenance_json: String,
) -> TripleQualityDecision {
    let raw_subject = clean_label(&triple.subject_id);
    let raw_predicate = clean_label(&triple.predicate);
    let raw_object = clean_label(&triple.object_id);
    let input_kind = match triple.object_kind {
        TripleObjectKind::Entity => "entity",
        TripleObjectKind::Literal => "literal",
    };

    let review = |reason_code: &'static str, reason: String| {
        TripleQualityDecision::Review(review_candidate(
            triple,
            cluster_id,
            source_episode_id,
            raw_subject.clone(),
            raw_predicate.clone(),
            raw_object.clone(),
            input_kind,
            reason_code,
            reason,
            provenance_json.clone(),
        ))
    };

    if raw_subject.is_empty() || raw_predicate.is_empty() || raw_object.is_empty() {
        return review(
            "incomplete_shape",
            "subject, predicate, and object must all be present".to_string(),
        );
    }

    if looks_like_assistant_or_tool_chatter(&raw_subject)
        || looks_like_assistant_or_tool_chatter(&raw_predicate)
        || looks_like_assistant_or_tool_chatter(&raw_object)
    {
        return review(
            "assistant_or_tool_chatter",
            "assistant/refusal/tool chatter is not durable user memory".to_string(),
        );
    }

    if triple.confidence.0 < MIN_ACTIVE_TRIPLE_CONFIDENCE {
        return review(
            "low_confidence",
            format!(
                "confidence {:.2} is below the active fact floor {:.2}",
                triple.confidence.0, MIN_ACTIVE_TRIPLE_CONFIDENCE
            ),
        );
    }

    if looks_like_long_underscore_blob(&raw_subject)
        || looks_like_long_underscore_blob(&raw_predicate)
        || looks_like_long_underscore_blob(&raw_object)
    {
        return review(
            "long_machine_blob",
            "long underscore-heavy machine blobs are too brittle for active facts".to_string(),
        );
    }

    if subject_is_metadata_value(&raw_subject) {
        return review(
            "metadata_subject",
            "paths, URLs, branches, and commit-like values cannot be fact subjects".to_string(),
        );
    }

    let predicate = normalize_predicate(&raw_predicate);
    if !useful_predicate(&predicate) {
        return review(
            "weak_predicate",
            format!("predicate {raw_predicate:?} is too vague or malformed"),
        );
    }

    let subject_id = canonical_entity_id(&raw_subject);
    if subject_id.is_empty() {
        return review(
            "weak_subject",
            format!("subject {raw_subject:?} does not normalize to a useful entity"),
        );
    }

    let mut object_kind = input_kind;
    let object_id;
    let object_alias;
    if input_kind == "entity" {
        if object_is_metadata_literal(&raw_object) {
            object_kind = "literal";
            object_id = raw_object.clone();
            object_alias = None;
        } else {
            object_id = canonical_entity_id(&raw_object);
            object_alias = Some(raw_object.clone());
            if object_id.is_empty() {
                return review(
                    "weak_object",
                    format!("object {raw_object:?} does not normalize to a useful entity"),
                );
            }
        }
    } else {
        object_id = raw_object.clone();
        object_alias = None;
    }

    if object_kind == "literal" && !useful_literal(&object_id) {
        return review(
            "weak_literal",
            "literal object is too short, too long, or too machine-shaped".to_string(),
        );
    }
    if object_kind == "literal" {
        if let Some(reason) = literal_review_reason(&subject_id, &predicate, &object_id, false) {
            return review(reason.reason_code, reason.reason);
        }
    }

    TripleQualityDecision::Active(ActiveTriple {
        triple_id: triple.triple_id.to_string(),
        subject_id,
        subject_alias: raw_subject,
        predicate,
        object_id,
        object_alias,
        object_kind,
        valid_from_ms: triple.valid_from_ms,
        valid_to_ms: triple.valid_to_ms,
        confidence: triple.confidence.0,
    })
}

pub fn active_triple_review_reason(
    subject_id: &str,
    predicate: &str,
    object_id: &str,
    object_kind: &str,
    confidence: f32,
    literal_only_subject: bool,
) -> Option<TripleQualityReviewReason> {
    let raw_subject = clean_label(subject_id);
    let raw_predicate = clean_label(predicate);
    let raw_object = clean_label(object_id);

    if raw_subject.is_empty() || raw_predicate.is_empty() || raw_object.is_empty() {
        return Some(review_reason(
            "incomplete_shape",
            "subject, predicate, and object must all be present",
        ));
    }
    if looks_like_assistant_or_tool_chatter(&raw_subject)
        || looks_like_assistant_or_tool_chatter(&raw_predicate)
        || looks_like_assistant_or_tool_chatter(&raw_object)
    {
        return Some(review_reason(
            "assistant_or_tool_chatter",
            "assistant/refusal/tool chatter is not durable user memory",
        ));
    }
    if confidence < MIN_ACTIVE_TRIPLE_CONFIDENCE {
        return Some(review_reason(
            "low_confidence",
            format!(
                "confidence {confidence:.2} is below the active fact floor \
                 {MIN_ACTIVE_TRIPLE_CONFIDENCE:.2}"
            ),
        ));
    }
    if looks_like_long_underscore_blob(&raw_subject)
        || looks_like_long_underscore_blob(&raw_predicate)
        || looks_like_long_underscore_blob(&raw_object)
    {
        return Some(review_reason(
            "long_machine_blob",
            "long underscore-heavy machine blobs are too brittle for active facts",
        ));
    }
    if subject_is_metadata_value(&raw_subject) {
        return Some(review_reason(
            "metadata_subject",
            "paths, URLs, branches, and commit-like values cannot be fact subjects",
        ));
    }
    if canonical_entity_id(&raw_subject).is_empty() {
        return Some(review_reason(
            "weak_subject",
            format!("subject {raw_subject:?} does not normalize to a useful entity"),
        ));
    }

    let normalized_predicate = normalize_predicate(&raw_predicate);
    if !useful_predicate(&normalized_predicate) {
        return Some(review_reason(
            "weak_predicate",
            format!("predicate {raw_predicate:?} is too vague or malformed"),
        ));
    }

    match object_kind {
        "entity" => {
            if canonical_entity_id(&raw_object).is_empty() {
                return Some(review_reason(
                    "weak_object",
                    format!("object {raw_object:?} does not normalize to a useful entity"),
                ));
            }
            None
        }
        "literal" => {
            if !useful_literal(&raw_object) {
                return Some(review_reason(
                    "weak_literal",
                    "literal object is too short, too long, or too machine-shaped",
                ));
            }
            literal_review_reason(
                &canonical_entity_id(&raw_subject),
                &normalized_predicate,
                &raw_object,
                literal_only_subject,
            )
        }
        _ => Some(review_reason(
            "invalid_object_kind",
            format!("object_kind {object_kind:?} must be entity or literal"),
        )),
    }
}

pub fn active_entity_object_should_be_literal(object_id: &str) -> bool {
    object_is_metadata_literal(&clean_label(object_id))
}

pub fn insert_active_triple_in_tx(
    tx: &Transaction<'_>,
    triple: &ActiveTriple,
    cluster_id: &str,
    source_episode_id: Option<i64>,
    provenance_json: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    insert_active_triple_with_optional_cluster_in_tx(
        tx,
        triple,
        Some(cluster_id),
        source_episode_id,
        provenance_json,
        now_ms,
    )
}

pub fn insert_active_triple_with_optional_cluster_in_tx(
    tx: &Transaction<'_>,
    triple: &ActiveTriple,
    cluster_id: Option<&str>,
    source_episode_id: Option<i64>,
    provenance_json: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    upsert_entities_for_active_triple_in_tx(tx, triple, now_ms)?;

    tx.execute(
        "INSERT INTO triples
            (triple_id, subject_id, predicate, object_id,
             object_kind, valid_from_ms, valid_to_ms,
             confidence, provenance_json,
             created_at_ms, updated_at_ms, cluster_id,
             source_episode_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            triple.triple_id,
            triple.subject_id,
            triple.predicate,
            triple.object_id,
            triple.object_kind,
            triple.valid_from_ms,
            triple.valid_to_ms,
            triple.confidence,
            provenance_json,
            now_ms,
            now_ms,
            cluster_id,
            source_episode_id,
        ],
    )?;
    upsert_relationship_edge_for_active_triple_in_tx(
        tx,
        triple,
        cluster_id,
        source_episode_id,
        now_ms,
    )?;
    insert_memory_claim_for_active_triple_in_tx(
        tx,
        triple,
        cluster_id,
        source_episode_id,
        provenance_json,
        now_ms,
    )?;
    Ok(())
}

pub fn upsert_graph_for_active_triple_in_tx(
    tx: &Transaction<'_>,
    triple: &ActiveTriple,
    cluster_id: Option<&str>,
    source_episode_id: Option<i64>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    upsert_entities_for_active_triple_in_tx(tx, triple, now_ms)?;
    upsert_relationship_edge_for_active_triple_in_tx(
        tx,
        triple,
        cluster_id,
        source_episode_id,
        now_ms,
    )
}

fn upsert_entities_for_active_triple_in_tx(
    tx: &Transaction<'_>,
    triple: &ActiveTriple,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let subject_display = display_label_for_alias(&triple.subject_alias);
    upsert_entity_alias_in_tx(
        tx,
        &triple.subject_alias,
        &triple.subject_id,
        subject_display.clone(),
        triple.confidence,
        now_ms,
    )?;
    upsert_entity_alias_in_tx(
        tx,
        &triple.subject_id,
        &triple.subject_id,
        subject_display.clone(),
        1.0,
        now_ms,
    )?;
    upsert_entity_in_tx(
        tx,
        &triple.subject_id,
        subject_display,
        triple.confidence,
        triple.valid_from_ms,
        triple.valid_to_ms,
        now_ms,
    )?;
    if let Some(alias) = triple.object_alias.as_deref() {
        let object_display = display_label_for_alias(alias);
        upsert_entity_alias_in_tx(
            tx,
            alias,
            &triple.object_id,
            object_display.clone(),
            triple.confidence,
            now_ms,
        )?;
        upsert_entity_alias_in_tx(
            tx,
            &triple.object_id,
            &triple.object_id,
            object_display.clone(),
            1.0,
            now_ms,
        )?;
        upsert_entity_in_tx(
            tx,
            &triple.object_id,
            object_display,
            triple.confidence,
            triple.valid_from_ms,
            triple.valid_to_ms,
            now_ms,
        )?;
    }
    Ok(())
}

pub fn insert_triple_review_in_tx(
    tx: &Transaction<'_>,
    candidate: &TripleReviewCandidate,
    now_ms: i64,
) -> rusqlite::Result<()> {
    insert_memory_claim_for_review_candidate_in_tx(tx, candidate, now_ms)?;
    tx.execute(
        "INSERT INTO triple_reviews
            (review_id, candidate_fingerprint, triple_id, cluster_id,
             source_episode_id, subject_id, predicate, object_id, object_kind,
             confidence, reason_code, reason, provenance_json,
             created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(candidate_fingerprint) DO UPDATE SET
             triple_id = excluded.triple_id,
             source_episode_id = excluded.source_episode_id,
             subject_id = excluded.subject_id,
             predicate = excluded.predicate,
             object_id = excluded.object_id,
             object_kind = excluded.object_kind,
             confidence = excluded.confidence,
             reason_code = excluded.reason_code,
             reason = excluded.reason,
             provenance_json = excluded.provenance_json,
             status = CASE
                 WHEN triple_reviews.status = 'needs_review' THEN 'needs_review'
                 ELSE triple_reviews.status
             END,
             updated_at_ms = excluded.updated_at_ms",
        params![
            candidate.review_id,
            candidate.candidate_fingerprint,
            candidate.triple_id,
            candidate.cluster_id,
            candidate.source_episode_id,
            candidate.subject_id,
            candidate.predicate,
            candidate.object_id,
            candidate.object_kind,
            candidate.confidence,
            candidate.reason_code,
            candidate.reason,
            candidate.provenance_json,
            now_ms,
            now_ms,
        ],
    )?;
    Ok(())
}

fn insert_memory_claim_for_active_triple_in_tx(
    tx: &Transaction<'_>,
    triple: &ActiveTriple,
    cluster_id: Option<&str>,
    source_episode_id: Option<i64>,
    provenance_json: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let fingerprint = active_claim_fingerprint(triple);
    let claim_id = format!("mc-{fingerprint}");
    let reason_codes = serde_json::json!([
        "confidence_floor_met",
        "durable_subject",
        "useful_predicate",
        if triple.object_kind == "entity" {
            "entity_relationship"
        } else {
            "literal_fact"
        },
        "activated"
    ])
    .to_string();
    tx.execute(
        "INSERT INTO memory_claims
            (claim_id, candidate_fingerprint, triple_id, review_id, subject_id,
             predicate, object_id, object_kind, source_type, cluster_id,
             source_episode_id, confidence, quality_score, status,
             reason_codes_json, evidence_count, user_approved, created_at_ms,
             activated_at_ms, reviewed_at_ms, updated_at_ms, provenance_json)
         VALUES
            (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, 'steward_triple', ?8,
             ?9, ?10, ?11, 'active', ?12, 1, 0, ?13, ?13, NULL, ?13, ?14)
         ON CONFLICT(candidate_fingerprint) DO UPDATE SET
             triple_id = excluded.triple_id,
             review_id = NULL,
             subject_id = excluded.subject_id,
             predicate = excluded.predicate,
             object_id = excluded.object_id,
             object_kind = excluded.object_kind,
             cluster_id = excluded.cluster_id,
             source_episode_id = excluded.source_episode_id,
             confidence = excluded.confidence,
             quality_score = excluded.quality_score,
             status = 'active',
             reason_codes_json = excluded.reason_codes_json,
             evidence_count = excluded.evidence_count,
             activated_at_ms = excluded.activated_at_ms,
             updated_at_ms = excluded.updated_at_ms,
             provenance_json = excluded.provenance_json",
        params![
            claim_id,
            fingerprint,
            triple.triple_id,
            triple.subject_id,
            triple.predicate,
            triple.object_id,
            triple.object_kind,
            cluster_id,
            source_episode_id,
            triple.confidence,
            active_claim_quality_score(triple),
            reason_codes,
            now_ms,
            provenance_json,
        ],
    )?;
    Ok(())
}

fn insert_memory_claim_for_review_candidate_in_tx(
    tx: &Transaction<'_>,
    candidate: &TripleReviewCandidate,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let status = claim_status_for_review_reason(candidate.reason_code);
    let quality_score = review_claim_quality_score(candidate.confidence, candidate.reason_code);
    let reason_codes = serde_json::json!([
        candidate.reason_code,
        status,
        "quality_gate",
        "not_activated"
    ])
    .to_string();
    let claim_id = format!("mc-{}", candidate.candidate_fingerprint);
    tx.execute(
        "INSERT INTO memory_claims
            (claim_id, candidate_fingerprint, triple_id, review_id, subject_id,
             predicate, object_id, object_kind, source_type, cluster_id,
             source_episode_id, confidence, quality_score, status,
             reason_codes_json, evidence_count, user_approved, created_at_ms,
             activated_at_ms, reviewed_at_ms, updated_at_ms, provenance_json)
         VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'steward_triple', ?9,
             ?10, ?11, ?12, ?13, ?14, 1, 0, ?15, NULL, NULL, ?15, ?16)
         ON CONFLICT(candidate_fingerprint) DO UPDATE SET
             triple_id = excluded.triple_id,
             review_id = excluded.review_id,
             subject_id = excluded.subject_id,
             predicate = excluded.predicate,
             object_id = excluded.object_id,
             object_kind = excluded.object_kind,
             cluster_id = excluded.cluster_id,
             source_episode_id = excluded.source_episode_id,
             confidence = excluded.confidence,
             quality_score = excluded.quality_score,
             status = CASE
                 WHEN memory_claims.user_approved = 1 THEN memory_claims.status
                 ELSE excluded.status
             END,
             reason_codes_json = excluded.reason_codes_json,
             updated_at_ms = excluded.updated_at_ms,
             provenance_json = excluded.provenance_json",
        params![
            claim_id,
            candidate.candidate_fingerprint,
            candidate.triple_id,
            candidate.review_id,
            candidate.subject_id,
            candidate.predicate,
            candidate.object_id,
            candidate.object_kind,
            candidate.cluster_id,
            candidate.source_episode_id,
            candidate.confidence,
            quality_score,
            status,
            reason_codes,
            now_ms,
            candidate.provenance_json,
        ],
    )?;
    Ok(())
}

fn active_claim_fingerprint(triple: &ActiveTriple) -> String {
    let raw = format!(
        "active|{}|{}|{}|{}|{}",
        triple.triple_id, triple.subject_id, triple.predicate, triple.object_id, triple.object_kind
    );
    fnv1a64_hex(&raw)
}

fn active_claim_quality_score(triple: &ActiveTriple) -> f32 {
    let shape_boost = if triple.object_kind == "entity" {
        0.08
    } else {
        0.04
    };
    (triple.confidence * 0.85 + shape_boost).clamp(0.0, 1.0)
}

pub fn claim_status_for_review_reason(reason_code: &str) -> &'static str {
    match reason_code {
        "assistant_or_tool_chatter" | "long_machine_blob" => "quarantined",
        "metadata_subject" | "incomplete_shape" | "invalid_object_kind" => "rejected",
        _ => "needs_review",
    }
}

pub fn review_claim_quality_score(confidence: f32, reason_code: &str) -> f32 {
    let penalty = match reason_code {
        "assistant_or_tool_chatter" => 0.90,
        "long_machine_blob" => 0.80,
        "metadata_subject" | "incomplete_shape" | "invalid_object_kind" => 1.00,
        "literal_only_machine_entity"
        | "machine_literal"
        | "long_literal_evidence"
        | "weak_literal_claim" => 0.35,
        "low_confidence" => 0.25,
        "weak_predicate" | "weak_subject" | "weak_object" | "weak_literal" => 0.20,
        _ => 0.15,
    };
    (confidence - penalty).clamp(0.0, 1.0)
}

pub fn canonical_entity_id(raw: &str) -> String {
    let label = clean_label(raw);
    let mut out = String::with_capacity(label.len());
    let mut last_was_sep = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if ch.is_whitespace() || matches!(ch, '_' | '-' | '.' | '/' | '\\') {
            if !last_was_sep && !out.is_empty() {
                out.push('-');
                last_was_sep = true;
            }
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

pub fn looks_like_machine_identifier(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if value != lower {
        return false;
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
    {
        return false;
    }
    let separators = value
        .chars()
        .filter(|c| matches!(c, '_' | '-' | '.'))
        .count();
    let digits = value.chars().filter(|c| c.is_ascii_digit()).count();
    let len = value.chars().count();
    (len >= 24 && separators >= 1) || separators >= 3 || digits >= 6
}

fn upsert_entity_alias_in_tx(
    tx: &Transaction<'_>,
    alias: &str,
    canonical_id: &str,
    display_label: String,
    confidence: f32,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let alias = clean_label(alias);
    if alias.is_empty() || canonical_id.is_empty() {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO entity_aliases
            (alias, canonical_id, display_label, source, confidence, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, 'triple_quality_gate', ?, ?, ?)
         ON CONFLICT(alias) DO UPDATE SET
             canonical_id = excluded.canonical_id,
             display_label = excluded.display_label,
             confidence = MAX(entity_aliases.confidence, excluded.confidence),
             updated_at_ms = excluded.updated_at_ms",
        params![
            alias,
            canonical_id,
            display_label,
            confidence,
            now_ms,
            now_ms
        ],
    )?;
    Ok(())
}

fn upsert_entity_in_tx(
    tx: &Transaction<'_>,
    entity_id: &str,
    canonical_name: String,
    confidence: f32,
    valid_from_ms: i64,
    valid_to_ms: Option<i64>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    if entity_id.is_empty() {
        return Ok(());
    }
    let last_seen_ms = valid_to_ms.unwrap_or(valid_from_ms);
    tx.execute(
        "INSERT INTO entities
            (entity_id, canonical_name, entity_type, aliases_json, confidence,
             first_seen_ms, last_seen_ms, status, created_at_ms, updated_at_ms)
         VALUES (?, ?, 'unknown', '[]', ?, ?, ?, 'active', ?, ?)
         ON CONFLICT(entity_id) DO UPDATE SET
             canonical_name = CASE
                 WHEN entities.canonical_name = entities.entity_id THEN excluded.canonical_name
                 ELSE entities.canonical_name
             END,
             confidence = MAX(entities.confidence, excluded.confidence),
             first_seen_ms = MIN(entities.first_seen_ms, excluded.first_seen_ms),
             last_seen_ms = MAX(entities.last_seen_ms, excluded.last_seen_ms),
             status = CASE
                 WHEN entities.status = 'candidate' THEN 'active'
                 ELSE entities.status
             END,
             updated_at_ms = excluded.updated_at_ms",
        params![
            entity_id,
            canonical_name,
            confidence,
            valid_from_ms,
            last_seen_ms,
            now_ms,
            now_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn upsert_relationship_edge_for_active_triple_in_tx(
    tx: &Transaction<'_>,
    triple: &ActiveTriple,
    cluster_id: Option<&str>,
    source_episode_id: Option<i64>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let (object_entity_id, object_literal): (Option<&str>, Option<&str>) =
        if triple.object_kind == "entity" {
            (Some(triple.object_id.as_str()), None)
        } else {
            (None, Some(triple.object_id.as_str()))
        };

    tx.execute(
        "INSERT INTO relationship_edges
            (edge_id, subject_entity_id, predicate, object_entity_id, object_literal,
             object_kind, valid_from_ms, valid_to_ms, confidence, strength,
             evidence_count, status, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 'active', ?, ?)
         ON CONFLICT(edge_id) DO UPDATE SET
             subject_entity_id = excluded.subject_entity_id,
             predicate = excluded.predicate,
             object_entity_id = excluded.object_entity_id,
             object_literal = excluded.object_literal,
             object_kind = excluded.object_kind,
             valid_from_ms = excluded.valid_from_ms,
             valid_to_ms = excluded.valid_to_ms,
             confidence = excluded.confidence,
             strength = excluded.strength,
             status = 'active',
             updated_at_ms = excluded.updated_at_ms",
        params![
            triple.triple_id,
            triple.subject_id,
            triple.predicate,
            object_entity_id,
            object_literal,
            triple.object_kind,
            triple.valid_from_ms,
            triple.valid_to_ms,
            triple.confidence,
            triple.confidence,
            now_ms,
            now_ms,
        ],
    )?;

    tx.execute(
        "INSERT INTO relationship_evidence
            (evidence_id, edge_id, triple_id, memory_id, source_episode_id,
             cluster_id, extraction_confidence, created_at_ms)
         VALUES (
             ?, ?, ?,
             (SELECT memory_id FROM episodes WHERE rowid = ?),
             ?, ?, ?, ?
         )
         ON CONFLICT(triple_id) DO UPDATE SET
             edge_id = excluded.edge_id,
             memory_id = excluded.memory_id,
             source_episode_id = excluded.source_episode_id,
             cluster_id = excluded.cluster_id,
             extraction_confidence = excluded.extraction_confidence,
             created_at_ms = excluded.created_at_ms",
        params![
            triple.triple_id,
            triple.triple_id,
            triple.triple_id,
            source_episode_id,
            source_episode_id,
            cluster_id,
            triple.confidence,
            now_ms,
        ],
    )?;
    Ok(())
}

fn review_candidate(
    triple: &Triple,
    cluster_id: &str,
    source_episode_id: Option<i64>,
    subject_id: String,
    predicate: String,
    object_id: String,
    object_kind: &'static str,
    reason_code: &'static str,
    reason: String,
    provenance_json: String,
) -> TripleReviewCandidate {
    let fingerprint =
        format!("{cluster_id}|{subject_id}|{predicate}|{object_id}|{object_kind}|{reason_code}");
    let hash = fnv1a64_hex(&fingerprint);
    TripleReviewCandidate {
        review_id: format!("trv-{hash}"),
        candidate_fingerprint: hash,
        triple_id: triple.triple_id.to_string(),
        cluster_id: cluster_id.to_string(),
        source_episode_id,
        subject_id,
        predicate,
        object_id,
        object_kind,
        confidence: triple.confidence.0,
        reason_code,
        reason,
        provenance_json,
    }
}

fn clean_label(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '[' | ']' | '(' | ')'));
    trimmed
        .strip_prefix("ent:")
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

fn display_label_for_alias(alias: &str) -> String {
    let cleaned = clean_label(alias);
    if cleaned.is_empty() {
        return cleaned;
    }
    cleaned
}

fn normalize_predicate(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_was_sep = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if ch.is_whitespace() || matches!(ch, '-' | '_' | ':' | '/' | '\\') {
            if !last_was_sep && !out.is_empty() {
                out.push('_');
                last_was_sep = true;
            }
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn useful_predicate(predicate: &str) -> bool {
    let len = predicate.chars().count();
    if !(2..=64).contains(&len) {
        return false;
    }
    !matches!(
        predicate,
        "none" | "null" | "unknown" | "n_a" | "na" | "thing" | "stuff"
    )
}

fn useful_literal(value: &str) -> bool {
    let len = value.chars().count();
    if !(1..=240).contains(&len) {
        return false;
    }
    !looks_like_long_underscore_blob(value)
}

fn subject_is_metadata_value(value: &str) -> bool {
    looks_like_path_or_url(value) || looks_like_commit_hash(value) || looks_like_branch_ref(value)
}

fn object_is_metadata_literal(value: &str) -> bool {
    looks_like_path_or_url(value) || looks_like_commit_hash(value) || looks_like_branch_ref(value)
}

fn literal_review_reason(
    subject_id: &str,
    predicate: &str,
    object_id: &str,
    literal_only_subject: bool,
) -> Option<TripleQualityReviewReason> {
    if object_is_metadata_literal(object_id) {
        return None;
    }
    if literal_only_subject && looks_like_machine_identifier(subject_id) {
        return Some(review_reason(
            "literal_only_machine_entity",
            "machine-looking entities with only literal facts should be reviewed or given a human alias",
        ));
    }
    let len = object_id.chars().count();
    if len > 160 {
        return Some(review_reason(
            "long_literal_evidence",
            "long literal evidence should stay in source memory or inspector evidence, not as an active graph fact",
        ));
    }
    if looks_like_machine_literal(object_id) {
        return Some(review_reason(
            "machine_literal",
            "machine-shaped literal values should be reviewed before becoming active graph facts",
        ));
    }
    if weak_literal_predicate(predicate) && len > 80 {
        return Some(review_reason(
            "weak_literal_claim",
            "vague literal facts need a clearer predicate/object shape before activation",
        ));
    }
    None
}

fn review_reason(
    reason_code: &'static str,
    reason: impl Into<String>,
) -> TripleQualityReviewReason {
    TripleQualityReviewReason {
        reason_code,
        reason: reason.into(),
    }
}

fn looks_like_machine_literal(value: &str) -> bool {
    let value = value.trim();
    if value.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let len = value.chars().count();
    if len < 32 {
        return false;
    }
    let separators = value
        .chars()
        .filter(|c| matches!(c, '_' | '-' | '.' | ':' | '/'))
        .count();
    let alnum = value.chars().filter(|c| c.is_ascii_alphanumeric()).count();
    separators >= 2 && alnum >= 12
}

fn weak_literal_predicate(predicate: &str) -> bool {
    matches!(
        predicate,
        "has"
            | "has_value"
            | "has_feature"
            | "has_property"
            | "mentions"
            | "references"
            | "related_to"
            | "is"
    )
}

fn looks_like_path_or_url(value: &str) -> bool {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("file://")
        || lower.starts_with("mcp://")
        || v.starts_with('/')
        || v.starts_with("./")
        || v.starts_with("../")
        || v.starts_with(".\\")
        || v.starts_with("..\\")
        || (v.len() >= 3
            && v.as_bytes().get(1) == Some(&b':')
            && v.as_bytes().get(2) == Some(&b'\\'))
        || (v.contains('\\') && (v.contains(':') || v.contains(".rs") || v.contains(".ts")))
}

fn looks_like_commit_hash(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    let v = lower
        .strip_prefix("commit_")
        .or_else(|| lower.strip_prefix("commit-"))
        .unwrap_or(&lower);
    (7..=64).contains(&v.len()) && v.chars().all(|c| c.is_ascii_hexdigit())
}

fn looks_like_branch_ref(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.starts_with("refs/heads/")
        || lower.starts_with("refs/remotes/")
        || lower.starts_with("origin/")
        || lower.starts_with("feature/")
        || lower.starts_with("fix/")
        || lower.starts_with("bugfix/")
        || lower.starts_with("release/")
}

fn looks_like_assistant_or_tool_chatter(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let subject_noise = matches!(
        lower.as_str(),
        "assistant"
            | "the assistant"
            | "ai assistant"
            | "system"
            | "tool"
            | "tool_call"
            | "tool-call"
            | "tool_output"
            | "tool-output"
            | "tool result"
            | "tool_result"
            | "function_call"
            | "function-call"
            | "function output"
            | "function_output"
    );
    subject_noise
        || (lower.contains("assistant")
            && (lower.contains("unable")
                || lower.contains("cannot")
                || lower.contains("can't")
                || lower.contains("refus")
                || lower.contains("no assistance")))
        || lower.contains("unable to provide assistance")
        || lower.contains("i cannot")
        || lower.contains("i can't")
        || lower.contains("as an ai")
        || lower.contains("tool call")
        || lower.contains("tool output")
        || lower.contains("system prompt")
}

fn looks_like_long_underscore_blob(value: &str) -> bool {
    let len = value.chars().count();
    if len < 72 {
        return false;
    }
    let underscores = value.chars().filter(|c| *c == '_').count();
    let spaces = value.chars().filter(|c| c.is_whitespace()).count();
    underscores >= 5 && spaces <= 2
}

fn fnv1a64_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in input.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_core::{Confidence, MemoryId, Provenance};

    fn triple(
        subject: &str,
        predicate: &str,
        object: &str,
        kind: TripleObjectKind,
        confidence: f32,
    ) -> Triple {
        Triple {
            triple_id: MemoryId::new(),
            subject_id: subject.to_string(),
            predicate: predicate.to_string(),
            object_id: object.to_string(),
            object_kind: kind,
            valid_from_ms: 1,
            valid_to_ms: None,
            confidence: Confidence::new(confidence).unwrap(),
            provenance: Provenance {
                derived_from: vec![],
                derivation: "test".to_string(),
                by: "triple_quality_test".to_string(),
                at_ms: 1,
            },
        }
    }

    #[test]
    fn canonicalizes_separator_aliases() {
        assert_eq!(canonical_entity_id("solo_relay"), "solo-relay");
        assert_eq!(canonical_entity_id("Solo Relay"), "solo-relay");
        assert_eq!(canonical_entity_id("solo-relay"), "solo-relay");
    }

    #[test]
    fn demotes_paths_and_commits_in_object_position_to_literals() {
        let t = triple(
            "solo-relay",
            "uses_path",
            r"C:\Users\Example\Projects\solo",
            TripleObjectKind::Entity,
            0.9,
        );
        let decision = evaluate_extracted_triple(&t, "cluster", Some(1), "{}".to_string());
        let TripleQualityDecision::Active(active) = decision else {
            panic!("metadata object should remain a literal fact");
        };
        assert_eq!(active.subject_id, "solo-relay");
        assert_eq!(active.object_kind, "literal");

        let t = triple(
            "solo",
            "references_commit",
            "commit_0d85226",
            TripleObjectKind::Entity,
            0.9,
        );
        let decision = evaluate_extracted_triple(&t, "cluster", Some(1), "{}".to_string());
        let TripleQualityDecision::Active(active) = decision else {
            panic!("commit-like object should remain a literal fact");
        };
        assert_eq!(active.object_kind, "literal");
    }

    #[test]
    fn quarantines_assistant_chatter_and_long_blobs() {
        let t = triple(
            "assistant",
            "said",
            "The assistant was unable to provide assistance",
            TripleObjectKind::Literal,
            0.9,
        );
        let decision = evaluate_extracted_triple(&t, "cluster", None, "{}".to_string());
        assert!(matches!(
            decision,
            TripleQualityDecision::Review(TripleReviewCandidate {
                reason_code: "assistant_or_tool_chatter",
                ..
            })
        ));

        let t = triple(
            "solo",
            "has_value",
            "this_is_a_really_long_machine_generated_identifier_with_many_many_underscores_and_no_human_meaning",
            TripleObjectKind::Literal,
            0.9,
        );
        let decision = evaluate_extracted_triple(&t, "cluster", None, "{}".to_string());
        assert!(matches!(
            decision,
            TripleQualityDecision::Review(TripleReviewCandidate {
                reason_code: "long_machine_blob",
                ..
            })
        ));
    }

    #[test]
    fn bad_memory_corpus_routes_transcript_and_tool_noise_to_quarantine() {
        let cases = [
            (
                "assistant",
                "can_assist",
                "false",
                TripleObjectKind::Literal,
                0.4,
                "assistant capability noise should not become a low-confidence review",
            ),
            (
                "tool_output",
                "returned",
                "{\"ok\":true,\"content\":[]}",
                TripleObjectKind::Literal,
                0.95,
                "tool output transcript rows are not durable facts",
            ),
            (
                "solo",
                "notes",
                "I cannot access external websites from this environment",
                TripleObjectKind::Literal,
                0.95,
                "assistant refusal text should stay out of active memory",
            ),
            (
                "system",
                "prompt",
                "You are an assistant accessed via an API",
                TripleObjectKind::Literal,
                0.95,
                "system prompt snippets are not user memory",
            ),
        ];

        for (subject, predicate, object, kind, confidence, label) in cases {
            let t = triple(subject, predicate, object, kind, confidence);
            let decision = evaluate_extracted_triple(&t, "cluster", None, "{}".to_string());
            let TripleQualityDecision::Review(candidate) = decision else {
                panic!("{label}");
            };
            assert_eq!(
                candidate.reason_code, "assistant_or_tool_chatter",
                "{label}"
            );
            assert_eq!(
                claim_status_for_review_reason(candidate.reason_code),
                "quarantined",
                "{label}"
            );
            assert!(
                review_claim_quality_score(candidate.confidence, candidate.reason_code) <= 0.1,
                "{label}: quality score should make obvious noise sink"
            );
        }
    }

    #[test]
    fn active_review_reason_prioritizes_noise_over_low_confidence() {
        let reason =
            active_triple_review_reason("assistant", "can_assist", "false", "literal", 0.1, false)
                .expect("assistant capability noise should need review");
        assert_eq!(reason.reason_code, "assistant_or_tool_chatter");
        assert_eq!(
            claim_status_for_review_reason(reason.reason_code),
            "quarantined"
        );
    }

    #[test]
    fn review_claim_status_and_score_separate_noise_from_human_review() {
        assert_eq!(
            claim_status_for_review_reason("assistant_or_tool_chatter"),
            "quarantined"
        );
        assert_eq!(
            claim_status_for_review_reason("metadata_subject"),
            "rejected"
        );
        assert_eq!(
            claim_status_for_review_reason("weak_literal"),
            "needs_review"
        );

        assert_eq!(
            review_claim_quality_score(0.95, "metadata_subject"),
            0.0,
            "rejected metadata subjects should have no claim-quality score"
        );
        assert!(
            review_claim_quality_score(0.95, "assistant_or_tool_chatter")
                < review_claim_quality_score(0.55, "low_confidence"),
            "obvious transcript noise should rank below ordinary weak confidence"
        );
        assert!(
            review_claim_quality_score(0.95, "literal_only_machine_entity")
                < review_claim_quality_score(0.95, "weak_literal"),
            "machine-shaped literal review claims should score lower than ordinary weak literals"
        );
    }

    #[test]
    fn quarantines_low_confidence() {
        let t = triple(
            "solo",
            "relates_to",
            "memory",
            TripleObjectKind::Entity,
            0.8,
        );
        let decision = evaluate_extracted_triple(&t, "cluster", None, "{}".to_string());
        assert!(matches!(
            decision,
            TripleQualityDecision::Review(TripleReviewCandidate {
                reason_code: "low_confidence",
                ..
            })
        ));
    }

    #[test]
    fn active_review_reason_flags_machine_literals_but_not_short_project_slugs() {
        let reason = active_triple_review_reason(
            "machine-generated-subject-123456",
            "has",
            "this_is_machine_generated_literal_value_with_enough_parts_1234567890",
            "literal",
            0.95,
            true,
        )
        .expect("machine literal should need review");
        assert_eq!(reason.reason_code, "literal_only_machine_entity");

        assert!(
            active_triple_review_reason(
                "solo-relay",
                "uses",
                "relay_control_plane",
                "literal",
                0.95,
                true,
            )
            .is_none(),
            "short durable project slugs should not be treated as broken machine ids"
        );
        assert!(!looks_like_machine_identifier("solo-relay"));
        assert!(looks_like_machine_identifier(
            "machine-generated-subject-123456"
        ));
    }

    #[test]
    fn active_review_reason_flags_empty_canonical_subjects() {
        let reason = active_triple_review_reason("!!!", "uses", "solo", "entity", 0.95, false)
            .expect("punctuation-only subject should need review");
        assert_eq!(reason.reason_code, "weak_subject");
    }

    #[test]
    fn quarantines_commit_like_subjects() {
        let t = triple(
            "commit-0d85226",
            "changed",
            "solo",
            TripleObjectKind::Entity,
            0.9,
        );
        let decision = evaluate_extracted_triple(&t, "cluster", None, "{}".to_string());
        assert!(matches!(
            decision,
            TripleQualityDecision::Review(TripleReviewCandidate {
                reason_code: "metadata_subject",
                ..
            })
        ));
    }
}
