// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use solo_bench::{
    AggregateMetrics, ContentMode, DEFAULT_KS, LatencySummary, LongMemEvalEntry, LongMemEvalRunner,
    aggregate_by_type, aggregate_results, latency_summary,
};
use solo_core::Embedder;
#[cfg(feature = "bundled-embedder")]
use solo_storage::BundledEmbedder;
use solo_storage::StubEmbedder;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EmbedderChoice {
    Bundled,
    Stub,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ContentChoice {
    User,
    All,
}

impl From<ContentChoice> for ContentMode {
    fn from(value: ContentChoice) -> Self {
        match value {
            ContentChoice::User => Self::UserTurns,
            ContentChoice::All => Self::AllTurns,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SplitSubset {
    Dev,
    HeldOut,
}

#[derive(Debug, Parser)]
#[command(
    name = "solo-longmemeval",
    about = "Reproducible LongMemEval-S session retrieval benchmark for Solo"
)]
struct Args {
    /// Path to the official longmemeval_s_cleaned.json dataset.
    #[arg(long)]
    data: PathBuf,

    /// Directory for JSONL evidence and the aggregate summary.
    #[arg(long, default_value = "target/benchmarks/longmemeval")]
    out_dir: PathBuf,

    /// Bundled is publishable; stub only validates harness plumbing.
    #[arg(long, value_enum, default_value_t = EmbedderChoice::Bundled)]
    embedder: EmbedderChoice,

    /// User matches MemPalace raw mode; all is a Solo-native diagnostic.
    #[arg(long, value_enum, default_value_t = ContentChoice::User)]
    content: ContentChoice,

    /// Maximum questions after split/skip filtering; 0 means all.
    #[arg(long, default_value_t = 0)]
    limit: usize,

    /// Skip questions after split filtering, for deterministic slices.
    #[arg(long, default_value_t = 0)]
    skip: usize,

    /// Maximum ranked sessions retained and scored (1-100).
    #[arg(long, default_value_t = 50)]
    top_k: usize,

    /// Number of sessions embedded per model call (writer batches cap at 200).
    #[arg(long, default_value_t = 64)]
    embed_batch_size: usize,

    /// Optional MemPalace-compatible JSON split file with dev/held_out arrays.
    #[arg(long, requires = "subset")]
    split_file: Option<PathBuf>,

    /// Select the dev or held-out IDs from --split-file.
    #[arg(long, value_enum, requires = "split_file")]
    subset: Option<SplitSubset>,

    /// Label included in filenames and summary provenance.
    #[arg(long, default_value = "solo")]
    run_label: String,
}

#[derive(Debug, Deserialize)]
struct SplitFile {
    dev: Vec<String>,
    held_out: Vec<String>,
    #[serde(default)]
    seed: Option<u64>,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    schema_version: u32,
    benchmark: &'static str,
    metric_scope: &'static str,
    retrieval_pipeline: &'static str,
    storage_mode: &'static str,
    build_profile: &'static str,
    target_os: &'static str,
    target_arch: &'static str,
    solo_version: &'static str,
    run_label: String,
    created_at_unix_ms: u128,
    dataset_path: String,
    dataset_blake3: String,
    split_path: Option<String>,
    split_subset: Option<String>,
    split_seed: Option<u64>,
    embedder_name: String,
    embedder_version: String,
    embedder_dim: usize,
    publishable: bool,
    content_mode: String,
    retrieval_limit: usize,
    embed_batch_size: usize,
    skipped_questions: usize,
    overall: AggregateMetrics,
    by_question_type: std::collections::BTreeMap<String, AggregateMetrics>,
    ingest_latency: LatencySummary,
    query_latency: LatencySummary,
    elapsed_seconds: f64,
    evidence_jsonl: String,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !(1..=100).contains(&args.top_k) {
        bail!("--top-k must be between 1 and 100");
    }
    if !(1..=200).contains(&args.embed_batch_size) {
        bail!("--embed-batch-size must be between 1 and 200");
    }
    let started = Instant::now();
    let dataset_hash = hash_file(&args.data)?;
    let file = File::open(&args.data)
        .with_context(|| format!("open LongMemEval dataset {}", args.data.display()))?;
    let mut entries: Vec<LongMemEvalEntry> = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parse LongMemEval dataset {}", args.data.display()))?;

    let (split_ids, split_seed) = load_split(&args)?;
    if let Some(ids) = &split_ids {
        entries.retain(|entry| ids.contains(entry.question_id.as_str()));
        if entries.len() != ids.len() {
            bail!(
                "split selected {} ids but dataset contained {} of them",
                ids.len(),
                entries.len()
            );
        }
    }
    let selected: Vec<LongMemEvalEntry> = entries
        .into_iter()
        .skip(args.skip)
        .take(if args.limit == 0 {
            usize::MAX
        } else {
            args.limit
        })
        .collect();
    if selected.is_empty() {
        bail!("no benchmark questions remain after filtering");
    }
    let question_count = selected.len();

    let embedder: Arc<dyn Embedder> = match args.embedder {
        EmbedderChoice::Bundled => {
            #[cfg(feature = "bundled-embedder")]
            {
                Arc::new(BundledEmbedder::new())
            }
            #[cfg(not(feature = "bundled-embedder"))]
            {
                bail!(
                    "bundled embedding requires --features bundled-embedder; use --embedder stub only for harness checks"
                )
            }
        }
        EmbedderChoice::Stub => Arc::new(StubEmbedder::new("benchmark-stub", "v1", 384)),
    };
    if matches!(args.embedder, EmbedderChoice::Bundled) {
        embedder
            .embed("Solo LongMemEval warmup")
            .await
            .context("load bundled embedding model")?;
    }
    let runner = LongMemEvalRunner::new(
        Arc::clone(&embedder),
        args.content.into(),
        args.top_k,
        args.embed_batch_size,
    )?;

    fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("create output directory {}", args.out_dir.display()))?;
    let suffix = format!(
        "{}_{}_{}",
        sanitize_label(&args.run_label),
        match args.content {
            ContentChoice::User => "user",
            ContentChoice::All => "all",
        },
        match args.embedder {
            EmbedderChoice::Bundled => "bundled",
            EmbedderChoice::Stub => "stub",
        }
    );
    let evidence_path = args.out_dir.join(format!("{suffix}.jsonl"));
    let summary_path = args.out_dir.join(format!("{suffix}.summary.json"));
    let mut evidence = BufWriter::new(
        File::create(&evidence_path)
            .with_context(|| format!("create evidence file {}", evidence_path.display()))?,
    );

    println!("Solo x LongMemEval retrieval benchmark");
    println!("questions: {question_count}");
    println!(
        "embedder: {} {} ({} dimensions)",
        embedder.name(),
        embedder.version(),
        embedder.dim()
    );
    println!("content: {}", ContentMode::from(args.content).as_str());

    let mut results = Vec::with_capacity(question_count);
    for (index, entry) in selected.into_iter().enumerate() {
        let question_id = entry.question_id.clone();
        let result = runner.run_entry(entry).await?;
        serde_json::to_writer(&mut evidence, &result)?;
        evidence.write_all(b"\n")?;
        evidence.flush()?;
        let r5 = result
            .metrics
            .iter()
            .find(|metric| metric.k == 5)
            .map(|metric| metric.recall_any)
            .unwrap_or(0.0);
        println!(
            "[{}/{}] {} R@5={} query={:.1}ms ingest={:.1}ms",
            index + 1,
            question_count,
            question_id,
            if r5 > 0.0 { "hit" } else { "miss" },
            result.query_ms,
            result.ingest_ms
        );
        results.push(result);
    }
    evidence.flush()?;

    let summary = RunSummary {
        schema_version: 1,
        benchmark: "LongMemEval-S cleaned",
        metric_scope: "session retrieval; not end-to-end QA accuracy",
        retrieval_pipeline: "Solo production embedding + HNSW + FTS/RRF recall",
        storage_mode: "isolated unencrypted scratch SQLite database per question",
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        target_os: std::env::consts::OS,
        target_arch: std::env::consts::ARCH,
        solo_version: env!("CARGO_PKG_VERSION"),
        run_label: args.run_label,
        created_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        dataset_path: args.data.display().to_string(),
        dataset_blake3: dataset_hash,
        split_path: args
            .split_file
            .as_ref()
            .map(|path| path.display().to_string()),
        split_subset: args.subset.map(|subset| match subset {
            SplitSubset::Dev => "dev".into(),
            SplitSubset::HeldOut => "held_out".into(),
        }),
        split_seed,
        embedder_name: embedder.name().into(),
        embedder_version: embedder.version().into(),
        embedder_dim: embedder.dim(),
        publishable: matches!(args.embedder, EmbedderChoice::Bundled),
        content_mode: ContentMode::from(args.content).as_str().into(),
        retrieval_limit: args.top_k,
        embed_batch_size: args.embed_batch_size,
        skipped_questions: args.skip,
        overall: aggregate_results(&results, DEFAULT_KS),
        by_question_type: aggregate_by_type(&results, DEFAULT_KS),
        ingest_latency: latency_summary(
            &results
                .iter()
                .map(|result| result.ingest_ms)
                .collect::<Vec<_>>(),
        ),
        query_latency: latency_summary(
            &results
                .iter()
                .map(|result| result.query_ms)
                .collect::<Vec<_>>(),
        ),
        elapsed_seconds: started.elapsed().as_secs_f64(),
        evidence_jsonl: evidence_path.display().to_string(),
    };
    let mut summary_file = BufWriter::new(
        File::create(&summary_path)
            .with_context(|| format!("create summary file {}", summary_path.display()))?,
    );
    serde_json::to_writer_pretty(&mut summary_file, &summary)?;
    summary_file.write_all(b"\n")?;
    summary_file.flush()?;

    println!("\nSESSION RETRIEVAL RESULTS");
    for metric in &summary.overall.by_k {
        println!(
            "R@{:<2} any={:.3} all={:.3} NDCG={:.3}",
            metric.k, metric.recall_any, metric.recall_all, metric.ndcg
        );
    }
    println!("MRR={:.3}", summary.overall.mean_reciprocal_rank);
    println!(
        "query latency p50={:.1}ms p95={:.1}ms p99={:.1}ms",
        summary.query_latency.p50_ms, summary.query_latency.p95_ms, summary.query_latency.p99_ms
    );
    println!("evidence: {}", evidence_path.display());
    println!("summary:  {}", summary_path.display());
    Ok(())
}

fn load_split(args: &Args) -> Result<(Option<HashSet<String>>, Option<u64>)> {
    let Some(path) = &args.split_file else {
        return Ok((None, None));
    };
    let file = File::open(path).with_context(|| format!("open split file {}", path.display()))?;
    let split: SplitFile = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parse split file {}", path.display()))?;
    let ids = match args.subset {
        Some(SplitSubset::Dev) => split.dev,
        Some(SplitSubset::HeldOut) => split.held_out,
        None => bail!("--split-file requires --subset"),
    };
    Ok((Some(ids.into_iter().collect()), split.seed))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("open {} for hashing", path.display()))?,
    );
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn sanitize_label(label: &str) -> String {
    let sanitized: String = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "solo".into()
    } else {
        sanitized
    }
}
