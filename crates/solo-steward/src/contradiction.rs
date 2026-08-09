// SPDX-License-Identifier: Apache-2.0

//! Contradiction detection between two triples — the third stage of
//! consolidation per ADR-0002. Two-stage:
//!
//!   1. **Cheap rule filter** — pure-Rust short-circuits that exclude
//!      pairs that obviously can't contradict. Saves the LLM cost
//!      across the typical "nothing in common" majority.
//!   2. **LLM judge** — for the small subset that survives the rule
//!      filter, ask the model whether the surviving pair actually
//!      contradicts and (if so) how.
//!
//! ## Rule-filter short-circuits
//!
//! These return `Ok(None)` without any LLM call:
//!
//!   - `a.triple_id == b.triple_id` — same triple.
//!   - `a.subject_id != b.subject_id` — different things; not a
//!     direct contradiction. Cross-subject inconsistencies (e.g.,
//!     "Sam is in Paris" vs "Paris is empty of Sams") are out of
//!     scope for v0.2.0.
//!   - `a.predicate != b.predicate` — different relations. Same
//!     reasoning.
//!   - `a.object_id == b.object_id` — identical claim. Trivially
//!     consistent.
//!   - **Validity windows don't overlap** — temporal succession is
//!     not contradiction. ("Sam lived in Paris in 2020"; "Sam lived
//!     in Berlin in 2024" → both true at different times.)
//!
//! Anything that survives all five short-circuits is a contradiction
//! candidate; the LLM gets the final word.
//!
//! ## LLM judge contract
//!
//! Strict-JSON output:
//!
//! ```json
//! {
//!   "is_contradiction": <bool>,
//!   "kind": "overlapping_single_valued_predicate"
//!         | "direct_negation"
//!         | "numeric_inconsistency"
//!         | "other",
//!   "explanation": "<one-paragraph reason>"
//! }
//! ```
//!
//! Permissive parsing — same approach as `abstraction.rs`: try direct
//! JSON, fall back to fenced ```json``` block, on total failure treat
//! as `is_contradiction: false` (errors of judgment shouldn't poison
//! consolidation).
//!
//! ## What's not in v0.2.0
//!
//! - **Cross-subject reasoning** (e.g., uniqueness violations like
//!   "Paris is the capital of France" + "Berlin is the capital of
//!   France"). v0.2.0's rule filter requires identical subject_id,
//!   so these slip through.
//! - **Aggregate / derived contradictions** (numeric: "Sam owns 3
//!   cats" + "Sam owns 5 cats" — only caught if subject+predicate
//!   match exactly).
//! - **Probabilistic confidence weighting** — Y.5+ could rank
//!   contradictions by aggregate confidence + recency.

use std::sync::OnceLock;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use solo_core::{Contradiction, ContradictionKind, LlmClient, Message, Result, Triple};

/// Detect whether `a` and `b` contradict. `Ok(None)` means "not a
/// contradiction"; `Ok(Some(c))` carries the pairing + kind +
/// human-readable explanation.
///
/// `client` is consulted only after the rule filter narrows the
/// candidate set; the typical pair short-circuits without an LLM
/// call.
pub async fn detect_contradiction(
    a: &Triple,
    b: &Triple,
    client: &dyn LlmClient,
) -> Result<Option<Contradiction>> {
    // ----- Stage 1: rule filter -----
    if !is_contradiction_candidate(a, b) {
        return Ok(None);
    }

    // ----- Stage 2: LLM judge -----
    let messages = build_prompt(a, b);
    let response = client.complete(&messages).await?;
    Ok(parse_judge_response(a, b, &response.content))
}

/// Cheap pure-Rust short-circuits. Public so callers (e.g.,
/// `handle_consolidate`'s contradiction sweep) can pre-filter pairs
/// with a SQL JOIN + this function before paying the LLM cost.
pub fn is_contradiction_candidate(a: &Triple, b: &Triple) -> bool {
    if a.triple_id == b.triple_id {
        return false;
    }
    if a.subject_id != b.subject_id {
        return false;
    }
    if a.predicate != b.predicate {
        return false;
    }
    if a.object_id == b.object_id {
        return false;
    }
    if !temporal_overlap(a, b) {
        return false;
    }
    true
}

/// True if `a` and `b`'s validity windows overlap (closed interval).
/// `valid_to_ms = None` means "still valid" → treated as `i64::MAX`.
fn temporal_overlap(a: &Triple, b: &Triple) -> bool {
    let a_end = a.valid_to_ms.unwrap_or(i64::MAX);
    let b_end = b.valid_to_ms.unwrap_or(i64::MAX);
    a.valid_from_ms <= b_end && b.valid_from_ms <= a_end
}

const SYSTEM_PROMPT: &str = r#"You are Solo's contradiction-detection steward.

You are given two semantic triples (subject, predicate, object) about the same subject + predicate that have overlapping validity windows but different objects. Decide whether they actually contradict each other.

Output STRICT JSON. Do NOT include explanations outside the JSON, no prose, no markdown fences:

{
  "is_contradiction": <bool>,
  "kind": "overlapping_single_valued_predicate" | "direct_negation" | "numeric_inconsistency" | "other",
  "explanation": "<one-paragraph reason>"
}

Rules:
- "is_contradiction" is false when the two triples can both be simultaneously true (e.g., the predicate is a multi-valued relation — a thing can simultaneously hold many such values).
- MULTI-VALUE PREDICATES: the following predicates are multi-valued by nature; multiple distinct objects under the same (subject, predicate) are NOT contradictions, even with fully overlapping validity windows. Treat any pair where the predicate appears in this list as "is_contradiction": false unless the two objects directly negate each other (e.g., "supports X" vs "does not support X"):
   - "tagged_with" (a person/thing can carry many tags)
   - "uses" (a project can use many languages, frameworks, libraries)
   - "supports" (a product can support many platforms, formats, protocols)
   - "has" (an entity can have many attributes, components, features)
   - "depends_on" (a system can depend on many other systems)
- "is_contradiction" is true when both can NOT be simultaneously true (e.g., "lives_in" with two different cities at the same time, "is" with two different scalar values).
- For "kind":
   - "overlapping_single_valued_predicate" — the predicate fundamentally takes one value at a time (location, age, marital status, etc.).
   - "direct_negation" — one triple negates the other ("X is alive" vs "X is dead").
   - "numeric_inconsistency" — the predicate is numeric and the values differ ("X has age 5" vs "X has age 7").
   - "other" — anything else.
- "explanation" should reference both triples' object values + the time window in 1-2 sentences.
"#;

/// A short, stable identifier for the current `SYSTEM_PROMPT` body —
/// the first 8 hex chars of `Sha256(SYSTEM_PROMPT)`. Computed once on
/// first access (cached in a `OnceLock`) and used by the writer's
/// contradiction-sweep tracing so logged sweeps can be correlated to
/// the exact prompt text that drove them.
///
/// Why this matters: when a future prompt edit changes detector
/// behaviour, the hash changes. Operators reading logs / replaying old
/// runs can tell at a glance whether a flagged contradiction was
/// produced by the prompt they think it was — without diffing the
/// prompt body across releases.
pub fn prompt_version_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let digest = Sha256::digest(SYSTEM_PROMPT.as_bytes());
        // First 8 hex chars (4 bytes / 32 bits) — enough entropy to
        // distinguish hand-edited prompt revisions without bloating
        // log lines. Collisions are not a security concern: this is
        // an observability aid, not an integrity check.
        let mut out = String::with_capacity(8);
        for byte in digest.iter().take(4) {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
        }
        out
    })
}

fn build_prompt(a: &Triple, b: &Triple) -> Vec<Message> {
    let user = format!(
        "Triple A:\n  subject:   {sub}\n  predicate: {pred}\n  object:    {oa} ({kinda:?})\n  valid: [{a_from}, {a_to}]\n\n\
         Triple B:\n  subject:   {sub}\n  predicate: {pred}\n  object:    {ob} ({kindb:?})\n  valid: [{b_from}, {b_to}]\n",
        sub = a.subject_id,
        pred = a.predicate,
        oa = a.object_id,
        kinda = a.object_kind,
        a_from = format_ts(a.valid_from_ms),
        a_to = a
            .valid_to_ms
            .map(format_ts)
            .unwrap_or_else(|| "open".into()),
        ob = b.object_id,
        kindb = b.object_kind,
        b_from = format_ts(b.valid_from_ms),
        b_to = b
            .valid_to_ms
            .map(format_ts)
            .unwrap_or_else(|| "open".into()),
    );
    vec![Message::system(SYSTEM_PROMPT), Message::user(user)]
}

fn format_ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ms.to_string())
}

#[derive(Debug, Default, Deserialize)]
struct JudgePayload {
    #[serde(default)]
    is_contradiction: bool,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    explanation: String,
}

/// Parse the LLM response. Permissive — a malformed response is
/// treated as "no contradiction" (false negative) rather than
/// surfacing an error to the caller. Errors of judgment shouldn't
/// poison the consolidation pipeline.
///
/// Dev-log 0152 (low steward — parse_failed_count): when BOTH the
/// direct JSON parse and the fenced-extraction parse fail, emit a
/// `tracing::warn!` so operators can distinguish "few contradictions
/// in corpus" from "LLM returning garbage". The function still
/// returns `None` (no contradiction recorded) for backward
/// compatibility; the surface that aggregates these counts can sum
/// the warning emissions or pass a counter in a future iteration.
fn parse_judge_response(a: &Triple, b: &Triple, raw: &str) -> Option<Contradiction> {
    let direct = serde_json::from_str::<JudgePayload>(raw).ok();
    let parsed: JudgePayload = direct
        .or_else(|| extract_fenced_json(raw).and_then(|s| serde_json::from_str(&s).ok()))
        .unwrap_or_else(|| {
            tracing::warn!(
                a_triple_id = %a.triple_id,
                b_triple_id = %b.triple_id,
                raw_prefix = %raw.chars().take(120).collect::<String>(),
                "contradiction judge: LLM response failed both JSON \
                 parse paths; treating as no-contradiction. If many of \
                 these appear together, the LLM is likely returning \
                 non-JSON output."
            );
            JudgePayload::default()
        });

    if !parsed.is_contradiction {
        return None;
    }

    let kind = match parsed.kind.as_str() {
        "overlapping_single_valued_predicate" => {
            ContradictionKind::OverlappingSingleValuedPredicate
        }
        "direct_negation" => ContradictionKind::DirectNegation,
        "numeric_inconsistency" => ContradictionKind::NumericInconsistency,
        _ => ContradictionKind::Other,
    };

    let explanation = if parsed.explanation.is_empty() {
        format!(
            "{} {}: '{}' vs '{}' with overlapping validity",
            a.subject_id, a.predicate, a.object_id, b.object_id
        )
    } else {
        parsed.explanation
    };

    Some(Contradiction {
        a: a.triple_id,
        b: b.triple_id,
        kind,
        explanation,
    })
}

/// Extract the contents of the first ```json ... ``` (or generic
/// ``` ... ```) fenced code block. Mirrors `abstraction.rs`'s
/// helper — could be hoisted into a shared `prompt_io` module if a
/// third caller appears.
fn extract_fenced_json(raw: &str) -> Option<String> {
    let after_open = raw
        .find("```json")
        .map(|i| i + "```json".len())
        .or_else(|| raw.find("```").map(|i| i + 3))?;
    let rest = &raw[after_open..];
    let body_start = rest.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &rest[body_start..];
    let close = body.find("```")?;
    Some(body[..close].trim().to_string())
}

/// Convenience: when caller just wants to filter via the rule + LLM
/// across an iterator of pairs without writing the loop. Returns
/// only the contradictions, dropping `Ok(None)` results.
pub async fn detect_all<'a, I>(pairs: I, client: &dyn LlmClient) -> Result<Vec<Contradiction>>
where
    I: IntoIterator<Item = (&'a Triple, &'a Triple)>,
{
    let mut out = Vec::new();
    for (a, b) in pairs {
        if let Some(c) = detect_contradiction(a, b, client).await? {
            out.push(c);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::StubLlmClient;
    use solo_core::{Confidence, MemoryId, Provenance, Triple, TripleObjectKind};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn triple(
        subject: &str,
        predicate: &str,
        object: &str,
        kind: TripleObjectKind,
        valid_from_ms: i64,
        valid_to_ms: Option<i64>,
    ) -> Triple {
        Triple {
            triple_id: MemoryId::new(),
            subject_id: subject.into(),
            predicate: predicate.into(),
            object_id: object.into(),
            object_kind: kind,
            valid_from_ms,
            valid_to_ms,
            confidence: Confidence::new(0.9).unwrap(),
            provenance: Provenance {
                derived_from: vec![],
                derivation: "test".into(),
                by: "test".into(),
                at_ms: 0,
            },
        }
    }

    // ---------- rule filter ----------

    #[test]
    fn rule_filter_rejects_different_subjects() {
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple(
            "Bob",
            "lives_in",
            "Berlin",
            TripleObjectKind::Entity,
            0,
            None,
        );
        assert!(!is_contradiction_candidate(&a, &b));
    }

    #[test]
    fn rule_filter_rejects_different_predicates() {
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple("Sam", "born_in", "Paris", TripleObjectKind::Entity, 0, None);
        assert!(!is_contradiction_candidate(&a, &b));
    }

    #[test]
    fn rule_filter_rejects_identical_objects() {
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            100,
            None,
        );
        assert!(!is_contradiction_candidate(&a, &b));
    }

    #[test]
    fn rule_filter_rejects_non_overlapping_windows() {
        // Sam lived in Paris [0, 100]; Sam lived in Berlin [200, 300].
        // No overlap → temporal succession, not contradiction.
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            Some(100),
        );
        let b = triple(
            "Sam",
            "lives_in",
            "Berlin",
            TripleObjectKind::Entity,
            200,
            Some(300),
        );
        assert!(!is_contradiction_candidate(&a, &b));
    }

    #[test]
    fn rule_filter_admits_overlapping_same_subject_predicate() {
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple(
            "Sam",
            "lives_in",
            "Berlin",
            TripleObjectKind::Entity,
            50,
            None,
        );
        assert!(is_contradiction_candidate(&a, &b));
    }

    #[test]
    fn rule_filter_treats_open_window_as_max() {
        // a: [0, open]; b: [10, 20]. Overlap.
        let a = triple("Sam", "is", "alive", TripleObjectKind::Literal, 0, None);
        let b = triple("Sam", "is", "dead", TripleObjectKind::Literal, 10, Some(20));
        assert!(is_contradiction_candidate(&a, &b));
    }

    #[test]
    fn rule_filter_rejects_same_triple_id() {
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let mut b = a.clone();
        // Same triple_id (rare in practice but defensive).
        b.triple_id = a.triple_id;
        assert!(!is_contradiction_candidate(&a, &b));
    }

    // ---------- LLM-judge integration ----------

    #[test]
    fn judge_returns_contradiction_when_llm_says_yes() {
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple(
            "Sam",
            "lives_in",
            "Berlin",
            TripleObjectKind::Entity,
            50,
            None,
        );

        let canned = r#"{
            "is_contradiction": true,
            "kind": "overlapping_single_valued_predicate",
            "explanation": "Sam can't simultaneously live in two cities."
        }"#;
        let stub = StubLlmClient::with_canned("judge-yes", canned);

        let c = rt()
            .block_on(detect_contradiction(&a, &b, &stub))
            .unwrap()
            .expect("expected a contradiction");
        assert_eq!(c.a, a.triple_id);
        assert_eq!(c.b, b.triple_id);
        assert!(matches!(
            c.kind,
            ContradictionKind::OverlappingSingleValuedPredicate
        ));
        assert!(c.explanation.contains("two cities"));
    }

    #[test]
    fn judge_returns_none_when_llm_says_no() {
        let a = triple(
            "Sam",
            "tagged_with",
            "blue",
            TripleObjectKind::Literal,
            0,
            None,
        );
        let b = triple(
            "Sam",
            "tagged_with",
            "tall",
            TripleObjectKind::Literal,
            50,
            None,
        );

        let canned =
            r#"{ "is_contradiction": false, "kind": "other", "explanation": "tags compose" }"#;
        let stub = StubLlmClient::with_canned("judge-no", canned);

        let r = rt().block_on(detect_contradiction(&a, &b, &stub)).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn judge_handles_fenced_json_response() {
        let a = triple("Sam", "is", "alive", TripleObjectKind::Literal, 0, None);
        let b = triple("Sam", "is", "dead", TripleObjectKind::Literal, 10, None);

        let canned = "```json\n{\"is_contradiction\":true,\"kind\":\"direct_negation\",\"explanation\":\"obvious\"}\n```";
        let stub = StubLlmClient::with_canned("fenced", canned);

        let c = rt()
            .block_on(detect_contradiction(&a, &b, &stub))
            .unwrap()
            .expect("contradiction");
        assert!(matches!(c.kind, ContradictionKind::DirectNegation));
    }

    #[test]
    fn malformed_response_is_treated_as_no_contradiction() {
        // LLM refusal / non-JSON. Don't poison the pipeline.
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple(
            "Sam",
            "lives_in",
            "Berlin",
            TripleObjectKind::Entity,
            50,
            None,
        );

        let stub = StubLlmClient::with_canned("refusal", "I cannot help with that.");
        let r = rt().block_on(detect_contradiction(&a, &b, &stub)).unwrap();
        assert!(r.is_none(), "malformed → permissive None: got {r:?}");
    }

    #[test]
    fn unknown_kind_string_falls_back_to_other() {
        let a = triple("Sam", "is", "x", TripleObjectKind::Literal, 0, None);
        let b = triple("Sam", "is", "y", TripleObjectKind::Literal, 0, None);

        let canned = r#"{
            "is_contradiction": true,
            "kind": "made_up_kind",
            "explanation": "unknown vocabulary"
        }"#;
        let stub = StubLlmClient::with_canned("unknown-kind", canned);

        let c = rt()
            .block_on(detect_contradiction(&a, &b, &stub))
            .unwrap()
            .expect("contradiction");
        assert!(matches!(c.kind, ContradictionKind::Other));
    }

    #[test]
    fn empty_explanation_falls_back_to_synthetic() {
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple(
            "Sam",
            "lives_in",
            "Berlin",
            TripleObjectKind::Entity,
            50,
            None,
        );

        let canned = r#"{ "is_contradiction": true, "kind": "overlapping_single_valued_predicate", "explanation": "" }"#;
        let stub = StubLlmClient::with_canned("empty-expl", canned);

        let c = rt()
            .block_on(detect_contradiction(&a, &b, &stub))
            .unwrap()
            .expect("contradiction");
        assert!(c.explanation.contains("Paris"));
        assert!(c.explanation.contains("Berlin"));
    }

    #[test]
    fn rule_filter_short_circuits_skip_llm_entirely() {
        // Different subjects → rule filter rejects → LLM never called.
        let a = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple(
            "Bob",
            "lives_in",
            "Berlin",
            TripleObjectKind::Entity,
            0,
            None,
        );

        let stub = StubLlmClient::default_stub();
        let r = rt().block_on(detect_contradiction(&a, &b, &stub)).unwrap();
        assert!(r.is_none());
        assert_eq!(
            stub.call_count(),
            0,
            "LLM must not be called for filtered-out pairs"
        );
    }

    // ---------- SYSTEM_PROMPT regression guards (v0.5.0 sub-step 2A) ----------
    //
    // These tests pin the multi-value predicate list from
    // docs/dev-log/0071-v0.5.x-roadmap.md Priority 2 into the prompt
    // body. They exist to catch accidental removal/weakening of the
    // rule in future prompt edits; they are NOT meant to enforce
    // exact wording forever — if you intentionally restructure the
    // prompt and the same predicate list still survives, update
    // these keyword checks to match the new wording.
    //
    // Integration-style tests that drive a real LLM and assert on
    // judge output (the "Quotient uses Python" + "Quotient uses
    // FastAPI" thesis-test false positive) are infeasible in CI: the
    // StubLlmClient returns canned responses regardless of prompt
    // content. Empirical validation of the rule tightening is the
    // thesis-test corpus, run out-of-band against a real model.

    /// The rule block itself must survive future prompt edits. The
    /// "MULTI-VALUE PREDICATES" heading is the anchor — if it
    /// disappears, the rule has been deleted or restructured in a
    /// way that needs review.
    #[test]
    fn prompt_carries_multi_value_predicates_rule_block() {
        assert!(
            SYSTEM_PROMPT.contains("MULTI-VALUE PREDICATES"),
            "SYSTEM_PROMPT lost the MULTI-VALUE PREDICATES rule \
             header — see docs/dev-log/0071-v0.5.x-roadmap.md \
             Priority 2"
        );
    }

    /// Each of the five multi-value predicate names must appear in
    /// the prompt as a quoted, dedicated list entry. Asserting on a
    /// quoted form (e.g. `"uses"`) instead of the bare keyword
    /// avoids incidental matches against the word "uses" in
    /// surrounding sentences.
    #[test]
    fn prompt_lists_all_five_multi_value_predicates() {
        // tagged_with — original entry, kept to guard against
        // regression when future edits restructure the list.
        assert!(
            SYSTEM_PROMPT.contains(r#""tagged_with""#),
            "SYSTEM_PROMPT lost the `tagged_with` multi-value \
             predicate entry"
        );
        // The four added in 2A:
        assert!(
            SYSTEM_PROMPT.contains(r#""uses""#),
            "SYSTEM_PROMPT lost the `uses` multi-value predicate \
             entry (added in v0.5.0 sub-step 2A)"
        );
        assert!(
            SYSTEM_PROMPT.contains(r#""supports""#),
            "SYSTEM_PROMPT lost the `supports` multi-value predicate \
             entry (added in v0.5.0 sub-step 2A)"
        );
        assert!(
            SYSTEM_PROMPT.contains(r#""has""#),
            "SYSTEM_PROMPT lost the `has` multi-value predicate \
             entry (added in v0.5.0 sub-step 2A)"
        );
        assert!(
            SYSTEM_PROMPT.contains(r#""depends_on""#),
            "SYSTEM_PROMPT lost the `depends_on` multi-value \
             predicate entry (added in v0.5.0 sub-step 2A)"
        );
    }

    /// Sanity check: the SYSTEM_PROMPT actually flows through to the
    /// LLM-visible message bytes that `build_prompt` produces. Catches
    /// a regression where the prompt is rewritten but `build_prompt`
    /// stops including it (e.g., a refactor that forgets the
    /// `Message::system(SYSTEM_PROMPT)` line). Uses the canonical
    /// thesis-test false-positive pair (Quotient -- uses -- python /
    /// Quotient -- uses -- fastapi) as the driving inputs.
    #[test]
    fn multi_value_predicates_rule_reaches_the_llm_via_build_prompt() {
        let a = triple(
            "quotient",
            "uses",
            "python",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b = triple(
            "quotient",
            "uses",
            "fastapi",
            TripleObjectKind::Entity,
            50,
            None,
        );

        let canned = r#"{ "is_contradiction": false, "kind": "other", "explanation": "uses is multi-value" }"#;
        let stub = StubLlmClient::with_canned("uses-multi-value", canned);

        let r = rt().block_on(detect_contradiction(&a, &b, &stub)).unwrap();
        assert!(
            r.is_none(),
            "uses/python vs uses/fastapi must NOT be flagged when \
             the LLM judge applies the multi-value rule"
        );

        let prompts = stub.prompts();
        assert_eq!(
            prompts.len(),
            1,
            "rule filter should admit this pair → LLM judge called once"
        );
        // System message is index 0 (build_prompt puts it first).
        let system_msg = &prompts[0][0].content;
        assert!(
            system_msg.contains("MULTI-VALUE PREDICATES"),
            "system message sent to LLM missing the MULTI-VALUE \
             PREDICATES rule block — the judge won't know `uses` is \
             multi-valued"
        );
        assert!(
            system_msg.contains(r#""uses""#),
            "system message sent to LLM missing `uses` in the \
             multi-value predicate list — judge will see Python vs \
             FastAPI as a contradiction (canonical thesis-test false \
             positive)"
        );
    }

    // ---------- prompt_version_hash (v0.5.0 sub-step 2B) ----------

    /// The hash must be stable across calls — it's a `OnceLock` that
    /// caches the first computation, so repeated reads must return
    /// the same string. The writer relies on this for log correlation.
    #[test]
    fn prompt_version_hash_is_stable_across_calls() {
        let h1 = prompt_version_hash();
        let h2 = prompt_version_hash();
        assert_eq!(h1, h2, "prompt_version_hash must be deterministic");
    }

    /// Shape sanity: 8 hex chars = 32 bits of entropy from the
    /// SHA-256 digest. Enough to distinguish hand-edited prompt
    /// revisions; collisions are not a security concern.
    #[test]
    fn prompt_version_hash_is_eight_hex_chars() {
        let h = prompt_version_hash();
        assert_eq!(h.len(), 8, "expected 8 hex chars, got {h:?}");
        assert!(
            h.chars().all(|c| c.is_ascii_hexdigit()),
            "expected lowercase hex digits, got {h:?}"
        );
    }

    #[test]
    fn detect_all_filters_to_contradictions_only() {
        let a1 = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let a2 = triple(
            "Sam",
            "lives_in",
            "Berlin",
            TripleObjectKind::Entity,
            50,
            None,
        );
        // Pair (a1, a2): rule passes; LLM canned says yes.
        // Pair (b1, b2): rule rejects (different predicates).
        let b1 = triple(
            "Sam",
            "lives_in",
            "Paris",
            TripleObjectKind::Entity,
            0,
            None,
        );
        let b2 = triple("Sam", "born_in", "Paris", TripleObjectKind::Entity, 0, None);

        let canned = r#"{ "is_contradiction": true, "kind": "overlapping_single_valued_predicate", "explanation": "two cities" }"#;
        let stub = StubLlmClient::with_canned("yes", canned);

        let pairs = vec![(&a1, &a2), (&b1, &b2)];
        let cs = rt().block_on(detect_all(pairs, &stub)).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].a, a1.triple_id);
        assert_eq!(cs[0].b, a2.triple_id);
        assert_eq!(stub.call_count(), 1, "second pair filtered out before LLM");
    }
}
