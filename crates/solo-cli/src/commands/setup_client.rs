// SPDX-License-Identifier: Apache-2.0

//! `solo setup-client` - MCP client configuration.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process;
use std::time::Duration;
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue, value as toml_value};

const DEFAULT_MCP_URL: &str = "http://127.0.0.1:17821/mcp";
const DOCTOR_REQUIRED_MCP_TOOLS: &[&str] = &["memory_context", "memory_inbox", "memory_review"];

#[derive(Debug, Subcommand)]
pub enum SetupClientCommand {
    /// List supported MCP clients and detected config paths.
    List(ListArgs),
    /// Verify generated MCP client config locally.
    Verify(VerifyArgs),
    /// Diagnose local client config and MCP endpoint reachability.
    Doctor(DoctorArgs),
    /// Preview or write Claude Desktop config for Solo MCP.
    ClaudeDesktop(SetupArgs),
    /// Preview or write Cursor config for Solo MCP.
    Cursor(SetupArgs),
    /// Preview or write Codex config for Solo MCP.
    Codex(CodexSetupArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Client to verify. Omit to check every supported client.
    #[arg(value_enum)]
    client: Option<Client>,

    /// Codex config scope to verify when checking Codex.
    #[arg(long, value_enum, default_value_t = CodexScope::User)]
    scope: CodexScope,

    /// Project directory for `--scope project`; defaults to the current directory.
    #[arg(long)]
    project_dir: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Client to diagnose. Omit to check every supported client.
    #[arg(value_enum)]
    client: Option<Client>,

    /// Codex config scope to diagnose when checking Codex.
    #[arg(long, value_enum, default_value_t = CodexScope::User)]
    scope: CodexScope,

    /// Project directory for `--scope project`; defaults to the current directory.
    #[arg(long)]
    project_dir: Option<PathBuf>,

    /// MCP HTTP endpoint to probe.
    #[arg(long, default_value = DEFAULT_MCP_URL)]
    url: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Transport to configure. HTTP uses `npx mcp-remote` for stdio-only clients.
    #[arg(long, value_enum, default_value_t = Transport::Http)]
    pub transport: Transport,

    /// MCP HTTP endpoint for `--transport http`.
    #[arg(long, default_value = DEFAULT_MCP_URL)]
    pub url: String,

    /// Data directory to pass to `solo mcp-stdio` for `--transport stdio`.
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Preview only. This is the default unless --apply is set.
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,

    /// Write the config file after making a timestamped backup if it exists.
    #[arg(long, conflicts_with = "dry_run")]
    pub apply: bool,
}

#[derive(Debug, Args)]
pub struct CodexSetupArgs {
    /// Codex config scope to write.
    #[arg(long, value_enum, default_value_t = CodexScope::User)]
    pub scope: CodexScope,

    /// Project directory for `--scope project`; defaults to the current directory.
    #[arg(long)]
    pub project_dir: Option<PathBuf>,

    /// Transport to configure. HTTP points Codex directly at the Solo daemon.
    #[arg(long, value_enum, default_value_t = Transport::Http)]
    pub transport: Transport,

    /// MCP HTTP endpoint for `--transport http`.
    #[arg(long, default_value = DEFAULT_MCP_URL)]
    pub url: String,

    /// Data directory to pass to `solo mcp-stdio` for `--transport stdio`.
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Preview only. This is the default unless --apply is set.
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,

    /// Write the config file after making a timestamped backup if it exists.
    #[arg(long, conflicts_with = "dry_run")]
    pub apply: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    Http,
    Stdio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CodexScope {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Client {
    ClaudeDesktop,
    Cursor,
    Codex,
}

impl Client {
    fn display_name(self) -> &'static str {
        match self {
            Client::ClaudeDesktop => "Claude Desktop",
            Client::Cursor => "Cursor",
            Client::Codex => "Codex",
        }
    }

    fn cli_name(self) -> &'static str {
        match self {
            Client::ClaudeDesktop => "claude-desktop",
            Client::Cursor => "cursor",
            Client::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperatingSystem {
    Windows,
    Macos,
    Linux,
}

impl OperatingSystem {
    fn current() -> Self {
        match env::consts::OS {
            "windows" => Self::Windows,
            "macos" => Self::Macos,
            _ => Self::Linux,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }
}

fn supported_clients() -> [Client; 3] {
    [Client::ClaudeDesktop, Client::Cursor, Client::Codex]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathDetection {
    pub path: Option<PathBuf>,
    pub note: Option<String>,
}

#[derive(Debug)]
struct ConfigInspection {
    json: Option<Value>,
    status: String,
}

#[derive(Debug)]
struct TomlInspection {
    raw: Option<String>,
    status: String,
}

#[derive(Debug)]
struct ApplyOutcome {
    backup_path: Option<PathBuf>,
}

#[derive(Debug)]
struct VerifyRow {
    client: Client,
    path: Option<PathBuf>,
    status: VerifyStatus,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpEndpointProbe {
    url: String,
    status: McpEndpointStatus,
    detail: String,
    http_status: Option<u16>,
    tools: Option<McpToolsProbe>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpToolsProbe {
    tool_count: usize,
    missing_required_tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpEndpointStatus {
    Reachable,
    AuthRequired,
    WrongPath,
    Unsupported,
    Unreachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyStatus {
    Ok,
    Missing,
    Invalid,
    Unknown,
}

pub async fn run(cmd: SetupClientCommand) -> Result<()> {
    match cmd {
        SetupClientCommand::List(args) => run_list(args),
        SetupClientCommand::Verify(args) => run_verify(args),
        SetupClientCommand::Doctor(args) => run_doctor(args),
        SetupClientCommand::ClaudeDesktop(args) => run_setup(Client::ClaudeDesktop, args),
        SetupClientCommand::Cursor(args) => run_setup(Client::Cursor, args),
        SetupClientCommand::Codex(args) => run_codex_setup(args),
    }
}

fn run_list(args: ListArgs) -> Result<()> {
    let os = OperatingSystem::current();
    let clients = supported_clients();

    match args.format {
        OutputFormat::Table => {
            println!("Supported MCP clients (current OS: {})", os.label());
            println!();
            println!(
                "{:<16}  {:<28}  {:<9}  Config path",
                "Client", "Command", "Status"
            );
            for client in clients {
                let detection = detect_config_path(client, os, current_env_var);
                let status = path_status(&detection);
                let path = detection
                    .path
                    .as_deref()
                    .map(display_path)
                    .unwrap_or_else(|| detection.note.unwrap_or_else(|| "unavailable".to_string()));
                println!(
                    "{:<16}  {:<28}  {:<9}  {}",
                    client.display_name(),
                    format!("setup-client {}", client.cli_name()),
                    status,
                    path
                );
            }
            println!();
            println!(
                "Run `solo setup-client <client> --dry-run` to preview or `--apply` to write a backed-up config."
            );
        }
        OutputFormat::Json => {
            let rows: Vec<Value> = clients
                .into_iter()
                .map(|client| {
                    let detection = detect_config_path(client, os, current_env_var);
                    json!({
                        "client": client.cli_name(),
                        "display_name": client.display_name(),
                        "os": os.label(),
                        "config_path": detection.path.as_deref().map(display_path),
                        "status": path_status(&detection),
                        "note": detection.note,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&rows).context("serialize setup-client list")?
            );
        }
    }

    Ok(())
}

fn run_verify(args: VerifyArgs) -> Result<()> {
    if args.project_dir.is_some() && args.scope != CodexScope::Project {
        bail!("--project-dir can only be used with --scope project");
    }

    let os = OperatingSystem::current();
    let selected_clients: Vec<Client> = args
        .client
        .map(|client| vec![client])
        .unwrap_or_else(|| supported_clients().to_vec());
    let rows: Vec<VerifyRow> = selected_clients
        .iter()
        .copied()
        .map(|client| {
            verify_client_config(
                client,
                os,
                &current_env_var,
                args.scope,
                args.project_dir.as_deref(),
            )
        })
        .collect();

    match args.format {
        OutputFormat::Table => {
            println!(
                "MCP client config verification (current OS: {})",
                os.label()
            );
            println!();
            println!(
                "{:<16}  {:<9}  {:<42}  Detail",
                "Client", "Status", "Config path"
            );
            for row in &rows {
                println!(
                    "{:<16}  {:<9}  {:<42}  {}",
                    row.client.display_name(),
                    row.status.label(),
                    row.path
                        .as_deref()
                        .map(display_path)
                        .unwrap_or_else(|| "unavailable".to_string()),
                    row.detail
                );
            }
        }
        OutputFormat::Json => {
            let json_rows: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "client": row.client.cli_name(),
                        "display_name": row.client.display_name(),
                        "config_path": row.path.as_deref().map(display_path),
                        "status": row.status.label(),
                        "detail": row.detail,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json_rows)
                    .context("serialize setup-client verify")?
            );
        }
    }

    let has_invalid = rows.iter().any(|row| row.status == VerifyStatus::Invalid);
    let selected_client_failed = args
        .client
        .is_some_and(|_| rows.iter().any(|row| row.status != VerifyStatus::Ok));
    if has_invalid || selected_client_failed {
        bail!("setup-client verify found a config problem");
    }

    Ok(())
}

fn run_doctor(args: DoctorArgs) -> Result<()> {
    if args.project_dir.is_some() && args.scope != CodexScope::Project {
        bail!("--project-dir can only be used with --scope project");
    }

    let os = OperatingSystem::current();
    let selected_clients: Vec<Client> = args
        .client
        .map(|client| vec![client])
        .unwrap_or_else(|| supported_clients().to_vec());
    let rows: Vec<VerifyRow> = selected_clients
        .iter()
        .copied()
        .map(|client| {
            verify_client_config(
                client,
                os,
                &current_env_var,
                args.scope,
                args.project_dir.as_deref(),
            )
        })
        .collect();
    let endpoint = probe_mcp_endpoint(&args.url, Duration::from_secs(2));

    match args.format {
        OutputFormat::Table => {
            println!("MCP client doctor (current OS: {})", os.label());
            println!();
            println!("MCP library : Community Memory Library");
            println!(
                "MCP endpoint: {} ({})",
                endpoint.status.label(),
                endpoint.detail
            );
            if let Some(tools) = endpoint.tools.as_ref() {
                println!("MCP tools   : {}", tools.detail());
            }
            println!();
            println!(
                "{:<16}  {:<10}  {:<9}  {:<42}  Detail",
                "Client", "Config", "Solo", "Config path"
            );
            for row in &rows {
                println!(
                    "{:<16}  {:<10}  {:<9}  {:<42}  {}",
                    row.client.display_name(),
                    row.status.label(),
                    server_entry_status(row.status),
                    row.path
                        .as_deref()
                        .map(display_path)
                        .unwrap_or_else(|| "unavailable".to_string()),
                    row.detail
                );
            }
        }
        OutputFormat::Json => {
            let json_rows: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "client": row.client.cli_name(),
                        "display_name": row.client.display_name(),
                        "config_path": row.path.as_deref().map(display_path),
                        "config_status": row.status.label(),
                        "solo_entry": server_entry_status(row.status),
                        "detail": row.detail,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "library": "Community Memory Library",
                    "mcp_endpoint": {
                        "url": endpoint.url,
                        "status": endpoint.status.label(),
                        "detail": endpoint.detail,
                        "http_status": endpoint.http_status,
                        "tools": endpoint.tools.as_ref().map(|tools| json!({
                            "tool_count": tools.tool_count,
                            "missing_required_tools": tools.missing_required_tools,
                        })),
                    },
                    "clients": json_rows,
                }))
                .context("serialize setup-client doctor")?
            );
        }
    }

    Ok(())
}

fn run_setup(client: Client, args: SetupArgs) -> Result<()> {
    if args.transport == Transport::Stdio {
        println!(
            "Warning: stdio mode lets the MCP client own a Solo writer process and can conflict with a running daemon."
        );
        println!("No passphrase is written into the client config.");
        println!();
    }

    let os = OperatingSystem::current();
    let detection = detect_config_path(client, os, current_env_var);
    let inspection = detection.path.as_deref().map(inspect_config).transpose()?;
    let apply = args.apply;

    println!("Client      : {}", client.display_name());
    println!("Mode        : {}", if apply { "apply" } else { "dry-run" });
    println!("Transport   : {}", args.transport.label());
    println!("Library     : Community Memory Library");
    println!("Config path : {}", describe_detection_path(&detection));
    if let Some(inspection) = &inspection {
        println!("Config state: {}", inspection.status);
    }
    println!();

    let server_entry = server_entry(args.transport, &args.url, args.data_dir.as_deref());
    if apply {
        let path = detection.path.as_deref().with_context(|| {
            detection
                .note
                .clone()
                .unwrap_or_else(|| "could not detect config path".to_string())
        })?;
        let outcome = apply_config(path, args.transport, &args.url, args.data_dir.as_deref())?;

        println!("Ensured this `mcpServers.solo` entry:");
        println!(
            "{}",
            serde_json::to_string_pretty(&server_entry)
                .context("serialize setup-client server entry")?
        );
        println!();
        println!("Wrote config: {}", display_path(path));
        if let Some(backup_path) = outcome.backup_path {
            println!("Backup      : {}", display_path(&backup_path));
        }
        println!("No passphrase was written into the client config.");
    } else {
        let existing_json = inspection.as_ref().and_then(|i| i.json.clone());
        let preview = preview_config(
            existing_json,
            args.transport,
            &args.url,
            args.data_dir.as_deref(),
        );
        println!("Would ensure this `mcpServers.solo` entry:");
        println!(
            "{}",
            serde_json::to_string_pretty(&server_entry)
                .context("serialize setup-client server entry")?
        );
        println!();
        println!("Resulting config preview:");
        println!(
            "{}",
            serde_json::to_string_pretty(&preview).context("serialize setup-client preview")?
        );
        println!();
        println!("No files were written.");
    }

    Ok(())
}

fn run_codex_setup(args: CodexSetupArgs) -> Result<()> {
    if args.project_dir.is_some() && args.scope != CodexScope::Project {
        bail!("--project-dir can only be used with --scope project");
    }

    if args.transport == Transport::Stdio {
        println!(
            "Warning: stdio mode lets the MCP client own a Solo writer process and can conflict with a running daemon."
        );
        println!("No passphrase is written into the client config.");
        println!();
    }

    let os = OperatingSystem::current();
    let detection =
        detect_codex_config_path(args.scope, os, current_env_var, args.project_dir.as_deref());
    let inspection = detection
        .path
        .as_deref()
        .map(inspect_codex_config)
        .transpose()?;
    let apply = args.apply;

    println!("Client      : {}", Client::Codex.display_name());
    println!("Scope       : {}", args.scope.label());
    println!("Mode        : {}", if apply { "apply" } else { "dry-run" });
    println!("Transport   : {}", args.transport.label());
    println!("Library     : Community Memory Library");
    println!("Config path : {}", describe_detection_path(&detection));
    if let Some(inspection) = &inspection {
        println!("Config state: {}", inspection.status);
    }
    println!();

    let server_preview =
        preview_codex_config(None, args.transport, &args.url, args.data_dir.as_deref());
    if apply {
        let path = detection.path.as_deref().with_context(|| {
            detection
                .note
                .clone()
                .unwrap_or_else(|| "could not detect Codex config path".to_string())
        })?;
        let outcome =
            apply_codex_config(path, args.transport, &args.url, args.data_dir.as_deref())?;

        println!("Ensured this `[mcp_servers.solo]` entry:");
        print_toml_preview(&server_preview);
        println!();
        println!("Wrote config: {}", display_path(path));
        if let Some(backup_path) = outcome.backup_path {
            println!("Backup      : {}", display_path(&backup_path));
        }
        println!("No passphrase was written into the client config.");
    } else {
        let existing_toml = inspection
            .as_ref()
            .and_then(|i| i.raw.as_deref())
            .and_then(|raw| raw.parse::<DocumentMut>().ok());
        let preview = preview_codex_config(
            existing_toml,
            args.transport,
            &args.url,
            args.data_dir.as_deref(),
        );
        println!("Would ensure this `[mcp_servers.solo]` entry:");
        print_toml_preview(&server_preview);
        println!();
        println!("Resulting config preview:");
        print_toml_preview(&preview);
        println!();
        println!("No files were written.");
    }

    Ok(())
}

fn inspect_config(path: &Path) -> Result<ConfigInspection> {
    if !path.exists() {
        return Ok(ConfigInspection {
            json: None,
            status: "missing; parent directories may need to be created later".to_string(),
        });
    }

    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            return Ok(ConfigInspection {
                json: None,
                status: format!("unreadable: {err}"),
            });
        }
    };

    match serde_json::from_str::<Value>(&raw) {
        Ok(json) if json.is_object() => Ok(ConfigInspection {
            json: Some(json),
            status: "found; valid JSON object".to_string(),
        }),
        Ok(json) => Ok(ConfigInspection {
            json: Some(json),
            status: "found; valid JSON but root is not an object".to_string(),
        }),
        Err(err) => Ok(ConfigInspection {
            json: None,
            status: format!("found; malformed JSON: {err}"),
        }),
    }
}

fn inspect_codex_config(path: &Path) -> Result<TomlInspection> {
    if !path.exists() {
        return Ok(TomlInspection {
            raw: None,
            status: "missing; parent directories may need to be created later".to_string(),
        });
    }

    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            return Ok(TomlInspection {
                raw: None,
                status: format!("unreadable: {err}"),
            });
        }
    };

    match raw.parse::<DocumentMut>() {
        Ok(_) => Ok(TomlInspection {
            raw: Some(raw),
            status: "found; valid TOML".to_string(),
        }),
        Err(err) => Ok(TomlInspection {
            raw: None,
            status: format!("found; malformed TOML: {err}"),
        }),
    }
}

fn verify_client_config<F>(
    client: Client,
    os: OperatingSystem,
    env_var: &F,
    codex_scope: CodexScope,
    codex_project_dir: Option<&Path>,
) -> VerifyRow
where
    F: Fn(&str) -> Option<OsString>,
{
    if client == Client::Codex {
        return verify_codex_config(os, env_var, codex_scope, codex_project_dir);
    }

    let detection = detect_config_path(client, os, env_var);
    let Some(path) = detection.path else {
        return VerifyRow {
            client,
            path: None,
            status: VerifyStatus::Unknown,
            detail: detection
                .note
                .unwrap_or_else(|| "config path unavailable".to_string()),
        };
    };

    match read_existing_config_for_apply(&path) {
        Ok(Some(json)) => {
            let (status, detail) = verify_config_json(&json);
            VerifyRow {
                client,
                path: Some(path),
                status,
                detail,
            }
        }
        Ok(None) => VerifyRow {
            client,
            path: Some(path),
            status: VerifyStatus::Missing,
            detail: "config file does not exist".to_string(),
        },
        Err(err) => VerifyRow {
            client,
            path: Some(path),
            status: VerifyStatus::Invalid,
            detail: err.to_string(),
        },
    }
}

fn verify_codex_config<F>(
    os: OperatingSystem,
    env_var: &F,
    scope: CodexScope,
    project_dir: Option<&Path>,
) -> VerifyRow
where
    F: Fn(&str) -> Option<OsString>,
{
    let detection = detect_codex_config_path(scope, os, env_var, project_dir);
    let Some(path) = detection.path else {
        return VerifyRow {
            client: Client::Codex,
            path: None,
            status: VerifyStatus::Unknown,
            detail: detection
                .note
                .unwrap_or_else(|| "config path unavailable".to_string()),
        };
    };

    match read_existing_codex_config_for_apply(&path) {
        Ok(Some(doc)) => {
            let (status, detail) = verify_config_toml(&doc);
            VerifyRow {
                client: Client::Codex,
                path: Some(path),
                status,
                detail,
            }
        }
        Ok(None) => VerifyRow {
            client: Client::Codex,
            path: Some(path),
            status: VerifyStatus::Missing,
            detail: "config file does not exist".to_string(),
        },
        Err(err) => VerifyRow {
            client: Client::Codex,
            path: Some(path),
            status: VerifyStatus::Invalid,
            detail: err.to_string(),
        },
    }
}

fn verify_config_json(json: &Value) -> (VerifyStatus, String) {
    let Some(root) = json.as_object() else {
        return (
            VerifyStatus::Invalid,
            "config root is not a JSON object".to_string(),
        );
    };
    let Some(mcp_servers) = root.get("mcpServers") else {
        return (
            VerifyStatus::Missing,
            "`mcpServers.solo` is not configured".to_string(),
        );
    };
    let Some(mcp_servers) = mcp_servers.as_object() else {
        return (
            VerifyStatus::Invalid,
            "`mcpServers` is not a JSON object".to_string(),
        );
    };
    let Some(solo) = mcp_servers.get("solo") else {
        return (
            VerifyStatus::Missing,
            "`mcpServers.solo` is not configured".to_string(),
        );
    };
    let Some(solo_server) = solo.as_object() else {
        return (
            VerifyStatus::Invalid,
            "`mcpServers.solo` is not a JSON object".to_string(),
        );
    };

    if contains_passphrase_reference(solo) {
        return (
            VerifyStatus::Invalid,
            "`mcpServers.solo` contains SOLO_PASSPHRASE; remove passphrases from client config"
                .to_string(),
        );
    }
    if contains_bearer_authorization_reference(solo) {
        return (
            VerifyStatus::Invalid,
            "`mcpServers.solo` contains an Authorization bearer token; store tokens outside client config"
                .to_string(),
        );
    }
    if !matches!(solo_server.get("command"), Some(Value::String(command)) if !command.is_empty()) {
        return (
            VerifyStatus::Invalid,
            "`mcpServers.solo.command` must be a non-empty string".to_string(),
        );
    }
    if !matches!(solo_server.get("args"), Some(Value::Array(_))) {
        return (
            VerifyStatus::Invalid,
            "`mcpServers.solo.args` must be an array".to_string(),
        );
    }

    (
        VerifyStatus::Ok,
        "`mcpServers.solo` is configured".to_string(),
    )
}

fn verify_config_toml(doc: &DocumentMut) -> (VerifyStatus, String) {
    let root = doc.as_table();
    let Some(mcp_servers) = root.get("mcp_servers") else {
        return (
            VerifyStatus::Missing,
            "`mcp_servers.solo` is not configured".to_string(),
        );
    };
    let Some(mcp_servers) = mcp_servers.as_table() else {
        return (
            VerifyStatus::Invalid,
            "`mcp_servers` is not a TOML table".to_string(),
        );
    };
    let Some(solo) = mcp_servers.get("solo") else {
        return (
            VerifyStatus::Missing,
            "`mcp_servers.solo` is not configured".to_string(),
        );
    };
    let Some(solo_server) = solo.as_table() else {
        return (
            VerifyStatus::Invalid,
            "`mcp_servers.solo` is not a TOML table".to_string(),
        );
    };

    if contains_passphrase_reference_toml_item(solo) {
        return (
            VerifyStatus::Invalid,
            "`mcp_servers.solo` contains SOLO_PASSPHRASE; remove passphrases from client config"
                .to_string(),
        );
    }
    if contains_bearer_authorization_reference_toml_item(solo) {
        return (
            VerifyStatus::Invalid,
            "`mcp_servers.solo` contains an Authorization bearer token; store tokens outside client config"
                .to_string(),
        );
    }

    let has_url = matches!(
        solo_server.get("url").and_then(Item::as_str),
        Some(url) if !url.is_empty()
    );
    let has_command = matches!(
        solo_server.get("command").and_then(Item::as_str),
        Some(command) if !command.is_empty()
    );
    if !has_url && !has_command {
        return (
            VerifyStatus::Invalid,
            "`mcp_servers.solo.url` or `mcp_servers.solo.command` must be a non-empty string"
                .to_string(),
        );
    }
    if let Some(args) = solo_server.get("args") {
        if args.as_value().and_then(TomlValue::as_array).is_none() {
            return (
                VerifyStatus::Invalid,
                "`mcp_servers.solo.args` must be an array".to_string(),
            );
        }
    }

    (
        VerifyStatus::Ok,
        "`mcp_servers.solo` is configured".to_string(),
    )
}

fn server_entry_status(status: VerifyStatus) -> &'static str {
    match status {
        VerifyStatus::Ok => "installed",
        VerifyStatus::Missing => "missing",
        VerifyStatus::Invalid => "invalid",
        VerifyStatus::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpEndpoint {
    host: String,
    port: u16,
    request_target: String,
}

fn probe_mcp_endpoint(url: &str, timeout: Duration) -> McpEndpointProbe {
    match probe_mcp_endpoint_inner(url, timeout) {
        Ok(probe) => probe,
        Err(detail) => {
            let status = if detail.starts_with("only http://") {
                McpEndpointStatus::Unsupported
            } else {
                McpEndpointStatus::Unreachable
            };
            McpEndpointProbe {
                url: url.to_string(),
                status,
                detail,
                http_status: None,
                tools: None,
            }
        }
    }
}

fn probe_mcp_endpoint_inner(url: &str, timeout: Duration) -> Result<McpEndpointProbe, String> {
    let endpoint = parse_http_endpoint(url)?;
    let mut addrs = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {}:{} failed: {e}", endpoint.host, endpoint.port))?;
    let addr = addrs
        .next()
        .ok_or_else(|| format!("no address resolved for {}", endpoint.host))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connect failed: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set read timeout failed: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("set write timeout failed: {e}"))?;

    let body = json!({
        "jsonrpc": "2.0",
        "id": "solo-setup-client-doctor",
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {
                "name": "solo-setup-client-doctor",
                "version": solo_core::build_info::version_with_build_metadata(),
            }
        }
    })
    .to_string();
    let host_header = if endpoint.port == 80 {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    };
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        endpoint.request_target,
        host_header,
        body.len(),
        body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write probe failed: {e}"))?;

    let response = read_http_response_headers(&mut stream)?;
    let status = parse_http_status(&response)
        .ok_or_else(|| "MCP endpoint returned a malformed HTTP response".to_string())?;
    let (status_label, mut detail) = classify_mcp_http_status(status);
    let tools = if status_label == McpEndpointStatus::Reachable {
        match probe_mcp_tools_list(url, timeout) {
            Ok(tools) => {
                detail = format!("{detail}; {}", tools.detail());
                Some(tools)
            }
            Err(error) => {
                detail = format!("{detail}; tools/list check failed: {error}");
                None
            }
        }
    } else {
        None
    };
    Ok(McpEndpointProbe {
        url: url.to_string(),
        status: status_label,
        detail,
        http_status: Some(status),
        tools,
    })
}

fn probe_mcp_tools_list(url: &str, timeout: Duration) -> Result<McpToolsProbe, String> {
    let body = post_json_rpc(
        url,
        json!({
            "jsonrpc": "2.0",
            "id": "solo-setup-client-doctor-tools-list",
            "method": "tools/list",
        }),
        timeout,
    )?;
    let tools = body
        .pointer("/result/tools")
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("response missing /result/tools array: {body}"))?;
    let tool_names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    let missing_required_tools = DOCTOR_REQUIRED_MCP_TOOLS
        .iter()
        .filter(|tool| !tool_names.contains(**tool))
        .map(|tool| (*tool).to_string())
        .collect();
    Ok(McpToolsProbe {
        tool_count: tools.len(),
        missing_required_tools,
    })
}

fn post_json_rpc(url: &str, body: Value, timeout: Duration) -> Result<Value, String> {
    let endpoint = parse_http_endpoint(url)?;
    let mut addrs = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {}:{} failed: {e}", endpoint.host, endpoint.port))?;
    let addr = addrs
        .next()
        .ok_or_else(|| format!("no address resolved for {}", endpoint.host))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connect failed: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set read timeout failed: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("set write timeout failed: {e}"))?;

    let body = body.to_string();
    let host_header = if endpoint.port == 80 {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    };
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        endpoint.request_target,
        host_header,
        body.len(),
        body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write request failed: {e}"))?;
    let response = read_http_response(&mut stream)?;
    let status = parse_http_status(&response)
        .ok_or_else(|| "MCP endpoint returned a malformed HTTP response".to_string())?;
    if !(200..=299).contains(&status) {
        return Err(format!("endpoint returned HTTP {status}"));
    }
    let body = http_response_body(&response)?;
    serde_json::from_str::<Value>(body).map_err(|e| format!("decode response JSON: {e}"))
}

fn read_http_response(stream: &mut TcpStream) -> Result<String, String> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|e| format!("read response failed: {e}"))?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if response.len() >= 1024 * 1024 {
            return Err("MCP endpoint response was too large".to_string());
        }
    }
    if response.is_empty() {
        return Err("MCP endpoint returned an empty HTTP response".to_string());
    }
    Ok(String::from_utf8_lossy(&response).into_owned())
}

fn http_response_body(response: &str) -> Result<&str, String> {
    if let Some((_, body)) = response.split_once("\r\n\r\n") {
        return Ok(body);
    }
    if let Some((_, body)) = response.split_once("\n\n") {
        return Ok(body);
    }
    Err("MCP endpoint response did not include an HTTP body separator".to_string())
}

impl McpToolsProbe {
    fn detail(&self) -> String {
        if self.missing_required_tools.is_empty() {
            format!(
                "{} MCP tool(s); critical memory tools present",
                self.tool_count
            )
        } else {
            format!(
                "{} MCP tool(s); missing {}",
                self.tool_count,
                self.missing_required_tools.join(", ")
            )
        }
    }
}

fn read_http_response_headers(stream: &mut TcpStream) -> Result<String, String> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|e| format!("read probe response failed: {e}"))?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n")
            || response.windows(2).any(|window| window == b"\n\n")
        {
            break;
        }
        if response.len() >= 16 * 1024 {
            return Err("MCP endpoint response headers were too large".to_string());
        }
    }
    if response.is_empty() {
        return Err("MCP endpoint returned an empty HTTP response".to_string());
    }
    Ok(String::from_utf8_lossy(&response).into_owned())
}

fn parse_http_endpoint(url: &str) -> Result<HttpEndpoint, String> {
    let Some(rest) = url.strip_prefix("http://") else {
        return Err("only http:// MCP endpoints can be probed without TLS".to_string());
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() {
        return Err("MCP endpoint is missing a host".to_string());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            let port = port
                .parse::<u16>()
                .map_err(|e| format!("invalid MCP endpoint port `{port}`: {e}"))?;
            (host.to_string(), port)
        }
        _ => (authority.to_string(), 80),
    };
    let request_target = if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{path}")
    };
    Ok(HttpEndpoint {
        host,
        port,
        request_target,
    })
}

fn parse_http_status(response: &str) -> Option<u16> {
    response
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn classify_mcp_http_status(status: u16) -> (McpEndpointStatus, String) {
    match status {
        200..=299 => (
            McpEndpointStatus::Reachable,
            format!("endpoint responded with HTTP {status}"),
        ),
        401 | 403 => (
            McpEndpointStatus::AuthRequired,
            format!("endpoint is reachable but requires authorization (HTTP {status})"),
        ),
        404 | 405 => (
            McpEndpointStatus::WrongPath,
            format!(
                "daemon responded, but this does not look like the MCP endpoint (HTTP {status})"
            ),
        ),
        _ => (
            McpEndpointStatus::Reachable,
            format!("endpoint responded with HTTP {status}"),
        ),
    }
}

fn contains_passphrase_reference(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("SOLO_PASSPHRASE"),
        Value::Array(items) => items.iter().any(contains_passphrase_reference),
        Value::Object(map) => map.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("SOLO_PASSPHRASE") || contains_passphrase_reference(value)
        }),
        _ => false,
    }
}

fn contains_bearer_authorization_reference(value: &Value) -> bool {
    match value {
        Value::String(text) => is_authorization_bearer_header(text),
        Value::Array(items) => items.iter().any(contains_bearer_authorization_reference),
        Value::Object(map) => map.iter().any(|(key, value)| {
            (key.eq_ignore_ascii_case("authorization") && value_contains_bearer_scheme(value))
                || contains_bearer_authorization_reference(value)
        }),
        _ => false,
    }
}

fn value_contains_bearer_scheme(value: &Value) -> bool {
    match value {
        Value::String(text) => starts_with_bearer_scheme(text),
        Value::Array(items) => items.iter().any(value_contains_bearer_scheme),
        Value::Object(map) => map.values().any(value_contains_bearer_scheme),
        _ => false,
    }
}

fn contains_passphrase_reference_toml_item(item: &Item) -> bool {
    match item {
        Item::None => false,
        Item::Value(value) => contains_passphrase_reference_toml_value(value),
        Item::Table(table) => contains_passphrase_reference_toml_table(table),
        Item::ArrayOfTables(tables) => tables.iter().any(contains_passphrase_reference_toml_table),
    }
}

fn contains_bearer_authorization_reference_toml_item(item: &Item) -> bool {
    match item {
        Item::None => false,
        Item::Value(value) => contains_bearer_authorization_reference_toml_value(value),
        Item::Table(table) => contains_bearer_authorization_reference_toml_table(table),
        Item::ArrayOfTables(tables) => tables
            .iter()
            .any(contains_bearer_authorization_reference_toml_table),
    }
}

fn contains_passphrase_reference_toml_table(table: &Table) -> bool {
    table.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("SOLO_PASSPHRASE")
            || contains_passphrase_reference_toml_item(value)
    })
}

fn contains_bearer_authorization_reference_toml_table(table: &Table) -> bool {
    table.iter().any(|(key, value)| {
        (key.eq_ignore_ascii_case("authorization")
            && toml_value_contains_bearer_scheme(value.as_value()))
            || contains_bearer_authorization_reference_toml_item(value)
    })
}

fn contains_passphrase_reference_toml_value(value: &TomlValue) -> bool {
    match value {
        TomlValue::String(text) => text.value().contains("SOLO_PASSPHRASE"),
        TomlValue::Array(items) => items.iter().any(contains_passphrase_reference_toml_value),
        TomlValue::InlineTable(table) => table.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("SOLO_PASSPHRASE")
                || contains_passphrase_reference_toml_value(value)
        }),
        _ => false,
    }
}

fn contains_bearer_authorization_reference_toml_value(value: &TomlValue) -> bool {
    match value {
        TomlValue::String(text) => is_authorization_bearer_header(text.value()),
        TomlValue::Array(items) => items
            .iter()
            .any(contains_bearer_authorization_reference_toml_value),
        TomlValue::InlineTable(table) => table.iter().any(|(key, value)| {
            (key.eq_ignore_ascii_case("authorization")
                && starts_with_bearer_scheme_for_toml_value(value))
                || contains_bearer_authorization_reference_toml_value(value)
        }),
        _ => false,
    }
}

fn toml_value_contains_bearer_scheme(value: Option<&TomlValue>) -> bool {
    value.is_some_and(starts_with_bearer_scheme_for_toml_value)
}

fn starts_with_bearer_scheme_for_toml_value(value: &TomlValue) -> bool {
    match value {
        TomlValue::String(text) => starts_with_bearer_scheme(text.value()),
        TomlValue::Array(items) => items.iter().any(starts_with_bearer_scheme_for_toml_value),
        TomlValue::InlineTable(table) => table
            .iter()
            .any(|(_, value)| starts_with_bearer_scheme_for_toml_value(value)),
        _ => false,
    }
}

fn is_authorization_bearer_header(text: &str) -> bool {
    let Some((name, value)) = text.split_once(':') else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("authorization") && starts_with_bearer_scheme(value)
}

fn starts_with_bearer_scheme(text: &str) -> bool {
    let text = text.trim_start();
    text.as_bytes()
        .get(.."bearer ".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"bearer "))
}

fn apply_config(
    path: &Path,
    transport: Transport,
    url: &str,
    data_dir: Option<&Path>,
) -> Result<ApplyOutcome> {
    let parent = config_parent(path)?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create config parent {}", display_path(parent)))?;

    let existing = read_existing_config_for_apply(path)?;
    let updated = preview_config(existing, transport, url, data_dir);
    let backup_path = if path.exists() {
        Some(create_backup(path)?)
    } else {
        None
    };

    write_json_atomic(path, &updated)?;

    Ok(ApplyOutcome { backup_path })
}

fn apply_codex_config(
    path: &Path,
    transport: Transport,
    url: &str,
    data_dir: Option<&Path>,
) -> Result<ApplyOutcome> {
    let parent = config_parent(path)?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create config parent {}", display_path(parent)))?;

    let existing = read_existing_codex_config_for_apply(path)?;
    let updated = preview_codex_config(existing, transport, url, data_dir);
    let backup_path = if path.exists() {
        Some(create_backup(path)?)
    } else {
        None
    };

    write_toml_atomic(path, &updated)?;

    Ok(ApplyOutcome { backup_path })
}

fn read_existing_config_for_apply(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("read existing config {}", display_path(path)))?;
    let json = serde_json::from_str::<Value>(&raw).with_context(|| {
        format!(
            "parse existing config {}; refusing to apply",
            display_path(path)
        )
    })?;
    ensure_mergeable_config(&json)?;
    Ok(Some(json))
}

fn read_existing_codex_config_for_apply(path: &Path) -> Result<Option<DocumentMut>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("read existing config {}", display_path(path)))?;
    let doc = raw.parse::<DocumentMut>().with_context(|| {
        format!(
            "parse existing config {}; refusing to apply",
            display_path(path)
        )
    })?;
    ensure_mergeable_codex_config(&doc)?;
    Ok(Some(doc))
}

fn ensure_mergeable_config(json: &Value) -> Result<()> {
    if !json.is_object() {
        bail!("existing config root is not a JSON object; refusing to apply");
    }
    if let Some(mcp_servers) = json.get("mcpServers") {
        if !mcp_servers.is_object() {
            bail!("existing config `mcpServers` is not a JSON object; refusing to apply");
        }
    }
    Ok(())
}

fn ensure_mergeable_codex_config(doc: &DocumentMut) -> Result<()> {
    if let Some(mcp_servers) = doc.as_table().get("mcp_servers") {
        if !mcp_servers.is_table() {
            bail!("existing config `mcp_servers` is not a TOML table; refusing to apply");
        }
    }
    Ok(())
}

fn create_backup(path: &Path) -> Result<PathBuf> {
    let backup_path = unique_backup_path(path)?;
    let mut source =
        fs::File::open(path).with_context(|| format!("open {} for backup", display_path(path)))?;
    let mut backup = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup_path)
        .with_context(|| format!("create backup {}", display_path(&backup_path)))?;
    io::copy(&mut source, &mut backup)
        .with_context(|| format!("copy backup {}", display_path(&backup_path)))?;
    backup
        .sync_all()
        .with_context(|| format!("sync backup {}", display_path(&backup_path)))?;
    Ok(backup_path)
}

fn unique_backup_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .context("config path has no file name")?
        .to_string_lossy();
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ");

    for suffix in 0..1000 {
        let backup_name = if suffix == 0 {
            format!("{file_name}.bak.{timestamp}")
        } else {
            format!("{file_name}.bak.{timestamp}.{suffix}")
        };
        let candidate = path.with_file_name(backup_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "could not choose a unique backup path for {}",
        display_path(path)
    )
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serialize setup-client config")?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

fn write_toml_atomic(path: &Path, doc: &DocumentMut) -> Result<()> {
    let mut bytes = doc.to_string().into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    write_bytes_atomic(path, &bytes)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = config_parent(path)?;
    let file_name = path.file_name().context("config path has no file name")?;
    let temp_path = unique_temp_path(parent, file_name)?;
    let mut temp_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| format!("create temp config {}", display_path(&temp_path)))?;
    temp_file
        .write_all(bytes)
        .with_context(|| format!("write temp config {}", display_path(&temp_path)))?;
    temp_file
        .sync_all()
        .with_context(|| format!("sync temp config {}", display_path(&temp_path)))?;
    drop(temp_file);

    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(first_err) if path.exists() => {
            fs::remove_file(path)
                .with_context(|| format!("remove existing config {}", display_path(path)))?;
            if let Err(second_err) = fs::rename(&temp_path, path) {
                let _ = fs::remove_file(&temp_path);
                bail!(
                    "replace {} via temp file failed after existing file removal: {second_err}; initial rename error: {first_err}",
                    display_path(path)
                );
            }
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            Err(err).with_context(|| {
                format!(
                    "rename temp config {} to {}",
                    display_path(&temp_path),
                    display_path(path)
                )
            })
        }
    }
}

fn unique_temp_path(parent: &Path, file_name: &std::ffi::OsStr) -> Result<PathBuf> {
    let file_name = file_name.to_string_lossy();
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");

    for suffix in 0..1000 {
        let temp_name = format!(
            ".{file_name}.solo-setup-client.{}.{}.{}.tmp",
            process::id(),
            timestamp,
            suffix
        );
        let candidate = parent.join(temp_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "could not choose a unique temp path in {}",
        display_path(parent)
    )
}

fn config_parent(path: &Path) -> Result<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .with_context(|| format!("config path {} has no parent directory", display_path(path)))
}

fn current_env_var(key: &str) -> Option<OsString> {
    env::var_os(key)
}

fn preview_config(
    existing: Option<Value>,
    transport: Transport,
    url: &str,
    data_dir: Option<&Path>,
) -> Value {
    let mut root = match existing {
        Some(Value::Object(map)) => Value::Object(map),
        _ => json!({}),
    };

    let object = root
        .as_object_mut()
        .expect("root is always an object after normalization");
    let mcp_servers = object.entry("mcpServers").or_insert_with(|| json!({}));
    if !mcp_servers.is_object() {
        *mcp_servers = json!({});
    }
    mcp_servers
        .as_object_mut()
        .expect("mcpServers normalized to object")
        .insert("solo".to_string(), server_entry(transport, url, data_dir));

    root
}

fn preview_codex_config(
    existing: Option<DocumentMut>,
    transport: Transport,
    url: &str,
    data_dir: Option<&Path>,
) -> DocumentMut {
    let mut doc = existing.unwrap_or_default();
    let root = doc.as_table_mut();
    let mcp_servers = root
        .entry("mcp_servers")
        .or_insert_with(new_implicit_table_item);
    if !mcp_servers.is_table() {
        *mcp_servers = new_implicit_table_item();
    }
    let mcp_servers = mcp_servers
        .as_table_mut()
        .expect("mcp_servers normalized to table");
    *mcp_servers
        .entry("solo")
        .or_insert_with(|| Item::Table(Table::new())) =
        Item::Table(codex_server_table(transport, url, data_dir));

    doc
}

fn new_implicit_table_item() -> Item {
    let mut table = Table::new();
    table.set_implicit(true);
    Item::Table(table)
}

fn server_entry(transport: Transport, url: &str, data_dir: Option<&Path>) -> Value {
    match transport {
        Transport::Http => {
            let args = vec![
                "mcp-remote".to_string(),
                url.to_string(),
                "--transport".to_string(),
                "http-only".to_string(),
            ];
            json!({
                "command": "npx",
                "args": args
            })
        }
        Transport::Stdio => {
            let mut args = vec!["mcp-stdio".to_string()];
            if let Some(data_dir) = data_dir {
                args.push("--data-dir".to_string());
                args.push(display_path(data_dir));
            }
            json!({
                "command": "solo",
                "args": args
            })
        }
    }
}

fn codex_server_table(transport: Transport, url: &str, data_dir: Option<&Path>) -> Table {
    let mut table = Table::new();
    match transport {
        Transport::Http => {
            *table.entry("url").or_insert(Item::None) = toml_value(url);
        }
        Transport::Stdio => {
            *table.entry("command").or_insert(Item::None) = toml_value("solo");
            let mut args = Array::new();
            args.push("mcp-stdio");
            if let Some(data_dir) = data_dir {
                args.push("--data-dir");
                args.push(display_path(data_dir));
            }
            *table.entry("args").or_insert(Item::None) = Item::Value(TomlValue::Array(args));
        }
    }
    table
}

pub(crate) fn detect_config_path<F>(
    client: Client,
    os: OperatingSystem,
    env_var: F,
) -> PathDetection
where
    F: Fn(&str) -> Option<OsString>,
{
    match (client, os) {
        (Client::ClaudeDesktop, OperatingSystem::Windows) => env_var("APPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("Claude").join("claude_desktop_config.json"))
            .map(found_path)
            .unwrap_or_else(|| missing_env("APPDATA")),
        (Client::ClaudeDesktop, OperatingSystem::Macos) => home_dir(os, &env_var)
            .map(|p| {
                p.join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            })
            .map(found_path)
            .unwrap_or_else(|| missing_env("HOME")),
        (Client::ClaudeDesktop, OperatingSystem::Linux) => home_dir(os, &env_var)
            .map(|p| {
                p.join(".config")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            })
            .map(found_path)
            .unwrap_or_else(|| missing_env("HOME")),
        (Client::Cursor, _) => home_dir(os, &env_var)
            .map(|p| p.join(".cursor").join("mcp.json"))
            .map(found_path)
            .unwrap_or_else(|| {
                if os == OperatingSystem::Windows {
                    missing_env("USERPROFILE")
                } else {
                    missing_env("HOME")
                }
            }),
        (Client::Codex, _) => home_dir(os, &env_var)
            .map(|p| p.join(".codex").join("config.toml"))
            .map(found_path)
            .unwrap_or_else(|| {
                if os == OperatingSystem::Windows {
                    missing_env("USERPROFILE")
                } else {
                    missing_env("HOME")
                }
            }),
    }
}

fn detect_codex_config_path<F>(
    scope: CodexScope,
    os: OperatingSystem,
    env_var: F,
    project_dir: Option<&Path>,
) -> PathDetection
where
    F: Fn(&str) -> Option<OsString>,
{
    match scope {
        CodexScope::User => detect_config_path(Client::Codex, os, env_var),
        CodexScope::Project => {
            let project_dir = match project_dir {
                Some(path) => path.to_path_buf(),
                None => match env::current_dir() {
                    Ok(path) => path,
                    Err(err) => {
                        return PathDetection {
                            path: None,
                            note: Some(format!("unavailable; current directory: {err}")),
                        };
                    }
                },
            };
            found_path(project_dir.join(".codex").join("config.toml"))
        }
    }
}

fn home_dir<F>(os: OperatingSystem, env_var: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    if os == OperatingSystem::Windows {
        if let Some(profile) = env_var("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        let drive = env_var("HOMEDRIVE")?;
        let path = env_var("HOMEPATH")?;
        let mut joined = drive;
        joined.push(path);
        Some(PathBuf::from(joined))
    } else {
        env_var("HOME").map(PathBuf::from)
    }
}

fn found_path(path: PathBuf) -> PathDetection {
    PathDetection {
        path: Some(path),
        note: None,
    }
}

fn missing_env(var: &str) -> PathDetection {
    PathDetection {
        path: None,
        note: Some(format!("unavailable; missing {var}")),
    }
}

fn path_status(detection: &PathDetection) -> &'static str {
    match &detection.path {
        Some(path) if path.exists() => "found",
        Some(_) => "missing",
        None => "unknown",
    }
}

fn describe_detection_path(detection: &PathDetection) -> String {
    detection
        .path
        .as_deref()
        .map(display_path)
        .unwrap_or_else(|| {
            detection
                .note
                .clone()
                .unwrap_or_else(|| "unavailable".to_string())
        })
}

fn print_toml_preview(doc: &DocumentMut) {
    print!("{doc}");
    if !doc.to_string().ends_with('\n') {
        println!();
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

impl Transport {
    fn label(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Stdio => "stdio",
        }
    }
}

impl CodexScope {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

impl VerifyStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::Unknown => "unknown",
        }
    }
}

impl McpEndpointStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Reachable => "reachable",
            Self::AuthRequired => "auth_required",
            Self::WrongPath => "wrong_path",
            Self::Unsupported => "unsupported",
            Self::Unreachable => "unreachable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::tempdir;

    fn env_lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: BTreeMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn detects_claude_desktop_windows_path_from_appdata() {
        let detection = detect_config_path(
            Client::ClaudeDesktop,
            OperatingSystem::Windows,
            env_lookup(&[("APPDATA", r"C:\Users\Ada\AppData\Roaming")]),
        );

        let path = detection.path.expect("path");
        assert!(path.ends_with(Path::new("Claude").join("claude_desktop_config.json")));
        assert!(display_path(&path).contains("AppData"));
    }

    #[test]
    fn detects_claude_desktop_macos_path_from_home() {
        let detection = detect_config_path(
            Client::ClaudeDesktop,
            OperatingSystem::Macos,
            env_lookup(&[("HOME", "/Users/ada")]),
        );

        assert_eq!(
            detection.path,
            Some(
                PathBuf::from("/Users/ada")
                    .join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            )
        );
    }

    #[test]
    fn detects_claude_desktop_linux_path_from_home() {
        let detection = detect_config_path(
            Client::ClaudeDesktop,
            OperatingSystem::Linux,
            env_lookup(&[("HOME", "/home/ada")]),
        );

        assert_eq!(
            detection.path,
            Some(
                PathBuf::from("/home/ada")
                    .join(".config")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            )
        );
    }

    #[test]
    fn detects_cursor_windows_path_from_userprofile() {
        let detection = detect_config_path(
            Client::Cursor,
            OperatingSystem::Windows,
            env_lookup(&[("USERPROFILE", r"C:\Users\Ada")]),
        );

        let path = detection.path.expect("path");
        assert!(path.ends_with(Path::new(".cursor").join("mcp.json")));
        assert!(display_path(&path).contains("Ada"));
    }

    #[test]
    fn detects_codex_user_path_from_home() {
        let detection = detect_config_path(
            Client::Codex,
            OperatingSystem::Linux,
            env_lookup(&[("HOME", "/home/ada")]),
        );

        assert_eq!(
            detection.path,
            Some(
                PathBuf::from("/home/ada")
                    .join(".codex")
                    .join("config.toml")
            )
        );
    }

    #[test]
    fn detects_codex_project_scope_path_from_project_dir() {
        let project = PathBuf::from("/work/project");
        let detection = detect_codex_config_path(
            CodexScope::Project,
            OperatingSystem::Linux,
            env_lookup(&[]),
            Some(&project),
        );

        assert_eq!(
            detection.path,
            Some(project.join(".codex").join("config.toml"))
        );
    }

    #[test]
    fn reports_missing_environment_for_cursor() {
        let detection = detect_config_path(Client::Cursor, OperatingSystem::Linux, env_lookup(&[]));

        assert_eq!(detection.path, None);
        assert_eq!(
            detection.note,
            Some("unavailable; missing HOME".to_string())
        );
    }

    #[test]
    fn reports_missing_environment_for_codex_user_scope() {
        let detection = detect_config_path(Client::Codex, OperatingSystem::Linux, env_lookup(&[]));

        assert_eq!(detection.path, None);
        assert_eq!(
            detection.note,
            Some("unavailable; missing HOME".to_string())
        );
    }

    #[test]
    fn preview_preserves_existing_servers_and_adds_solo() {
        let existing = json!({
            "mcpServers": {
                "other": {
                    "command": "node",
                    "args": ["server.js"]
                }
            }
        });

        let preview = preview_config(Some(existing), Transport::Http, DEFAULT_MCP_URL, None);

        assert_eq!(
            preview["mcpServers"]["other"]["command"],
            Value::String("node".to_string())
        );
        assert_eq!(
            preview["mcpServers"]["solo"]["args"][0],
            Value::String("mcp-remote".to_string())
        );
    }

    #[test]
    fn stdio_preview_includes_data_dir_without_env_secret() {
        let preview = preview_config(
            None,
            Transport::Stdio,
            DEFAULT_MCP_URL,
            Some(Path::new("/tmp/solo-data")),
        );

        assert_eq!(
            preview["mcpServers"]["solo"]["command"],
            Value::String("solo".to_string())
        );
        assert_eq!(
            preview["mcpServers"]["solo"]["args"][2],
            Value::String("/tmp/solo-data".to_string())
        );
        assert!(preview["mcpServers"]["solo"].get("env").is_none());
    }

    #[test]
    fn generated_client_config_has_no_library_selector() {
        let stdio = preview_config(
            None,
            Transport::Stdio,
            DEFAULT_MCP_URL,
            Some(Path::new("/tmp/solo-data")),
        );
        let http = preview_config(None, Transport::Http, DEFAULT_MCP_URL, None);

        assert!(!stdio.to_string().contains("--tenant"));
        assert!(!http.to_string().contains("X-Solo-Tenant"));
    }

    #[test]
    fn codex_preview_preserves_existing_config_and_adds_http_server() -> Result<()> {
        let existing = r#"
model = "gpt-5.1-codex"

[mcp_servers.other]
url = "https://example.test/mcp"
"#
        .parse::<DocumentMut>()?;

        let preview = preview_codex_config(Some(existing), Transport::Http, DEFAULT_MCP_URL, None);

        assert_eq!(preview["model"].as_str(), Some("gpt-5.1-codex"));
        assert_eq!(
            preview["mcp_servers"]["other"]["url"].as_str(),
            Some("https://example.test/mcp")
        );
        assert_eq!(
            preview["mcp_servers"]["solo"]["url"].as_str(),
            Some(DEFAULT_MCP_URL)
        );
        assert!(preview["mcp_servers"]["solo"].get("http_headers").is_none());
        Ok(())
    }

    #[test]
    fn codex_stdio_preview_includes_data_dir_without_env_secret() {
        let preview = preview_codex_config(
            None,
            Transport::Stdio,
            DEFAULT_MCP_URL,
            Some(Path::new("/tmp/solo-data")),
        );

        assert_eq!(
            preview["mcp_servers"]["solo"]["command"].as_str(),
            Some("solo")
        );
        assert_eq!(
            preview["mcp_servers"]["solo"]["args"]
                .as_array()
                .and_then(|args| args.get(2))
                .and_then(TomlValue::as_str),
            Some("/tmp/solo-data")
        );
        assert!(preview["mcp_servers"]["solo"].get("env").is_none());
    }

    #[test]
    fn generated_codex_config_has_no_library_selector() {
        let http = preview_codex_config(None, Transport::Http, DEFAULT_MCP_URL, None);
        let stdio = preview_codex_config(
            None,
            Transport::Stdio,
            DEFAULT_MCP_URL,
            Some(Path::new("/tmp/solo-data")),
        );

        assert!(!http.to_string().contains("X-Solo-Tenant"));
        assert!(!stdio.to_string().contains("--tenant"));
    }

    #[test]
    fn doctor_maps_verify_status_to_installed_server_state() {
        assert_eq!(server_entry_status(VerifyStatus::Ok), "installed");
        assert_eq!(server_entry_status(VerifyStatus::Missing), "missing");
        assert_eq!(server_entry_status(VerifyStatus::Invalid), "invalid");
        assert_eq!(server_entry_status(VerifyStatus::Unknown), "unknown");
    }

    #[test]
    fn doctor_mcp_probe_reports_reachable_auth_required_endpoint() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept probe");
            let mut buf = [0_u8; 512];
            let _ = std::io::Read::read(&mut socket, &mut buf);
            std::io::Write::write_all(
                &mut socket,
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("write response");
        });

        let probe = probe_mcp_endpoint(
            &format!("http://{}:{}/mcp", addr.ip(), addr.port()),
            Duration::from_secs(2),
        );
        handle.join().expect("probe server thread");

        assert_eq!(probe.status, McpEndpointStatus::AuthRequired);
        assert_eq!(probe.http_status, Some(401));
        assert!(probe.detail.contains("requires authorization"));
        Ok(())
    }

    #[test]
    fn doctor_mcp_probe_accepts_streaming_headers_without_waiting_for_close() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let (close_tx, close_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept probe");
            let mut buf = [0_u8; 512];
            let _ = std::io::Read::read(&mut socket, &mut buf);
            std::io::Write::write_all(
                &mut socket,
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
            )
            .expect("write response headers");
            std::io::Write::flush(&mut socket).expect("flush response headers");
            let _ = close_rx.recv_timeout(Duration::from_secs(2));
        });

        let probe = probe_mcp_endpoint(
            &format!("http://{}:{}/mcp", addr.ip(), addr.port()),
            Duration::from_millis(200),
        );
        let _ = close_tx.send(());
        handle.join().expect("probe server thread");

        assert_eq!(probe.status, McpEndpointStatus::Reachable);
        assert_eq!(probe.http_status, Some(200));
        Ok(())
    }

    #[test]
    fn doctor_mcp_probe_reports_tools_list_shape() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = std::thread::spawn(move || {
            for request_idx in 0..2 {
                let (mut socket, _) = listener.accept().expect("accept probe");
                let mut buf = [0_u8; 1024];
                let _ = std::io::Read::read(&mut socket, &mut buf);
                if request_idx == 0 {
                    std::io::Write::write_all(
                        &mut socket,
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .expect("write initialize response");
                } else {
                    let body = json!({
                        "jsonrpc": "2.0",
                        "id": "solo-setup-client-doctor-tools-list",
                        "result": {
                            "tools": [
                                { "name": "memory_context" },
                                { "name": "memory_inbox" },
                                { "name": "memory_review" }
                            ]
                        }
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    std::io::Write::write_all(&mut socket, response.as_bytes())
                        .expect("write tools/list response");
                }
            }
        });

        let probe = probe_mcp_endpoint(
            &format!("http://{}:{}/mcp", addr.ip(), addr.port()),
            Duration::from_secs(2),
        );
        handle.join().expect("probe server thread");

        assert_eq!(probe.status, McpEndpointStatus::Reachable);
        let tools = probe.tools.expect("tools probe");
        assert_eq!(tools.tool_count, 3);
        assert!(tools.missing_required_tools.is_empty());
        assert!(probe.detail.contains("critical memory tools present"));
        Ok(())
    }

    #[test]
    fn doctor_mcp_probe_never_sends_a_library_selector() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            for request_idx in 0..2 {
                let (mut socket, _) = listener.accept().expect("accept probe");
                let mut buf = [0_u8; 2048];
                let read = std::io::Read::read(&mut socket, &mut buf).expect("read request");
                request_tx
                    .send(String::from_utf8_lossy(&buf[..read]).into_owned())
                    .expect("send captured request");
                if request_idx == 0 {
                    std::io::Write::write_all(
                        &mut socket,
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .expect("write initialize response");
                } else {
                    let body = json!({
                        "jsonrpc": "2.0",
                        "id": "solo-setup-client-doctor-tools-list",
                        "result": {
                            "tools": [
                                { "name": "memory_context" },
                                { "name": "memory_inbox" },
                                { "name": "memory_review" }
                            ]
                        }
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    std::io::Write::write_all(&mut socket, response.as_bytes())
                        .expect("write tools/list response");
                }
            }
        });

        let probe = probe_mcp_endpoint(
            &format!("http://{}:{}/mcp", addr.ip(), addr.port()),
            Duration::from_secs(2),
        );
        let first_request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first request");
        let second_request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second request");
        handle.join().expect("probe server thread");

        assert_eq!(probe.status, McpEndpointStatus::Reachable);
        assert!(!first_request.contains("X-Solo-Tenant"));
        assert!(first_request.contains("\"method\":\"initialize\""));
        assert!(!second_request.contains("X-Solo-Tenant"));
        assert!(second_request.contains("\"method\":\"tools/list\""));
        assert!(
            probe
                .tools
                .expect("tools probe")
                .missing_required_tools
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn doctor_mcp_probe_reports_unsupported_https_without_connecting() {
        let probe = probe_mcp_endpoint("https://127.0.0.1:17821/mcp", Duration::from_millis(1));

        assert_eq!(probe.status, McpEndpointStatus::Unsupported);
        assert!(probe.detail.contains("only http://"));
    }

    #[test]
    fn doctor_http_endpoint_parser_keeps_path_and_port() {
        let endpoint = parse_http_endpoint("http://127.0.0.1:17821/mcp?x=1").unwrap();

        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 17821);
        assert_eq!(endpoint.request_target, "/mcp?x=1");
    }

    #[test]
    fn apply_creates_parent_dirs_without_backup_when_config_is_missing() -> Result<()> {
        let temp = tempdir()?;
        let path = temp
            .path()
            .join("Claude")
            .join("claude_desktop_config.json");

        let outcome = apply_config(&path, Transport::Http, DEFAULT_MCP_URL, None)?;

        assert!(outcome.backup_path.is_none());
        let written = serde_json::from_str::<Value>(&fs::read_to_string(path)?)?;
        assert_eq!(
            written["mcpServers"]["solo"]["command"],
            Value::String("npx".to_string())
        );
        assert!(written["mcpServers"]["solo"].get("env").is_none());
        Ok(())
    }

    #[test]
    fn apply_backs_up_and_preserves_existing_keys_and_servers() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join(".cursor").join("mcp.json");
        fs::create_dir_all(path.parent().expect("parent"))?;
        let existing = json!({
            "window": {
                "theme": "dark"
            },
            "mcpServers": {
                "other": {
                    "command": "node",
                    "args": ["server.js"]
                },
                "solo": {
                    "command": "old-solo",
                    "args": []
                }
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&existing)?)?;

        let outcome = apply_config(&path, Transport::Http, DEFAULT_MCP_URL, None)?;

        let backup_path = outcome.backup_path.expect("backup");
        assert!(backup_path.exists());
        let backup = serde_json::from_str::<Value>(&fs::read_to_string(backup_path)?)?;
        assert_eq!(
            backup["mcpServers"]["solo"]["command"],
            Value::String("old-solo".to_string())
        );

        let written = serde_json::from_str::<Value>(&fs::read_to_string(path)?)?;
        assert_eq!(
            written["window"]["theme"],
            Value::String("dark".to_string())
        );
        assert_eq!(
            written["mcpServers"]["other"]["command"],
            Value::String("node".to_string())
        );
        assert_eq!(
            written["mcpServers"]["solo"]["args"][0],
            Value::String("mcp-remote".to_string())
        );
        assert!(written["mcpServers"]["solo"].get("env").is_none());
        Ok(())
    }

    #[test]
    fn codex_apply_backs_up_and_preserves_existing_toml() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join(".codex").join("config.toml");
        fs::create_dir_all(path.parent().expect("parent"))?;
        fs::write(
            &path,
            r#"model = "gpt-5.1-codex"

[mcp_servers.other]
url = "https://example.test/mcp"

[mcp_servers.solo]
url = "http://old.example/mcp"
"#,
        )?;

        let outcome = apply_codex_config(&path, Transport::Http, DEFAULT_MCP_URL, None)?;

        let backup_path = outcome.backup_path.expect("backup");
        assert!(backup_path.exists());
        let backup = fs::read_to_string(backup_path)?;
        assert!(backup.contains("http://old.example/mcp"));

        let written = fs::read_to_string(path)?;
        let doc = written.parse::<DocumentMut>()?;
        assert_eq!(doc["model"].as_str(), Some("gpt-5.1-codex"));
        assert_eq!(
            doc["mcp_servers"]["other"]["url"].as_str(),
            Some("https://example.test/mcp")
        );
        assert_eq!(
            doc["mcp_servers"]["solo"]["url"].as_str(),
            Some(DEFAULT_MCP_URL)
        );
        assert!(doc["mcp_servers"]["solo"].get("http_headers").is_none());
        Ok(())
    }

    #[test]
    fn apply_refuses_unmergeable_existing_json() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("mcp.json");
        fs::write(&path, "[]")?;

        let err = apply_config(&path, Transport::Http, DEFAULT_MCP_URL, None)
            .expect_err("non-object config should be refused");

        assert!(
            err.to_string().contains("root is not a JSON object"),
            "{err:?}"
        );
        assert_eq!(fs::read_to_string(path)?, "[]");
        Ok(())
    }

    #[test]
    fn codex_apply_refuses_unmergeable_existing_toml() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("config.toml");
        fs::write(&path, "mcp_servers = \"not-a-table\"\n")?;

        let err = apply_codex_config(&path, Transport::Http, DEFAULT_MCP_URL, None)
            .expect_err("non-table mcp_servers should be refused");

        assert!(err.to_string().contains("mcp_servers"), "{err:?}");
        assert_eq!(fs::read_to_string(path)?, "mcp_servers = \"not-a-table\"\n");
        Ok(())
    }

    #[test]
    fn verify_rejects_passphrase_reference_in_solo_entry() {
        let config = json!({
            "mcpServers": {
                "solo": {
                    "command": "solo",
                    "args": ["mcp-stdio"],
                    "env": {
                        "SOLO_PASSPHRASE": "secret"
                    }
                }
            }
        });

        let (status, detail) = verify_config_json(&config);

        assert_eq!(status, VerifyStatus::Invalid);
        assert!(detail.contains("SOLO_PASSPHRASE"));
    }

    #[test]
    fn verify_rejects_bearer_token_reference_in_solo_entry() {
        let config = json!({
            "mcpServers": {
                "solo": {
                    "command": "npx",
                    "args": [
                        "mcp-remote",
                        "http://127.0.0.1:17821/mcp",
                        "--header",
                        "Authorization: Bearer secret-token"
                    ]
                }
            }
        });

        let (status, detail) = verify_config_json(&config);

        assert_eq!(status, VerifyStatus::Invalid);
        assert!(detail.contains("Authorization bearer token"));
        assert!(!detail.contains("secret-token"));
    }

    #[test]
    fn codex_verify_rejects_passphrase_reference_in_solo_entry() -> Result<()> {
        let config = r#"
[mcp_servers.solo]
command = "solo"
args = ["mcp-stdio"]

[mcp_servers.solo.env]
SOLO_PASSPHRASE = "secret"
"#
        .parse::<DocumentMut>()?;

        let (status, detail) = verify_config_toml(&config);

        assert_eq!(status, VerifyStatus::Invalid);
        assert!(detail.contains("SOLO_PASSPHRASE"));
        Ok(())
    }

    #[test]
    fn codex_verify_rejects_bearer_token_reference_in_solo_entry() -> Result<()> {
        let config = r#"
[mcp_servers.solo]
url = "http://127.0.0.1:17821/mcp"

[mcp_servers.solo.http_headers]
Authorization = "Bearer secret-token"
"#
        .parse::<DocumentMut>()?;

        let (status, detail) = verify_config_toml(&config);

        assert_eq!(status, VerifyStatus::Invalid);
        assert!(detail.contains("Authorization bearer token"));
        assert!(!detail.contains("secret-token"));
        Ok(())
    }
}
