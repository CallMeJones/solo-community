// SPDX-License-Identifier: Apache-2.0

//! Reproducible quality and latency benchmarks for Solo.
//!
//! The first production harness targets LongMemEval-S retrieval. Each
//! benchmark question gets a fresh store, one episode per history session,
//! and exact session-id scoring. This mirrors the public MemPalace raw
//! baseline while exercising Solo's production HNSW + FTS/RRF recall path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use solo_core::{Confidence, Embedder, EncodingContext, Episode, MemoryId, Tier, VectorIndex};
use solo_query::recall::run_recall_inner;
use solo_storage::test_support::open_test_db_at;
use solo_storage::{HnswIndex, HnswParams, ReaderPool, WriteHandle, WriterActor, WriterSpawn};

pub const DEFAULT_KS: &[usize] = &[1, 3, 5, 10, 30, 50];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentMode {
    /// MemPalace raw-parity mode: concatenate only user turns.
    UserTurns,
    /// Solo-native diagnostic: concatenate user and assistant turns.
    AllTurns,
}

impl ContentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserTurns => "user_turns",
            Self::AllTurns => "all_turns",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LongMemEvalTurn {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub has_answer: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LongMemEvalEntry {
    pub question_id: String,
    pub question_type: String,
    pub question: String,
    pub answer: serde_json::Value,
    #[serde(default)]
    pub question_date: String,
    pub haystack_session_ids: Vec<String>,
    pub haystack_dates: Vec<String>,
    pub haystack_sessions: Vec<Vec<LongMemEvalTurn>>,
    pub answer_session_ids: Vec<String>,
}

impl LongMemEvalEntry {
    pub fn validate(&self) -> Result<()> {
        let sessions = self.haystack_sessions.len();
        if sessions != self.haystack_session_ids.len() || sessions != self.haystack_dates.len() {
            bail!(
                "question {} has mismatched haystack lengths: sessions={}, ids={}, dates={}",
                self.question_id,
                sessions,
                self.haystack_session_ids.len(),
                self.haystack_dates.len()
            );
        }
        if self.question.trim().is_empty() {
            bail!("question {} has empty question text", self.question_id);
        }
        let corpus_ids: HashSet<&str> = self
            .haystack_session_ids
            .iter()
            .map(String::as_str)
            .collect();
        // The official cleaned corpus contains repeated session IDs, sometimes
        // with different dates. Preserve every occurrence and its raw rank;
        // scoring below credits each relevant ID only once.
        for answer_id in &self.answer_session_ids {
            if !corpus_ids.contains(answer_id.as_str()) {
                bail!(
                    "question {} answer session {} is absent from the haystack",
                    self.question_id,
                    answer_id
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RankedSession {
    pub rank: usize,
    pub session_id: String,
    pub memory_id: String,
    pub cos_distance: f32,
    pub bm25_score: Option<f32>,
    pub fused_score: f32,
    pub text_preview: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsAtK {
    pub k: usize,
    pub recall_any: f64,
    pub recall_all: f64,
    pub ndcg: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuestionResult {
    pub question_id: String,
    pub question_type: String,
    pub question: String,
    pub answer: serde_json::Value,
    pub question_date: String,
    pub answer_session_ids: Vec<String>,
    pub corpus_sessions: usize,
    pub duplicate_session_occurrences: usize,
    pub scored: bool,
    pub first_relevant_rank: Option<usize>,
    pub reciprocal_rank: f64,
    pub metrics: Vec<MetricsAtK>,
    pub ingest_ms: f64,
    pub query_ms: f64,
    pub ranked_sessions: Vec<RankedSession>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LatencySummary {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub mean_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AggregateMetrics {
    pub questions: usize,
    pub scored_questions: usize,
    pub mean_reciprocal_rank: f64,
    pub by_k: Vec<MetricsAtK>,
}

pub fn format_session(session: &[LongMemEvalTurn], mode: ContentMode) -> String {
    session
        .iter()
        .filter(|turn| mode == ContentMode::AllTurns || turn.role == "user")
        .map(|turn| turn.content.as_str())
        .filter(|content| !content.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn score_ranked_sessions(
    ranked_session_ids: &[String],
    answer_session_ids: &[String],
    ks: &[usize],
) -> (bool, Option<usize>, f64, Vec<MetricsAtK>) {
    let relevant: HashSet<&str> = answer_session_ids.iter().map(String::as_str).collect();
    if relevant.is_empty() {
        return (
            false,
            None,
            0.0,
            ks.iter()
                .copied()
                .map(|k| MetricsAtK {
                    k,
                    recall_any: 0.0,
                    recall_all: 0.0,
                    ndcg: 0.0,
                })
                .collect(),
        );
    }

    let first_relevant_rank = ranked_session_ids
        .iter()
        .position(|id| relevant.contains(id.as_str()))
        .map(|idx| idx + 1);
    let reciprocal_rank = first_relevant_rank
        .map(|rank| 1.0 / rank as f64)
        .unwrap_or(0.0);
    let by_k = ks
        .iter()
        .copied()
        .map(|k| {
            let top = &ranked_session_ids[..ranked_session_ids.len().min(k)];
            let found: HashSet<&str> = top
                .iter()
                .map(String::as_str)
                .filter(|id| relevant.contains(id))
                .collect();
            let mut credited = HashSet::new();
            let dcg = top
                .iter()
                .enumerate()
                .filter(|(_, id)| relevant.contains(id.as_str()) && credited.insert(id.as_str()))
                .map(|(idx, _)| 1.0 / ((idx + 2) as f64).log2())
                .sum::<f64>();
            let ideal_relevant = relevant.len().min(k);
            let ideal_dcg = (0..ideal_relevant)
                .map(|idx| 1.0 / ((idx + 2) as f64).log2())
                .sum::<f64>();
            MetricsAtK {
                k,
                recall_any: f64::from(!found.is_empty()),
                recall_all: f64::from(found.len() == relevant.len()),
                ndcg: if ideal_dcg > 0.0 && dcg > 0.0 {
                    dcg / ideal_dcg
                } else {
                    0.0
                },
            }
        })
        .collect();
    (true, first_relevant_rank, reciprocal_rank, by_k)
}

pub fn aggregate_results(results: &[QuestionResult], ks: &[usize]) -> AggregateMetrics {
    let scored: Vec<&QuestionResult> = results.iter().filter(|result| result.scored).collect();
    let denominator = scored.len().max(1) as f64;
    let by_k = ks
        .iter()
        .copied()
        .map(|k| {
            let matching: Vec<&MetricsAtK> = scored
                .iter()
                .filter_map(|result| result.metrics.iter().find(|metric| metric.k == k))
                .collect();
            MetricsAtK {
                k,
                recall_any: matching.iter().map(|metric| metric.recall_any).sum::<f64>()
                    / denominator,
                recall_all: matching.iter().map(|metric| metric.recall_all).sum::<f64>()
                    / denominator,
                ndcg: matching.iter().map(|metric| metric.ndcg).sum::<f64>() / denominator,
            }
        })
        .collect();
    AggregateMetrics {
        questions: results.len(),
        scored_questions: scored.len(),
        mean_reciprocal_rank: scored
            .iter()
            .map(|result| result.reciprocal_rank)
            .sum::<f64>()
            / denominator,
        by_k,
    }
}

pub fn aggregate_by_type(
    results: &[QuestionResult],
    ks: &[usize],
) -> BTreeMap<String, AggregateMetrics> {
    let mut grouped: BTreeMap<String, Vec<QuestionResult>> = BTreeMap::new();
    for result in results {
        grouped
            .entry(result.question_type.clone())
            .or_default()
            .push(result.clone());
    }
    grouped
        .into_iter()
        .map(|(question_type, entries)| (question_type, aggregate_results(&entries, ks)))
        .collect()
}

pub fn latency_summary(values_ms: &[f64]) -> LatencySummary {
    if values_ms.is_empty() {
        return LatencySummary::default();
    }
    let mut values = values_ms.to_vec();
    values.sort_by(f64::total_cmp);
    let percentile = |p: f64| {
        let index = ((values.len() - 1) as f64 * p).ceil() as usize;
        values[index.min(values.len() - 1)]
    };
    LatencySummary {
        p50_ms: percentile(0.50),
        p95_ms: percentile(0.95),
        p99_ms: percentile(0.99),
        mean_ms: values.iter().sum::<f64>() / values.len() as f64,
    }
}

pub struct LongMemEvalRunner {
    embedder: Arc<dyn Embedder>,
    content_mode: ContentMode,
    retrieval_limit: usize,
    embed_batch_size: usize,
}

impl LongMemEvalRunner {
    pub fn new(
        embedder: Arc<dyn Embedder>,
        content_mode: ContentMode,
        retrieval_limit: usize,
        embed_batch_size: usize,
    ) -> Result<Self> {
        if !(1..=100).contains(&retrieval_limit) {
            bail!("retrieval limit must be between 1 and 100");
        }
        if embed_batch_size == 0 {
            bail!("embed batch size must be greater than zero");
        }
        Ok(Self {
            embedder,
            content_mode,
            retrieval_limit,
            embed_batch_size,
        })
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    pub async fn run_entry(&self, entry: LongMemEvalEntry) -> Result<QuestionResult> {
        entry.validate()?;
        let duplicate_session_occurrences = entry.haystack_session_ids.len()
            - entry
                .haystack_session_ids
                .iter()
                .collect::<HashSet<_>>()
                .len();
        let mut sessions = Vec::new();
        for ((session, session_id), date) in entry
            .haystack_sessions
            .iter()
            .zip(&entry.haystack_session_ids)
            .zip(&entry.haystack_dates)
        {
            let content = format_session(session, self.content_mode);
            if !content.is_empty() {
                sessions.push((session_id.clone(), date.clone(), content));
            }
        }
        if sessions.is_empty() {
            bail!("question {} produced an empty corpus", entry.question_id);
        }
        let retained_ids: HashSet<&str> = sessions.iter().map(|item| item.0.as_str()).collect();
        for answer_id in &entry.answer_session_ids {
            if !retained_ids.contains(answer_id.as_str()) {
                bail!(
                    "question {} answer session {} was removed by content mode {}",
                    entry.question_id,
                    answer_id,
                    self.content_mode.as_str()
                );
            }
        }

        let store = QuestionStore::open(self.embedder.dim(), sessions.len()).await?;
        let result = async {
            let ingest_started = Instant::now();
            let memory_to_session = store
                .ingest(&self.embedder, &sessions, self.embed_batch_size)
                .await
                .with_context(|| format!("ingest question {}", entry.question_id))?;
            let ingest_ms = ingest_started.elapsed().as_secs_f64() * 1000.0;

            let query_started = Instant::now();
            let recall = run_recall_inner(
                &self.embedder,
                &store.hnsw,
                &store.pool,
                &entry.question,
                self.retrieval_limit,
            )
            .await
            .with_context(|| format!("recall question {}", entry.question_id))?;
            let query_ms = query_started.elapsed().as_secs_f64() * 1000.0;
            if recall.retrieval_mode != solo_query::RetrievalMode::Hybrid {
                bail!("question {} degraded to lexical-only retrieval; refusing a mixed-mode benchmark", entry.question_id);
            }

            let mut ranked_sessions = Vec::with_capacity(recall.hits.len());
            for (index, hit) in recall.hits.into_iter().enumerate() {
                let session_id = memory_to_session
                    .get(&hit.memory_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("recall returned unknown memory id {}", hit.memory_id))?;
                ranked_sessions.push(RankedSession {
                    rank: index + 1,
                    session_id,
                    memory_id: hit.memory_id,
                    cos_distance: hit.cos_distance,
                    bm25_score: hit.bm25_score,
                    fused_score: hit.fused_score,
                    text_preview: hit.content.chars().take(500).collect(),
                });
            }

            let ranked_ids: Vec<String> = ranked_sessions
                .iter()
                .map(|item| item.session_id.clone())
                .collect();
            let (scored, first_relevant_rank, reciprocal_rank, metrics) =
                score_ranked_sessions(&ranked_ids, &entry.answer_session_ids, DEFAULT_KS);

            Ok(QuestionResult {
                question_id: entry.question_id,
                question_type: entry.question_type,
                question: entry.question,
                answer: entry.answer,
                question_date: entry.question_date,
                answer_session_ids: entry.answer_session_ids,
                corpus_sessions: sessions.len(),
                duplicate_session_occurrences,
                scored,
                first_relevant_rank,
                reciprocal_rank,
                metrics,
                ingest_ms,
                query_ms,
                ranked_sessions,
            })
        }.await;
        store.shutdown().await?;
        result
    }
}

struct QuestionStore {
    _tempdir: tempfile::TempDir,
    hnsw: Arc<dyn VectorIndex + Send + Sync>,
    pool: ReaderPool,
    writer: Option<WriteHandle>,
    writer_join: Option<std::thread::JoinHandle<()>>,
}

impl QuestionStore {
    async fn open(dim: usize, expected_sessions: usize) -> Result<Self> {
        let tempdir = tempfile::tempdir().context("create benchmark tempdir")?;
        let db_path = tempdir.path().join("benchmark.db");
        let connection = open_test_db_at(&db_path);
        let params = HnswParams {
            max_elements_hint: expected_sessions.max(100),
            ..HnswParams::default()
        };
        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(HnswIndex::new(dim, params));
        let WriterSpawn { handle, join } = WriterActor::spawn(connection, Arc::clone(&hnsw));
        let pool = ReaderPool::new(&db_path, None, Arc::clone(&hnsw))?;
        Ok(Self {
            _tempdir: tempdir,
            hnsw,
            pool,
            writer: Some(handle),
            writer_join: Some(join),
        })
    }

    async fn ingest(
        &self,
        embedder: &Arc<dyn Embedder>,
        sessions: &[(String, String, String)],
        batch_size: usize,
    ) -> Result<HashMap<String, String>> {
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| anyhow!("benchmark writer already shut down"))?;
        let mut memory_to_session = HashMap::with_capacity(sessions.len());
        for chunk in sessions.chunks(batch_size.min(200)) {
            let texts: Vec<&str> = chunk.iter().map(|item| item.2.as_str()).collect();
            let embeddings = embedder.embed_batch(&texts).await?;
            if embeddings.len() != chunk.len() {
                bail!(
                    "embedder returned {} embeddings for {} inputs",
                    embeddings.len(),
                    chunk.len()
                );
            }
            let mut items = Vec::with_capacity(chunk.len());
            let mut memory_ids = Vec::with_capacity(chunk.len());
            for ((session_id, date, content), embedding) in chunk.iter().zip(embeddings) {
                let memory_id = MemoryId::new();
                memory_ids.push((memory_id.to_string(), session_id.clone()));
                let mut extra = serde_json::Map::new();
                extra.insert(
                    "longmemeval_date".into(),
                    serde_json::Value::String(date.clone()),
                );
                items.push((
                    Episode {
                        memory_id,
                        ts_ms: 0,
                        source_type: "benchmark_longmemeval".into(),
                        source_id: Some(session_id.clone()),
                        content: content.clone(),
                        encoding_context: EncodingContext {
                            session_id: Some(session_id.clone()),
                            task: None,
                            recent_summary: None,
                            affect: None,
                            extra,
                        },
                        provenance: None,
                        confidence: Confidence::new(1.0)?,
                        strength: 0.5,
                        salience: 0.5,
                        tier: Tier::Hot,
                    },
                    embedding,
                ));
            }
            writer.remember_batch_as(None, items).await?;
            memory_to_session.extend(memory_ids);
        }
        Ok(memory_to_session)
    }

    async fn shutdown(mut self) -> Result<()> {
        drop(self.writer.take());
        let join = self.writer_join.take();
        drop(self.pool);
        if let Some(join) = join {
            tokio::task::spawn_blocking(move || join.join())
                .await
                .context("join benchmark writer task")?
                .map_err(|_| anyhow!("benchmark writer thread panicked"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_storage::StubEmbedder;

    fn turn(role: &str, content: &str) -> LongMemEvalTurn {
        LongMemEvalTurn {
            role: role.into(),
            content: content.into(),
            has_answer: false,
        }
    }

    #[test]
    fn parity_mode_uses_only_user_turns() {
        let session = vec![
            turn("user", "first user fact"),
            turn("assistant", "assistant expansion"),
            turn("user", "second user fact"),
        ];
        assert_eq!(
            format_session(&session, ContentMode::UserTurns),
            "first user fact\nsecond user fact"
        );
        assert!(format_session(&session, ContentMode::AllTurns).contains("assistant expansion"));
    }

    #[test]
    fn metrics_distinguish_any_all_and_rank() {
        let ranked = vec!["noise".into(), "gold-a".into(), "gold-b".into()];
        let gold = vec!["gold-a".into(), "gold-b".into()];
        let (scored, first, rr, metrics) = score_ranked_sessions(&ranked, &gold, &[1, 2, 3]);
        assert!(scored);
        assert_eq!(first, Some(2));
        assert_eq!(rr, 0.5);
        assert_eq!(metrics[0].recall_any, 0.0);
        assert_eq!(metrics[1].recall_any, 1.0);
        assert_eq!(metrics[1].recall_all, 0.0);
        assert_eq!(metrics[2].recall_all, 1.0);
    }

    #[test]
    fn duplicate_relevant_ids_do_not_inflate_ndcg_or_compress_ranks() {
        let ranked = vec!["gold-a".into(), "gold-a".into(), "gold-b".into()];
        let gold = vec!["gold-a".into(), "gold-b".into()];
        let (_, _, _, metrics) = score_ranked_sessions(&ranked, &gold, &[1, 2, 3]);
        assert_eq!(metrics[0].ndcg, 1.0);
        assert_eq!(metrics[1].recall_all, 0.0);
        assert!(metrics[1].ndcg < 1.0);
        assert_eq!(metrics[2].recall_all, 1.0);
        assert!(metrics[2].ndcg < 1.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runner_scores_exact_session_ids_through_solo_recall() {
        let entry = LongMemEvalEntry {
            question_id: "fixture-1".into(),
            question_type: "single-session-user".into(),
            question: "Where is codeword cobalt?".into(),
            answer: serde_json::json!("In the correct session"),
            question_date: "2026/01/01".into(),
            haystack_session_ids: vec!["noise-session".into(), "answer-session".into()],
            haystack_dates: vec!["2025/01/01".into(), "2025/01/02".into()],
            haystack_sessions: vec![
                vec![turn("user", "ordinary unrelated note")],
                vec![turn("user", "codeword cobalt is stored in this session")],
            ],
            answer_session_ids: vec!["answer-session".into()],
        };
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", 16));
        let runner = LongMemEvalRunner::new(embedder, ContentMode::UserTurns, 2, 2).unwrap();
        let mut repeated = entry.clone();
        repeated.haystack_session_ids.push("answer-session".into());
        repeated.haystack_dates.push("2025/01/03".into());
        repeated
            .haystack_sessions
            .push(vec![turn("user", "another codeword cobalt occurrence")]);
        let repeated_result = runner.run_entry(repeated).await.unwrap();
        assert_eq!(repeated_result.corpus_sessions, 3);
        assert_eq!(repeated_result.duplicate_session_occurrences, 1);
        assert!(repeated_result.metrics.iter().all(|m| m.ndcg <= 1.0));
        let result = runner.run_entry(entry).await.unwrap();
        assert_eq!(result.ranked_sessions[0].session_id, "answer-session");
        assert_eq!(
            result.metrics.iter().find(|m| m.k == 1).unwrap().recall_any,
            1.0
        );
    }
}
