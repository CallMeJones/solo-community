// SPDX-License-Identifier: Apache-2.0

//! `solo audit list|purge|export` — admin CLI for the Memory Library audit log.
//!
//! v0.8.0 P4. The CLI is the only surface exposing audit-table reads —
//! the audit table is intentionally NOT routed through MCP or
//! HTTP (operator tier, not request tier).
//!
//! Three subcommands:
//!
//!   * `solo audit list` — paginated read, filterable by principal /
//!     operation / since.
//!   * `solo audit purge --older-than <duration>` — manual retention
//!     sweep, alternative to the background sweep gated on
//!     `[audit] purge_interval_secs`. Requires `--confirm`.
//!   * `solo audit export --format jsonl|csv` — bulk export.

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use clap::{Args, Subcommand, ValueEnum};
use solo_storage::{KeyMaterial, SoloConfig};
use std::io::Write;
use std::path::PathBuf;

use crate::commands::common::read_passphrase;

#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// List recent audit rows (newest first).
    List(ListArgs),
    /// Delete rows older than `--older-than` (e.g. `30d`, `12h`).
    /// Requires `--confirm`; refuses without it.
    Purge(PurgeArgs),
    /// Export the audit log as JSON Lines (default) or CSV.
    Export(ExportArgs),
}

#[derive(Debug, Args)]
pub struct CommonAuditArgs {
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub common: CommonAuditArgs,
    /// Filter by `principal_subject` (exact match).
    #[arg(long)]
    pub principal: Option<String>,
    /// Filter by `operation` (exact match, e.g. `memory.remember`).
    #[arg(long)]
    pub operation: Option<String>,
    /// Filter to rows with `ts_ms >= <RFC3339 timestamp>` (e.g.
    /// `2026-05-01T00:00:00Z`).
    #[arg(long)]
    pub since: Option<String>,
    /// Max rows to return. Default 20.
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ListFormat::Table)]
    pub format: ListFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ListFormat {
    Table,
    Json,
}

#[derive(Debug, Args)]
pub struct PurgeArgs {
    #[command(flatten)]
    pub common: CommonAuditArgs,
    /// Delete rows older than this duration. Accepts a suffix-based
    /// shorthand: `90d` (days), `12h` (hours), `30m` (minutes), `90s`
    /// (seconds). Each component is a positive integer.
    #[arg(long)]
    pub older_than: String,
    /// Required: confirm the destructive action.
    #[arg(long)]
    pub confirm: bool,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    #[command(flatten)]
    pub common: CommonAuditArgs,
    /// Output format. Default `jsonl` (one JSON object per line).
    #[arg(long, value_enum, default_value_t = ExportFormat::Jsonl)]
    pub format: ExportFormat,
    /// Output path. `-` (or omitted) = stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ExportFormat {
    Jsonl,
    Csv,
}

/// Parse `90d` / `12h` / `30m` / `90s` into seconds. Returns Err on
/// negative, empty, or unrecognised input.
fn parse_duration_to_secs(raw: &str) -> Result<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("--older-than must not be empty");
    }
    let (digits, suffix) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| trimmed.split_at(i))
        .unwrap_or((trimmed, "s"));
    if digits.is_empty() {
        bail!("--older-than must start with a positive integer (e.g. `90d`, `12h`)");
    }
    let n: u64 = digits
        .parse()
        .map_err(|e| anyhow::anyhow!("--older-than: parse `{digits}`: {e}"))?;
    let secs_per_unit: u64 = match suffix {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 60 * 60 * 24,
        other => {
            bail!("--older-than: unknown unit `{other}`. Use one of: s, m, h, d (e.g. `90d`).")
        }
    };
    n.checked_mul(secs_per_unit).ok_or_else(|| {
        anyhow::anyhow!("--older-than: overflow for `{trimmed}` (try a smaller value)")
    })
}

pub async fn run(cmd: AuditCommand) -> Result<()> {
    match cmd {
        AuditCommand::List(args) => run_list(args).await,
        AuditCommand::Purge(args) => run_purge(args).await,
        AuditCommand::Export(args) => run_export(args).await,
    }
}

/// Common bootstrap: resolve the data dir and open the Community database
/// with a fresh SQLCipher key. Does NOT go through `prepare_oneshot` —
/// audit operations are admin-tier and don't need to spin up the writer
/// / reader pool / HNSW. Just open one short-lived connection.
fn open_library_audit_conn(common: &CommonAuditArgs) -> Result<rusqlite::Connection> {
    let data_dir = match common.data_dir.clone() {
        Some(p) => p,
        None => solo_storage::default_data_dir()
            .context("could not resolve default data dir; pass --data-dir explicitly")?,
    };
    let config_path = data_dir.join("solo.config.toml");
    if !config_path.is_file() {
        bail!(
            "solo.config.toml not found at {}. Run `solo init` first.",
            config_path.display()
        );
    }
    let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
    let salt = config.salt_bytes().context("decode salt from config")?;

    let passphrase = read_passphrase()?;
    let key =
        KeyMaterial::derive(&passphrase, &salt).context("derive key from passphrase + salt")?;
    drop(passphrase);

    let db_path = data_dir.join(solo_storage::COMMUNITY_DB_FILENAME);
    solo_storage::init::open_sqlcipher(&db_path, &key)
        .context("open Community Memory Library audit DB")
}

async fn run_list(args: ListArgs) -> Result<()> {
    let conn = open_library_audit_conn(&args.common)?;

    let since_ms: Option<i64> = match args.since.as_deref() {
        None => None,
        Some(s) => Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| anyhow::anyhow!("--since: parse RFC3339 `{s}`: {e}"))?
                .timestamp_millis(),
        ),
    };

    // Build dynamic WHERE clause based on supplied filters.
    let mut sql = String::from(
        "SELECT audit_id, ts_ms, principal_subject, operation, target_id, result, details_json \
         FROM audit_events",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(p) = args.principal.clone() {
        clauses.push("principal_subject = ?");
        params.push(p.into());
    }
    if let Some(op) = args.operation.clone() {
        clauses.push("operation = ?");
        params.push(op.into());
    }
    if let Some(s) = since_ms {
        clauses.push("ts_ms >= ?");
        params.push(s.into());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY ts_ms DESC, audit_id DESC LIMIT ?");
    params.push((args.limit as i64).into());

    let mut stmt = conn.prepare(&sql).context("prepare audit list")?;
    let rows: Vec<AuditRow> = stmt
        .query_map(rusqlite::params_from_iter(&params), row_to_audit_row)
        .context("query audit list")?
        .filter_map(|r| r.ok())
        .collect();

    match args.format {
        ListFormat::Json => {
            let values: Vec<serde_json::Value> = rows.iter().map(audit_row_to_json).collect();
            let json =
                serde_json::to_string_pretty(&values).context("serialize audit list to JSON")?;
            println!("{json}");
        }
        ListFormat::Table => print_table(&rows),
    }
    Ok(())
}

fn audit_row_to_json(r: &AuditRow) -> serde_json::Value {
    serde_json::json!({
        "audit_id": r.audit_id,
        "ts_ms": r.ts_ms,
        "principal_subject": r.principal_subject,
        "operation": r.operation,
        "target_id": r.target_id,
        "result": r.result,
        "details_json": r.details_json,
    })
}

async fn run_purge(args: PurgeArgs) -> Result<()> {
    if !args.confirm {
        bail!(
            "refusing to purge without --confirm. \
             Re-run with `solo audit purge --older-than <dur> --confirm`."
        );
    }
    let secs = parse_duration_to_secs(&args.older_than)?;
    let cutoff_ms = chrono::Utc::now().timestamp_millis()
        - i64::try_from(secs * 1000).context("--older-than too large to represent in ms")?;

    let mut conn = open_library_audit_conn(&args.common)?;
    let deleted =
        solo_storage::purge_older_than(&mut conn, cutoff_ms).context("purge_older_than")?;
    println!(
        "purged {deleted} audit row(s) older than {} ({} seconds)",
        args.older_than, secs
    );
    Ok(())
}

async fn run_export(args: ExportArgs) -> Result<()> {
    let conn = open_library_audit_conn(&args.common)?;
    let mut stmt = conn
        .prepare(
            "SELECT audit_id, ts_ms, principal_subject, operation, target_id, result, details_json
             FROM audit_events
             ORDER BY ts_ms DESC, audit_id DESC",
        )
        .context("prepare audit export")?;
    let rows = stmt
        .query_map([], row_to_audit_row)
        .context("query audit export")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collect audit export rows")?;

    let writer: Box<dyn Write> = match args.out.as_ref().and_then(|p| p.to_str()) {
        None | Some("-") => Box::new(std::io::stdout()),
        Some(_) => {
            let path = args.out.clone().unwrap();
            let file = std::fs::File::create(&path)
                .with_context(|| format!("create {}", path.display()))?;
            Box::new(std::io::BufWriter::new(file))
        }
    };
    let mut w = writer;
    match args.format {
        ExportFormat::Jsonl => {
            for r in &rows {
                let line = serde_json::to_string(&audit_row_to_json(r))
                    .context("serialize audit row to JSON")?;
                writeln!(w, "{line}").context("write JSONL line")?;
            }
        }
        ExportFormat::Csv => {
            // RFC 4180 minimal: header + escape quotes by doubling them.
            writeln!(
                w,
                "audit_id,ts_ms,principal_subject,operation,target_id,result,details_json"
            )?;
            for r in &rows {
                writeln!(
                    w,
                    "{},{},{},{},{},{},{}",
                    r.audit_id,
                    r.ts_ms,
                    csv_escape(r.principal_subject.as_deref().unwrap_or("")),
                    csv_escape(&r.operation),
                    csv_escape(r.target_id.as_deref().unwrap_or("")),
                    csv_escape(&r.result),
                    csv_escape(r.details_json.as_deref().unwrap_or("")),
                )?;
            }
        }
    }
    w.flush().context("flush audit export writer")?;
    Ok(())
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

fn print_table(rows: &[AuditRow]) {
    if rows.is_empty() {
        println!("(no audit rows match)");
        return;
    }
    println!(
        "{:<6} {:<25} {:<24} {:<22} {:<10}  details",
        "id", "ts", "principal", "operation", "result"
    );
    for r in rows {
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(r.ts_ms)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| format!("invalid({})", r.ts_ms));
        let principal = r.principal_subject.as_deref().unwrap_or("-");
        let details = r.details_json.as_deref().unwrap_or("");
        println!(
            "{:<6} {:<25} {:<24} {:<22} {:<10}  {}",
            r.audit_id, ts, principal, r.operation, r.result, details
        );
    }
}

#[derive(Debug)]
struct AuditRow {
    audit_id: i64,
    ts_ms: i64,
    principal_subject: Option<String>,
    operation: String,
    target_id: Option<String>,
    result: String,
    details_json: Option<String>,
}

fn row_to_audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRow> {
    Ok(AuditRow {
        audit_id: row.get(0)?,
        ts_ms: row.get(1)?,
        principal_subject: row.get(2)?,
        operation: row.get(3)?,
        target_id: row.get(4)?,
        result: row.get(5)?,
        details_json: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_handles_days_hours_minutes_seconds() {
        assert_eq!(parse_duration_to_secs("90d").unwrap(), 90 * 86_400);
        assert_eq!(parse_duration_to_secs("12h").unwrap(), 12 * 3600);
        assert_eq!(parse_duration_to_secs("30m").unwrap(), 30 * 60);
        assert_eq!(parse_duration_to_secs("90s").unwrap(), 90);
        // Default unit when no suffix → seconds.
        assert_eq!(parse_duration_to_secs("60").unwrap(), 60);
    }

    #[test]
    fn parse_duration_rejects_unknown_unit() {
        assert!(parse_duration_to_secs("90y").is_err());
        assert!(parse_duration_to_secs("d").is_err());
        assert!(parse_duration_to_secs("").is_err());
    }

    #[test]
    fn csv_escape_handles_commas_quotes_newlines() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("with,comma"), "\"with,comma\"");
        assert_eq!(csv_escape("with\"quote"), "\"with\"\"quote\"");
        assert_eq!(csv_escape("with\nnewline"), "\"with\nnewline\"");
    }
}
