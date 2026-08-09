// SPDX-License-Identifier: Apache-2.0

//! `solo eval ...` - deterministic offline fixture scoring.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Map as JsonMap, Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const BUILTIN_FIXTURES: &[BuiltinFixture] = &[
    BuiltinFixture {
        selector: "memory-baseline",
        contents: MEMORY_BASELINE_FIXTURE,
    },
    BuiltinFixture {
        selector: "memory-corrections",
        contents: MEMORY_CORRECTIONS_FIXTURE,
    },
];

const MEMORY_BASELINE_FIXTURE: &str = r#"{
  "name": "memory-baseline",
  "description": "Core preference, project, and entity recall cases for the offline lexical baseline.",
  "passing_score": 1.0,
  "cases": [
    {
      "id": "preference_lookup",
      "query": "What editor theme does Ada prefer for late-night work?",
      "max_results": 3,
      "expected_memory_ids": ["pref-editor-theme"],
      "memories": [
        {
          "id": "pref-editor-theme",
          "text": "Ada prefers a dark editor theme for late-night coding sessions.",
          "tier": "preference",
          "importance": 0.9,
          "status": "active"
        },
        {
          "id": "pref-coffee",
          "text": "Ada likes dark roast coffee before morning planning.",
          "tier": "preference",
          "importance": 0.4,
          "status": "active"
        },
        {
          "id": "editor-font",
          "text": "Ada changed the editor font to Berkeley Mono for demos.",
          "tier": "episodic",
          "importance": 0.4,
          "status": "active"
        }
      ]
    },
    {
      "id": "roadmap_next_slice",
      "query": "What should ship after the tray, inbox, setup wizard, importers, and policy pack?",
      "max_results": 3,
      "expected_memory_ids": ["roadmap-eval-harness"],
      "memories": [
        {
          "id": "roadmap-eval-harness",
          "text": "After desktop tray, inbox, setup wizard, importers, and the policy pack, Solo needs a minimal eval harness for memory quality regression checks.",
          "tier": "decision",
          "importance": 1.0,
          "status": "active"
        },
        {
          "id": "policy-pack-docs",
          "text": "The memory policy pack documents client rules for retrieval, durable writes, corrections, and contradiction resolution.",
          "tier": "semantic",
          "importance": 0.7,
          "status": "active"
        },
        {
          "id": "tray-followup",
          "text": "The Windows tray follow-up improved supervision, logging, and autostart handling.",
          "tier": "episodic",
          "importance": 0.6,
          "status": "active"
        }
      ]
    },
    {
      "id": "entity_disambiguation",
      "query": "Which Jordan prefers morning calls for contract reviews?",
      "max_results": 2,
      "expected_memory_ids": ["jordan-client-calls"],
      "forbidden_memory_ids": ["jordan-city-trip"],
      "memories": [
        {
          "id": "jordan-client-calls",
          "text": "Jordan Lee from the Acme contract team prefers morning calls for review sessions.",
          "tier": "semantic",
          "importance": 0.8,
          "status": "active"
        },
        {
          "id": "jordan-city-trip",
          "text": "Jordan is also a country Ada wants to visit during a history-focused trip.",
          "tier": "episodic",
          "importance": 0.3,
          "status": "active"
        },
        {
          "id": "review-template",
          "text": "Contract review templates should include renewal date, owner, and risk notes.",
          "tier": "semantic",
          "importance": 0.5,
          "status": "active"
        }
      ]
    }
  ]
}"#;

const MEMORY_CORRECTIONS_FIXTURE: &str = r#"{
  "name": "memory-corrections",
  "description": "Correction and deletion cases that ensure stale memories do not win the offline baseline.",
  "passing_score": 1.0,
  "cases": [
    {
      "id": "superseded_storage_choice",
      "query": "Where should invoice archives be stored?",
      "max_results": 2,
      "expected_memory_ids": ["invoice-storage-current"],
      "forbidden_memory_ids": ["invoice-storage-old"],
      "memories": [
        {
          "id": "invoice-storage-old",
          "text": "Invoice archives should be stored in S3.",
          "tier": "decision",
          "importance": 0.9,
          "status": "superseded"
        },
        {
          "id": "invoice-storage-current",
          "text": "Invoice archives should be stored in local encrypted storage; S3 was rejected for this workflow.",
          "tier": "decision",
          "importance": 1.0,
          "status": "active"
        },
        {
          "id": "receipt-storage",
          "text": "Receipt screenshots are kept with expense notes until monthly close.",
          "tier": "episodic",
          "importance": 0.4,
          "status": "active"
        }
      ]
    },
    {
      "id": "forgotten_secret_is_ignored",
      "query": "What deployment channel should the finance bot use?",
      "max_results": 2,
      "expected_memory_ids": ["finance-bot-channel"],
      "forbidden_memory_ids": ["finance-bot-secret"],
      "memories": [
        {
          "id": "finance-bot-secret",
          "text": "The finance bot uses a private deployment token from a pasted debug log.",
          "tier": "episodic",
          "importance": 0.8,
          "status": "forgotten"
        },
        {
          "id": "finance-bot-channel",
          "text": "The finance bot should deploy through the stable channel after policy review.",
          "tier": "decision",
          "importance": 0.9,
          "status": "active"
        },
        {
          "id": "support-bot-channel",
          "text": "The support bot can deploy through beta when a rollback owner is assigned.",
          "tier": "decision",
          "importance": 0.5,
          "status": "active"
        }
      ]
    }
  ]
}"#;

#[derive(Debug, Subcommand)]
pub enum EvalCommand {
    /// List bundled eval fixtures.
    List(ListArgs),
    /// Run one eval fixture by bundled name or JSON file path.
    Run(RunArgs),
    /// Print a saved eval report by run id or JSON report path.
    Report(ReportArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Bundled fixture name (see `solo eval list`) or a JSON fixture path.
    pub fixture: Option<String>,

    /// Run every bundled eval fixture.
    #[arg(long, conflicts_with = "fixture")]
    pub all: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,

    /// Override every case's top-k cutoff.
    #[arg(long)]
    pub top_k: Option<usize>,

    /// Save the JSON report to --report-dir for later `solo eval report`.
    #[arg(long)]
    pub save: bool,

    /// Directory used by `--save` and `solo eval report <run-id>`.
    #[arg(long, default_value = ".solo/eval-runs")]
    pub report_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    /// Saved run id, or a path to a saved eval JSON report.
    pub run_id: String,

    /// Emit the saved JSON report.
    #[arg(long)]
    pub json: bool,

    /// Directory to search when run_id is not a file path.
    #[arg(long, default_value = ".solo/eval-runs")]
    pub report_dir: PathBuf,
}

pub async fn run(cmd: EvalCommand) -> Result<()> {
    match cmd {
        EvalCommand::List(args) => run_list(args),
        EvalCommand::Run(args) => run_fixture(args),
        EvalCommand::Report(args) => run_report(args),
    }
}

fn run_list(args: ListArgs) -> Result<()> {
    let summaries = builtin_summaries()?;
    if args.json {
        let rows: Vec<Value> = summaries
            .iter()
            .map(|summary| {
                json!({
                    "name": summary.name,
                    "description": summary.description,
                    "cases": summary.case_count,
                    "passing_score": summary.passing_score,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).context("serialize eval fixture list")?
        );
        return Ok(());
    }

    println!("{:<22}  {:>5}  Description", "Fixture", "Cases");
    for summary in summaries {
        println!(
            "{:<22}  {:>5}  {}",
            summary.name, summary.case_count, summary.description
        );
    }
    println!();
    println!("Run `solo eval run <fixture> --json` for CI-friendly output.");
    Ok(())
}

fn run_fixture(args: RunArgs) -> Result<()> {
    if matches!(args.top_k, Some(0)) {
        bail!("--top-k must be greater than 0");
    }

    if args.all {
        return run_all_fixtures(args.json, args.top_k, args.save, &args.report_dir);
    }

    let Some(fixture) = args.fixture else {
        bail!("provide a fixture name/path or pass --all");
    };

    let loaded = load_fixture(&fixture)?;
    let report = score_fixture(&loaded.fixture, args.top_k);
    let mut output = report.to_json();
    if args.save {
        let saved = save_report(output, &args.report_dir, &loaded.fixture.name, "fixture")?;
        output = saved.payload;
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&output).context("serialize eval report")?
        );
    } else {
        print_report(&loaded, &report);
        if let Some((run_id, path)) = saved_report_fields(&output) {
            println!();
            println!("saved report : {run_id}");
            println!("report path  : {}", path.display());
        }
    }

    if !report.passed {
        bail!(
            "eval fixture `{}` failed: score {:.3} < passing_score {:.3}",
            loaded.fixture.name,
            report.score,
            loaded.fixture.passing_score
        );
    }
    Ok(())
}

fn run_all_fixtures(
    json_output: bool,
    top_k: Option<usize>,
    save: bool,
    report_dir: &Path,
) -> Result<()> {
    let mut reports = Vec::with_capacity(BUILTIN_FIXTURES.len());
    for fixture in BUILTIN_FIXTURES {
        let parsed = parse_fixture(fixture.contents)
            .with_context(|| format!("parse bundled eval fixture `{}`", fixture.selector))?;
        let report = score_fixture(&parsed, top_k);
        reports.push(SuiteFixtureReport {
            selector: fixture.selector.to_string(),
            report,
        });
    }

    let suite = SuiteReport::from_reports(&reports);
    let mut output = suite.to_json(&reports);
    if save {
        let saved = save_report(output, report_dir, "bundled", "suite")?;
        output = saved.payload;
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&output).context("serialize eval suite report")?
        );
    } else {
        print_suite_report(&suite, &reports);
        if let Some((run_id, path)) = saved_report_fields(&output) {
            println!();
            println!("saved report : {run_id}");
            println!("report path  : {}", path.display());
        }
    }

    if !suite.passed {
        bail!(
            "eval suite failed: score {:.3} across {} fixtures",
            suite.score,
            suite.fixture_count
        );
    }
    Ok(())
}

fn run_report(args: ReportArgs) -> Result<()> {
    let path = resolve_report_path(&args.run_id, &args.report_dir)?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read eval report {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse eval report {}", path.display()))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).context("serialize saved eval report")?
        );
    } else {
        print_saved_report(&path, &value);
    }
    Ok(())
}

fn save_report(
    mut payload: Value,
    report_dir: &Path,
    stem: &str,
    report_kind: &str,
) -> Result<SavedReport> {
    let saved_at_ms = current_unix_ms()?;
    std::fs::create_dir_all(report_dir)
        .with_context(|| format!("create eval report directory {}", report_dir.display()))?;
    let stem = sanitize_report_stem(stem);

    for counter in 0..1000 {
        let run_id = if counter == 0 {
            format!("eval-{saved_at_ms}-{stem}")
        } else {
            format!("eval-{saved_at_ms}-{stem}-{counter}")
        };
        let path = report_dir.join(format!("{run_id}.json"));
        if path.exists() {
            continue;
        }

        let root = payload
            .as_object_mut()
            .context("eval report JSON root must be an object")?;
        root.insert("run_id".to_string(), json!(run_id));
        root.insert("report_kind".to_string(), json!(report_kind));
        root.insert("saved_at_ms".to_string(), json!(saved_at_ms));
        root.insert("report_path".to_string(), json!(path.display().to_string()));
        let serialized =
            serde_json::to_string_pretty(&payload).context("serialize eval report artifact")?;
        std::fs::write(&path, serialized)
            .with_context(|| format!("write eval report {}", path.display()))?;
        return Ok(SavedReport { payload });
    }

    bail!(
        "could not allocate an eval report id in {}",
        report_dir.display()
    );
}

fn resolve_report_path(run_id: &str, report_dir: &Path) -> Result<PathBuf> {
    let explicit = PathBuf::from(run_id);
    if explicit.is_file() {
        return Ok(explicit);
    }

    let file_name = if run_id.ends_with(".json") {
        run_id.to_string()
    } else {
        format!("{run_id}.json")
    };
    let candidate = report_dir.join(file_name);
    if candidate.is_file() {
        return Ok(candidate);
    }

    bail!(
        "eval report `{run_id}` not found. Looked for {} or pass a report JSON path",
        candidate.display()
    );
}

fn saved_report_fields(payload: &Value) -> Option<(String, PathBuf)> {
    let root = payload.as_object()?;
    let run_id = root.get("run_id")?.as_str()?.to_string();
    let path = root.get("report_path")?.as_str()?;
    Some((run_id, PathBuf::from(path)))
}

fn print_saved_report(path: &Path, value: &Value) {
    let run_id = value
        .get("run_id")
        .and_then(Value::as_str)
        .unwrap_or("(unsaved)");
    let kind = value
        .get("report_kind")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if value.get("suite").is_some() {
                "suite"
            } else {
                "fixture"
            }
        });
    let name = value
        .get("fixture")
        .or_else(|| value.get("suite"))
        .and_then(Value::as_str)
        .unwrap_or("(unknown)");
    let score = value.get("score").and_then(Value::as_f64).unwrap_or(0.0);
    let passed = value
        .get("passed")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    println!("eval report : {run_id}");
    println!("source      : {}", path.display());
    println!("type        : {kind}");
    println!("name        : {name}");
    println!("score       : {score:.3}");
    println!("result      : {}", if passed { "PASS" } else { "FAIL" });

    if let Some(case_count) = value.get("case_count").and_then(Value::as_u64) {
        println!("cases       : {case_count}");
    }
    if let Some(fixture_count) = value.get("fixture_count").and_then(Value::as_u64) {
        println!("fixtures    : {fixture_count}");
    }
}

fn current_unix_ms() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis())
}

fn sanitize_report_stem(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if ch == '_' || ch == '-' {
            Some('-')
        } else {
            None
        };

        match next {
            Some('-') if !last_dash && !out.is_empty() => {
                out.push('-');
                last_dash = true;
            }
            Some(ch) if ch != '-' => {
                out.push(ch);
                last_dash = false;
            }
            _ => {}
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "report".to_string()
    } else {
        trimmed
    }
}

fn builtin_summaries() -> Result<Vec<FixtureSummary>> {
    BUILTIN_FIXTURES
        .iter()
        .map(|fixture| {
            let parsed = parse_fixture(fixture.contents)
                .with_context(|| format!("parse bundled eval fixture `{}`", fixture.selector))?;
            Ok(FixtureSummary {
                name: parsed.name,
                description: parsed.description,
                case_count: parsed.cases.len(),
                passing_score: parsed.passing_score,
            })
        })
        .collect()
}

fn load_fixture(selector: &str) -> Result<LoadedFixture> {
    for fixture in BUILTIN_FIXTURES {
        if fixture.selector == selector {
            let parsed = parse_fixture(fixture.contents)
                .with_context(|| format!("parse bundled eval fixture `{}`", fixture.selector))?;
            return Ok(LoadedFixture {
                source: "builtin".to_string(),
                fixture: parsed,
            });
        }
    }

    let path = Path::new(selector);
    if path.is_file() {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let parsed = parse_fixture(&raw)
            .with_context(|| format!("parse eval fixture {}", path.display()))?;
        return Ok(LoadedFixture {
            source: path.display().to_string(),
            fixture: parsed,
        });
    }

    let names = BUILTIN_FIXTURES
        .iter()
        .map(|fixture| fixture.selector)
        .collect::<Vec<_>>()
        .join(", ");
    bail!("unknown eval fixture `{selector}`. Known bundled fixtures: {names}");
}

fn parse_fixture(raw: &str) -> Result<EvalFixture> {
    let value: Value = serde_json::from_str(raw).context("parse fixture JSON")?;
    let root = as_object(&value, "fixture root")?;
    let name = string_field(root, "name", "fixture root")?;
    let description = string_field(root, "description", "fixture root")?;
    let passing_score = optional_f64_field(root, "passing_score", 1.0, "fixture root")?;
    if !(0.0..=1.0).contains(&passing_score) {
        bail!("fixture `{name}` passing_score must be between 0.0 and 1.0");
    }

    let raw_cases = root
        .get("cases")
        .and_then(Value::as_array)
        .with_context(|| format!("fixture `{name}` cases must be an array"))?;
    if raw_cases.is_empty() {
        bail!("fixture `{name}` must contain at least one case");
    }

    let mut cases = Vec::with_capacity(raw_cases.len());
    for (idx, raw_case) in raw_cases.iter().enumerate() {
        cases.push(parse_case(raw_case, &name, idx)?);
    }

    Ok(EvalFixture {
        name,
        description,
        passing_score,
        cases,
    })
}

fn parse_case(value: &Value, fixture_name: &str, idx: usize) -> Result<EvalCase> {
    let context = format!("fixture `{fixture_name}` case[{idx}]");
    let root = as_object(value, &context)?;
    let id = string_field(root, "id", &context)?;
    let query = string_field(root, "query", &context)?;
    let max_results = optional_usize_field(root, "max_results", 3, &context)?;
    if max_results == 0 {
        bail!("{context} max_results must be greater than 0");
    }

    let expected_memory_ids = string_array_field(root, "expected_memory_ids", &context)?;
    if expected_memory_ids.is_empty() {
        bail!("{context} expected_memory_ids must not be empty");
    }
    let forbidden_memory_ids = optional_string_array_field(root, "forbidden_memory_ids", &context)?;
    let expected_set: BTreeSet<&str> = expected_memory_ids.iter().map(String::as_str).collect();
    for forbidden in &forbidden_memory_ids {
        if expected_set.contains(forbidden.as_str()) {
            bail!("{context} forbidden memory `{forbidden}` is also expected");
        }
    }

    let raw_memories = root
        .get("memories")
        .and_then(Value::as_array)
        .with_context(|| format!("{context} memories must be an array"))?;
    if raw_memories.is_empty() {
        bail!("{context} memories must not be empty");
    }

    let mut memories = Vec::with_capacity(raw_memories.len());
    for (memory_idx, raw_memory) in raw_memories.iter().enumerate() {
        memories.push(parse_memory(raw_memory, &id, memory_idx)?);
    }

    let memory_ids: BTreeSet<&str> = memories.iter().map(|memory| memory.id.as_str()).collect();
    for expected in &expected_memory_ids {
        if !memory_ids.contains(expected.as_str()) {
            bail!("{context} expected memory `{expected}` is not listed in memories");
        }
    }
    for forbidden in &forbidden_memory_ids {
        if !memory_ids.contains(forbidden.as_str()) {
            bail!("{context} forbidden memory `{forbidden}` is not listed in memories");
        }
    }

    Ok(EvalCase {
        id,
        query,
        max_results,
        expected_memory_ids,
        forbidden_memory_ids,
        memories,
    })
}

fn parse_memory(value: &Value, case_id: &str, idx: usize) -> Result<EvalMemory> {
    let context = format!("case `{case_id}` memory[{idx}]");
    let root = as_object(value, &context)?;
    let id = string_field(root, "id", &context)?;
    let text = string_field(root, "text", &context)?;
    let tier = optional_string_field(root, "tier", "semantic", &context)?;
    let status = optional_string_field(root, "status", "active", &context)?;
    let importance = optional_f64_field(root, "importance", 0.5, &context)?;
    if !(0.0..=1.0).contains(&importance) {
        bail!("{context} importance must be between 0.0 and 1.0");
    }

    Ok(EvalMemory {
        id,
        text,
        tier,
        status,
        importance,
    })
}

fn as_object<'a>(value: &'a Value, context: &str) -> Result<&'a JsonMap<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("{context} must be a JSON object"))
}

fn string_field(root: &JsonMap<String, Value>, key: &str, context: &str) -> Result<String> {
    root.get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .with_context(|| format!("{context} `{key}` must be a string"))
}

fn optional_string_field(
    root: &JsonMap<String, Value>,
    key: &str,
    default: &str,
    context: &str,
) -> Result<String> {
    match root.get(key) {
        Some(value) => value
            .as_str()
            .map(ToOwned::to_owned)
            .with_context(|| format!("{context} `{key}` must be a string")),
        None => Ok(default.to_string()),
    }
}

fn string_array_field(
    root: &JsonMap<String, Value>,
    key: &str,
    context: &str,
) -> Result<Vec<String>> {
    let values = root
        .get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("{context} `{key}` must be an array"))?;
    values
        .iter()
        .enumerate()
        .map(|(idx, value)| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .with_context(|| format!("{context} `{key}`[{idx}] must be a string"))
        })
        .collect()
}

fn optional_string_array_field(
    root: &JsonMap<String, Value>,
    key: &str,
    context: &str,
) -> Result<Vec<String>> {
    if root.contains_key(key) {
        string_array_field(root, key, context)
    } else {
        Ok(Vec::new())
    }
}

fn optional_f64_field(
    root: &JsonMap<String, Value>,
    key: &str,
    default: f64,
    context: &str,
) -> Result<f64> {
    match root.get(key) {
        Some(value) => value
            .as_f64()
            .with_context(|| format!("{context} `{key}` must be a number")),
        None => Ok(default),
    }
}

fn optional_usize_field(
    root: &JsonMap<String, Value>,
    key: &str,
    default: usize,
    context: &str,
) -> Result<usize> {
    match root.get(key) {
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .with_context(|| format!("{context} `{key}` must be a non-negative integer")),
        None => Ok(default),
    }
}

fn score_fixture(fixture: &EvalFixture, top_k_override: Option<usize>) -> FixtureReport {
    let cases: Vec<CaseReport> = fixture
        .cases
        .iter()
        .map(|case| score_case(case, top_k_override.unwrap_or(case.max_results)))
        .collect();
    let score = cases.iter().map(|case| case.score).sum::<f64>() / cases.len() as f64;
    let passed = score >= fixture.passing_score;

    FixtureReport {
        fixture: fixture.name.clone(),
        description: fixture.description.clone(),
        passing_score: fixture.passing_score,
        score,
        passed,
        cases,
    }
}

fn score_case(case: &EvalCase, top_k: usize) -> CaseReport {
    let ranked = rank_memories(case);
    let expected: BTreeSet<&str> = case
        .expected_memory_ids
        .iter()
        .map(String::as_str)
        .collect();
    let forbidden: BTreeSet<&str> = case
        .forbidden_memory_ids
        .iter()
        .map(String::as_str)
        .collect();
    let top_ids: BTreeSet<&str> = ranked
        .iter()
        .take(top_k)
        .map(|memory| memory.memory_id.as_str())
        .collect();
    let hits = expected
        .iter()
        .filter(|memory_id| top_ids.contains(**memory_id))
        .count();
    let score = hits as f64 / expected.len() as f64;
    let missing_expected: Vec<String> = expected
        .iter()
        .filter(|memory_id| !top_ids.contains(**memory_id))
        .map(|memory_id| (*memory_id).to_string())
        .collect();
    let forbidden_ranked: Vec<String> = ranked
        .iter()
        .take(top_k)
        .filter(|memory| forbidden.contains(memory.memory_id.as_str()))
        .map(|memory| memory.memory_id.clone())
        .collect();
    let first_expected_rank = ranked
        .iter()
        .filter(|memory| expected.contains(memory.memory_id.as_str()))
        .map(|memory| memory.rank)
        .min();
    let top_results = ranked.into_iter().take(top_k).collect();
    let passed = missing_expected.is_empty() && forbidden_ranked.is_empty();
    let score = if forbidden_ranked.is_empty() {
        score
    } else {
        0.0
    };

    CaseReport {
        id: case.id.clone(),
        query: case.query.clone(),
        top_k,
        score,
        passed,
        expected_memory_ids: case.expected_memory_ids.clone(),
        forbidden_memory_ids: case.forbidden_memory_ids.clone(),
        missing_expected,
        forbidden_ranked,
        first_expected_rank,
        top_results,
    }
}

fn rank_memories(case: &EvalCase) -> Vec<RankedMemory> {
    let query_terms = token_counts(&case.query);
    let mut ranked = Vec::new();

    for memory in &case.memories {
        if memory.status != "active" {
            continue;
        }

        let text_terms = token_counts(&memory.text);
        let mut overlap = 0usize;
        let mut matched_terms = Vec::new();
        for (term, query_count) in &query_terms {
            if let Some(text_count) = text_terms.get(term) {
                overlap += (*query_count).min(*text_count);
                matched_terms.push(term.clone());
            }
        }

        let coverage = if query_terms.is_empty() {
            0.0
        } else {
            overlap as f64 / query_terms.values().sum::<usize>() as f64
        };
        let specificity = matched_terms
            .iter()
            .map(|term| if term.len() >= 6 { 0.04 } else { 0.02 })
            .sum::<f64>();
        let tier_bonus = match memory.tier.as_str() {
            "decision" | "preference" => 0.08,
            "semantic" => 0.05,
            _ => 0.0,
        };
        let importance_bonus = memory.importance.clamp(0.0, 1.0) * 0.04;
        let score = coverage + specificity + tier_bonus + importance_bonus;

        ranked.push(RankedMemory {
            rank: 0,
            memory_id: memory.id.clone(),
            score,
            matched_terms,
            text_preview: preview(&memory.text, 96),
        });
    }

    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.memory_id.cmp(&right.memory_id))
    });
    for (idx, memory) in ranked.iter_mut().enumerate() {
        memory.rank = idx + 1;
    }
    ranked
}

fn token_counts(input: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else {
            push_token(&mut counts, &mut current);
        }
    }
    push_token(&mut counts, &mut current);
    counts
}

fn push_token(counts: &mut BTreeMap<String, usize>, current: &mut String) {
    if !current.is_empty() && !is_stop_word(current) {
        *counts.entry(std::mem::take(current)).or_insert(0) += 1;
    } else {
        current.clear();
    }
}

fn is_stop_word(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "be"
            | "by"
            | "does"
            | "for"
            | "from"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "should"
            | "the"
            | "to"
            | "what"
            | "where"
            | "which"
            | "who"
            | "with"
    )
}

fn preview(input: &str, max_chars: usize) -> String {
    let flattened = input.replace(['\n', '\r'], " ");
    if flattened.chars().count() <= max_chars {
        return flattened;
    }
    flattened.chars().take(max_chars - 3).collect::<String>() + "..."
}

fn print_report(loaded: &LoadedFixture, report: &FixtureReport) {
    println!("eval fixture : {}", report.fixture);
    println!("source       : {}", loaded.source);
    println!("description  : {}", report.description);
    println!("score        : {:.3}", report.score);
    println!("passing_score: {:.3}", report.passing_score);
    println!(
        "result       : {}",
        if report.passed { "PASS" } else { "FAIL" }
    );
    println!();
    println!(
        "{:<28}  {:>6}  {:>5}  {:<16}  {:>9}  Top hit",
        "Case", "Score", "TopK", "Expected rank", "Forbidden"
    );
    for case in &report.cases {
        let expected_rank = case
            .first_expected_rank
            .map(|rank| rank.to_string())
            .unwrap_or_else(|| "-".to_string());
        let top_hit = case
            .top_results
            .first()
            .map(|memory| memory.memory_id.as_str())
            .unwrap_or("-");
        println!(
            "{:<28}  {:>6.3}  {:>5}  {:<16}  {:>9}  {}",
            case.id,
            case.score,
            case.top_k,
            expected_rank,
            case.forbidden_ranked.len(),
            top_hit
        );
    }
}

fn print_suite_report(suite: &SuiteReport, reports: &[SuiteFixtureReport]) {
    println!("eval suite : bundled");
    println!("fixtures   : {}", suite.fixture_count);
    println!("cases      : {}", suite.case_count);
    println!("score      : {:.3}", suite.score);
    println!(
        "result     : {}",
        if suite.passed { "PASS" } else { "FAIL" }
    );
    println!();
    println!(
        "{:<22}  {:>6}  {:>5}  {:<6}  Description",
        "Fixture", "Score", "Cases", "Result"
    );
    for item in reports {
        println!(
            "{:<22}  {:>6.3}  {:>5}  {:<6}  {}",
            item.selector,
            item.report.score,
            item.report.cases.len(),
            if item.report.passed { "PASS" } else { "FAIL" },
            item.report.description
        );
    }
    println!();
    println!("Run `solo eval run <fixture> --json` to inspect individual cases.");
}

#[derive(Debug)]
struct BuiltinFixture {
    selector: &'static str,
    contents: &'static str,
}

#[derive(Debug)]
struct FixtureSummary {
    name: String,
    description: String,
    case_count: usize,
    passing_score: f64,
}

#[derive(Debug)]
struct LoadedFixture {
    source: String,
    fixture: EvalFixture,
}

#[derive(Debug)]
struct SuiteFixtureReport {
    selector: String,
    report: FixtureReport,
}

#[derive(Debug)]
struct SavedReport {
    payload: Value,
}

#[derive(Debug)]
struct EvalFixture {
    name: String,
    description: String,
    passing_score: f64,
    cases: Vec<EvalCase>,
}

#[derive(Debug)]
struct EvalCase {
    id: String,
    query: String,
    max_results: usize,
    expected_memory_ids: Vec<String>,
    forbidden_memory_ids: Vec<String>,
    memories: Vec<EvalMemory>,
}

#[derive(Debug)]
struct EvalMemory {
    id: String,
    text: String,
    tier: String,
    status: String,
    importance: f64,
}

#[derive(Debug)]
struct FixtureReport {
    fixture: String,
    description: String,
    passing_score: f64,
    score: f64,
    passed: bool,
    cases: Vec<CaseReport>,
}

#[derive(Debug)]
struct SuiteReport {
    score: f64,
    passed: bool,
    fixture_count: usize,
    case_count: usize,
}

impl SuiteReport {
    fn from_reports(reports: &[SuiteFixtureReport]) -> Self {
        let fixture_count = reports.len();
        let case_count = reports
            .iter()
            .map(|item| item.report.cases.len())
            .sum::<usize>();
        let score = if fixture_count == 0 {
            0.0
        } else {
            reports.iter().map(|item| item.report.score).sum::<f64>() / fixture_count as f64
        };
        let passed = reports.iter().all(|item| item.report.passed);

        Self {
            score,
            passed,
            fixture_count,
            case_count,
        }
    }

    fn to_json(&self, reports: &[SuiteFixtureReport]) -> Value {
        json!({
            "suite": "bundled",
            "score": round3(self.score),
            "passed": self.passed,
            "fixture_count": self.fixture_count,
            "case_count": self.case_count,
            "fixtures": reports.iter().map(SuiteFixtureReport::to_json).collect::<Vec<_>>(),
        })
    }
}

impl SuiteFixtureReport {
    fn to_json(&self) -> Value {
        let mut fixture = self.report.to_json();
        fixture["selector"] = json!(self.selector);
        fixture
    }
}

impl FixtureReport {
    fn to_json(&self) -> Value {
        json!({
            "fixture": self.fixture,
            "description": self.description,
            "score": round3(self.score),
            "passing_score": round3(self.passing_score),
            "passed": self.passed,
            "case_count": self.cases.len(),
            "cases": self.cases.iter().map(CaseReport::to_json).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug)]
struct CaseReport {
    id: String,
    query: String,
    top_k: usize,
    score: f64,
    passed: bool,
    expected_memory_ids: Vec<String>,
    forbidden_memory_ids: Vec<String>,
    missing_expected: Vec<String>,
    forbidden_ranked: Vec<String>,
    first_expected_rank: Option<usize>,
    top_results: Vec<RankedMemory>,
}

impl CaseReport {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "query": self.query,
            "top_k": self.top_k,
            "score": round3(self.score),
            "passed": self.passed,
            "expected_memory_ids": self.expected_memory_ids,
            "forbidden_memory_ids": self.forbidden_memory_ids,
            "missing_expected": self.missing_expected,
            "forbidden_ranked": self.forbidden_ranked,
            "first_expected_rank": self.first_expected_rank,
            "top_results": self.top_results.iter().map(RankedMemory::to_json).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug)]
struct RankedMemory {
    rank: usize,
    memory_id: String,
    score: f64,
    matched_terms: Vec<String>,
    text_preview: String,
}

impl RankedMemory {
    fn to_json(&self) -> Value {
        json!({
            "rank": self.rank,
            "memory_id": self.memory_id,
            "score": round3(self.score),
            "matched_terms": self.matched_terms,
            "text_preview": self.text_preview,
        })
    }
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fixtures_parse_and_pass() {
        for fixture in BUILTIN_FIXTURES {
            let parsed = parse_fixture(fixture.contents).expect("parse bundled fixture");
            let report = score_fixture(&parsed, None);
            assert!(
                report.passed,
                "fixture {} should pass: {report:#?}",
                fixture.selector
            );
        }
    }

    #[test]
    fn bundled_fixtures_match_repo_copies_when_available() {
        let fixture_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/fixtures");
        for fixture in BUILTIN_FIXTURES {
            let path = fixture_dir.join(format!("{}.json", fixture.selector));
            if path.is_file() {
                let repo_copy = std::fs::read_to_string(&path).expect("read repo fixture copy");
                let repo_copy = repo_copy.replace("\r\n", "\n");
                let bundled_copy = fixture.contents.replace("\r\n", "\n");
                assert_eq!(repo_copy.trim_end(), bundled_copy.trim_end());
            }
        }
    }

    #[test]
    fn inactive_memories_do_not_rank() {
        let fixture = parse_fixture(
            r#"{
                "name": "inactive-test",
                "description": "test",
                "cases": [{
                    "id": "case",
                    "query": "where should invoices be stored",
                    "expected_memory_ids": ["current"],
                    "memories": [
                        {"id": "old", "text": "invoices stored in s3", "status": "superseded"},
                        {"id": "current", "text": "invoices stored locally", "status": "active"}
                    ]
                }]
            }"#,
        )
        .expect("parse fixture");

        let report = score_fixture(&fixture, None);

        assert!(report.passed, "{report:#?}");
        assert_eq!(report.cases[0].top_results[0].memory_id, "current");
    }

    #[test]
    fn forbidden_memories_make_case_fail_when_ranked() {
        let fixture = parse_fixture(
            r#"{
                "name": "forbidden-test",
                "description": "test",
                "cases": [{
                    "id": "case",
                    "query": "where should invoices be stored in s3",
                    "expected_memory_ids": ["current"],
                    "forbidden_memory_ids": ["old"],
                    "memories": [
                        {"id": "old", "text": "invoices stored in s3", "status": "active"},
                        {"id": "current", "text": "invoices stored locally", "status": "active"}
                    ]
                }]
            }"#,
        )
        .expect("parse fixture");

        let report = score_fixture(&fixture, None);

        assert!(!report.passed, "{report:#?}");
        assert_eq!(report.cases[0].score, 0.0);
        assert_eq!(report.cases[0].forbidden_ranked, vec!["old"]);
    }

    #[test]
    fn forbidden_memory_ids_must_not_overlap_expected_ids() {
        let err = parse_fixture(
            r#"{
                "name": "bad-forbidden-test",
                "description": "test",
                "cases": [{
                    "id": "case",
                    "query": "where should invoices be stored",
                    "expected_memory_ids": ["current"],
                    "forbidden_memory_ids": ["current"],
                    "memories": [
                        {"id": "current", "text": "invoices stored locally", "status": "active"}
                    ]
                }]
            }"#,
        )
        .expect_err("overlap should be rejected");

        assert!(err.to_string().contains("also expected"), "{err}");
    }
}
