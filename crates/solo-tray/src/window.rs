// SPDX-License-Identifier: Apache-2.0

//! eframe app: log-viewer window + tray-event dispatch.
//!
//! The window is visible at launch so the user can see startup logs.
//! Closing it via the X button minimises instead of quitting.
//! Quit-from-tray is the canonical shutdown.

use crate::daemon::{DaemonHandle, SupervisorState};
use crate::logs::{Level, RingBuffer};
use crate::notify::Notifier;
use crate::settings::{
    ConnectedToolLastStatus, MemoryReviewStatus, Settings, Theme, WorkspaceAccessScope,
};
use crate::status::{DaemonHealth, StatusState};
use crate::{autostart, tray};
use eframe::{App, CreationContext, Frame};
use egui::{Context, Key, RichText, ScrollArea, TextStyle, ViewportCommand};
use solo_core::{ProjectMemoryDescriptor, ProjectPolicyClient, render_project_policy};
use solo_storage::{InitParams, SoloConfig, probe_embedder_config_from_env};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System, UpdateKind};
use tokio::sync::Mutex;
use zeroize::{Zeroize, Zeroizing};

const POLICY_CLAUDE_DESKTOP: &str = include_str!("../../../docs/policies/claude-desktop.md");
const POLICY_CODEX: &str = include_str!("../../../docs/policies/codex.md");
const POLICY_CURSOR: &str = include_str!("../../../docs/policies/cursor.md");
const POLICY_GENERIC_MCP_AGENT: &str = include_str!("../../../docs/policies/generic-mcp-agent.md");
const MEMORY_INBOX_RECENT_LIMIT: usize = 100;

/// Upper bound on GTK iterations serviced per eframe frame on Linux.
/// The tray's GTK queue is normally near-empty, so this is a guard against
/// a pathological burst monopolising the frame, not a tuning knob. At the
/// 4 Hz repaint cadence this still drains 256 events/second.
#[cfg(target_os = "linux")]
const MAX_GTK_ITERATIONS_PER_FRAME: usize = 64;
const COMMUNITY_LIBRARY_KEY: &str = "community";

pub struct AppState {
    pub log_buffer: Arc<Mutex<RingBuffer>>,
    pub daemon_handle: Arc<Mutex<DaemonHandle>>,
    pub status_state: Arc<Mutex<StatusState>>,
    pub notifier: Arc<Mutex<Notifier>>,
    pub settings: Settings,
    pub settings_path: PathBuf,
    pub runtime_handle: tokio::runtime::Handle,
    pub initial_passphrase: Option<Zeroizing<String>>,
}

pub struct SoloTrayApp {
    state: AppState,
    /// The tray icon. Owned for its lifetime; dropping removes it from
    /// the OS tray. Constructed in `new()` on the eframe thread so the
    /// menu-event channel dispatches in our event loop.
    tray: Option<tray_icon::TrayIcon>,
    /// Last health we used to refresh the tray icon. We only call
    /// `tray.set_icon()` on transitions to avoid the per-frame redraw
    /// cost — except while in the Starting state, where the pulse
    /// animation requires per-frame updates (capped to 4Hz repaint).
    last_health: DaemonHealth,
    /// Wall-clock start used for the icon pulse animation.
    started_at: std::time::Instant,
    /// UI state for the log viewer.
    filter_level: Level,
    auto_scroll: bool,
    log_source: LogSource,
    tray_log_lines: Vec<String>,
    tray_log_status: String,
    tray_log_last_refresh: Option<std::time::Instant>,
    active_tab: MainTab,
    setup_snapshot: SetupSnapshot,
    first_run_init: FirstRunInitState,
    first_run_init_rx: Option<Receiver<FirstRunInitResult>>,
    setup_action: SetupActionState,
    setup_result_rx: Option<Receiver<SetupActionResult>>,
    tool_snapshot: ToolSnapshot,
    mcp_probe: McpProbeState,
    mcp_probe_rx: Option<Receiver<McpProbeResult>>,
    client_check: ClientCheckState,
    client_check_rx: Option<Receiver<ClientCheckResult>>,
    setup_doctor: SetupDoctorState,
    setup_doctor_rx: Option<Receiver<SetupDoctorResult>>,
    selected_tool_detail: Option<SetupTarget>,
    backup_snapshot: BackupSnapshot,
    library_snapshot: LibrarySnapshot,
    memory_capture_text: String,
    memory_search_query: String,
    memory_context_query: String,
    memory_context_subject: String,
    memory_context: MemoryContextState,
    memory_context_rx: Option<Receiver<MemoryContextResult>>,
    memory_action: MemoryActionState,
    memory_result_rx: Option<Receiver<MemoryActionResult>>,
    memory_recent: MemoryRecentState,
    memory_recent_rx: Option<Receiver<MemoryRecentResult>>,
    memory_review_filter: MemoryReviewFilter,
    memory_source_filter: MemorySourceFilter,
    memory_detail: MemoryDetailState,
    memory_detail_rx: Option<Receiver<MemoryDetailResult>>,
    memory_edit_text: String,
    memory_update: MemoryUpdateState,
    memory_update_rx: Option<Receiver<MemoryUpdateResult>>,
    memory_forget_confirmed: bool,
    memory_forget: MemoryForgetState,
    memory_forget_rx: Option<Receiver<MemoryForgetResult>>,
    memory_contradictions: MemoryContradictionState,
    memory_contradictions_rx: Option<Receiver<MemoryContradictionResult>>,
    memory_contradiction_resolve: MemoryContradictionResolveState,
    memory_contradiction_resolve_rx: Option<Receiver<MemoryContradictionResolveResult>>,
    import_source: ImportSource,
    import_path_input: String,
    import_action: ImportActionState,
    import_result_rx: Option<Receiver<ImportActionResult>>,
    import_commit_confirmed: bool,
    import_commit: ImportCommitState,
    import_commit_rx: Option<Receiver<ImportCommitResult>>,
    document_list: DocumentListState,
    document_list_rx: Option<Receiver<DocumentListResult>>,
    document_search_query: String,
    document_search: DocumentSearchState,
    document_search_rx: Option<Receiver<DocumentSearchResult>>,
    document_detail: DocumentDetailState,
    document_detail_rx: Option<Receiver<DocumentDetailResult>>,
    document_forget_confirmed: bool,
    document_forget: DocumentForgetState,
    document_forget_rx: Option<Receiver<DocumentForgetResult>>,
    backup_action: BackupActionState,
    backup_result_rx: Option<Receiver<BackupActionResult>>,
    ollama_migration_model: String,
    ollama_migration_dim: String,
    ollama_migration_base_url: String,
    ollama_migration_passphrase: String,
    ollama_migration: OllamaMigrationState,
    ollama_migration_rx: Option<Receiver<OllamaMigrationResult>>,
    ollama_migration_restart_passphrase: Option<Zeroizing<String>>,
    secret_snapshot: SecretSnapshot,
    secret_action: SecretActionState,
    project_snapshot: ProjectMemorySnapshot,
    workspace_file_access_snapshot: WorkspaceFileAccessSnapshot,
    workspace_file_access_message: Option<String>,
    workspace_file_access_restart_required: bool,
    project_action: ProjectActionState,
    project_result_rx: Option<Receiver<ProjectActionResult>>,
    project_init_confirmed: bool,
    project_docs_preview: Option<ProjectDocsPreview>,
    project_docs_import_confirmed: bool,
    project_docs_import: ProjectDocsImportState,
    project_docs_import_rx: Option<Receiver<ProjectDocsImportResult>>,
    project_root_input: String,
    project_decision_text: String,
    project_decision_query: String,
    project_decision_action: ProjectDecisionActionState,
    project_decision_rx: Option<Receiver<ProjectDecisionResult>>,
    project_fact_subject: String,
    project_facts: ProjectFactsState,
    project_facts_rx: Option<Receiver<ProjectFactsResult>>,
    pending_keychain_passphrase: Option<Zeroizing<String>>,
    last_detection_refresh: std::time::Instant,
    /// Visible state of the log-viewer panel — controls whether
    /// `draw_main_window` actually renders its central panel. The
    /// underlying eframe viewport is always visible (just possibly
    /// minimised); see the module-level discussion in
    /// `crates/solo-tray/src/main.rs` for why we don't hide it.
    window_visible: bool,
    /// Track if we've requested a graceful quit (so we can show the
    /// "shutting down…" placeholder UI while the daemon drains).
    quitting: bool,
    daemon_started: bool,
    passphrase_input: String,
    init_passphrase_confirm: String,
    init_first_name: String,
    keychain_passphrase_input: String,
    bearer_token_input: String,
    passphrase_error: Option<String>,
    /// Frame counter for the periodic heartbeat log. Lets us confirm
    /// in `~/.solo/tray.log` that `update()` is being called
    /// regularly. Caught the hidden-viewport `update()`-starvation
    /// bug; kept as a permanent operator-visible regression sensor.
    update_ticks: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainTab {
    Controls,
    Dashboard,
    Health,
    Mcp,
    Memory,
    Projects,
    Tools,
    Settings,
    Data,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogSource {
    Daemon,
    Tray,
}

impl LogSource {
    const ALL: [Self; 2] = [Self::Daemon, Self::Tray];

    fn label(self) -> &'static str {
        match self {
            Self::Daemon => "Daemon stderr",
            Self::Tray => "Tray log",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Daemon => "Live daemon output captured in memory",
            Self::Tray => "Persistent tray log from the Solo data directory",
        }
    }
}

#[derive(Debug, Clone)]
struct SetupSnapshot {
    data_dir: PathBuf,
    current_exe: Option<PathBuf>,
    sibling_solo: PathBuf,
    sibling_solo_exists: bool,
    solo_on_path_exists: bool,
    solo_command_available: bool,
    settings_exists: bool,
    solo_config_exists: bool,
    lockfile: LockfileSnapshot,
}

#[derive(Debug, Clone)]
struct LockfileSnapshot {
    path: PathBuf,
    state: LockfileState,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockfileState {
    Free,
    Stale,
    Held,
    Unreadable,
}

#[derive(Debug, Clone)]
struct ToolSnapshot {
    rows: Vec<ToolConfigRow>,
}

#[derive(Debug, Clone)]
struct ToolConfigRow {
    target: SetupTarget,
    path: Option<PathBuf>,
    state: ToolConfigState,
    transport: ToolTransport,
    profile_route: ToolProfileRoute,
    detail: String,
    last_status: Option<ConnectedToolLastStatus>,
}

#[derive(Debug)]
enum McpProbeState {
    Idle,
    Running {
        profile: String,
        started_at: std::time::Instant,
    },
    Succeeded {
        summary: McpProbeSuccess,
        completed_at: std::time::SystemTime,
    },
    Failed {
        profile: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

#[derive(Debug)]
struct McpProbeResult {
    result: Result<McpProbeSuccess, String>,
}

#[derive(Debug, Clone)]
struct McpProbeSuccess {
    profile: String,
    server_name: String,
    server_version: String,
    protocol_version: String,
    tool_count: usize,
    session_id: String,
    used_bearer_token: bool,
}

impl McpProbeState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

#[derive(Debug)]
enum ClientCheckState {
    Idle,
    Running {
        target: SetupTarget,
        started_at: std::time::Instant,
    },
    Succeeded {
        target: SetupTarget,
        summary: String,
        completed_at: std::time::SystemTime,
    },
    Failed {
        target: SetupTarget,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl ClientCheckState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

#[derive(Debug)]
struct ClientCheckResult {
    target: SetupTarget,
    result: Result<ClientCheckSuccess, String>,
}

#[derive(Debug)]
struct ClientCheckSuccess {
    summary: String,
}

#[derive(Debug)]
enum SetupDoctorState {
    Idle,
    Running {
        target: SetupTarget,
        started_at: std::time::Instant,
    },
    Succeeded {
        target: SetupTarget,
        report: SetupDoctorReport,
        completed_at: std::time::SystemTime,
    },
    Failed {
        target: SetupTarget,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl SetupDoctorState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

#[derive(Debug)]
struct SetupDoctorResult {
    target: SetupTarget,
    result: Result<SetupDoctorReport, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetupDoctorReport {
    profile_route: Option<String>,
    endpoint: SetupDoctorEndpoint,
    clients: Vec<SetupDoctorClient>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetupDoctorEndpoint {
    url: String,
    status: String,
    detail: String,
    http_status: Option<u16>,
    tools: Option<SetupDoctorTools>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetupDoctorTools {
    tool_count: usize,
    missing_required_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetupDoctorClient {
    client: String,
    display_name: String,
    config_path: Option<String>,
    config_status: String,
    solo_entry: String,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolConfigState {
    Verified,
    NeedsSetup,
    NeedsRepair,
    Unknown,
}

impl ToolConfigState {
    fn label(self) -> &'static str {
        match self {
            Self::Verified => "Verified",
            Self::NeedsSetup => "Needs setup",
            Self::NeedsRepair => "Needs repair",
            Self::Unknown => "Unknown",
        }
    }

    fn tone(self) -> StateTone {
        match self {
            Self::Verified => StateTone::Good,
            Self::NeedsSetup | Self::Unknown => StateTone::Warn,
            Self::NeedsRepair => StateTone::Bad,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolTransport {
    Http,
    HttpBridge,
    Stdio,
    Unknown,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolProfileRoute {
    DaemonDefault,
    #[cfg(test)]
    Explicit(String),
    Unknown,
}

impl ToolProfileRoute {
    fn label(&self) -> String {
        match self {
            Self::DaemonDefault => "daemon default".to_string(),
            #[cfg(test)]
            Self::Explicit(profile) => format!("profile `{profile}`"),
            Self::Unknown => "unknown".to_string(),
        }
    }
}

impl ToolTransport {
    fn label(self) -> &'static str {
        match self {
            Self::Http => "HTTP",
            Self::HttpBridge => "HTTP bridge",
            Self::Stdio => "stdio",
            Self::Unknown => "unknown",
            Self::None => "-",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupWizardStepState {
    Complete,
    Active,
    Waiting,
}

impl SetupWizardStepState {
    fn label(self) -> &'static str {
        match self {
            Self::Complete => "Done",
            Self::Active => "Now",
            Self::Waiting => "Next",
        }
    }

    fn tone(self) -> StateTone {
        match self {
            Self::Complete => StateTone::Good,
            Self::Active | Self::Waiting => StateTone::Warn,
        }
    }
}

#[derive(Debug, Clone)]
struct BackupSnapshot {
    data_dir: PathBuf,
    db_path: PathBuf,
    snapshots_dir: PathBuf,
    latest_known_backup: Option<BackupFile>,
}

#[derive(Debug, Clone)]
struct BackupFile {
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone)]
struct LibrarySnapshot {
    db_path: PathBuf,
    exists: bool,
    size_bytes: Option<u64>,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct SecretSnapshot {
    backend: &'static str,
    passphrase_stored: Option<bool>,
    bearer_token_stored: Option<bool>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectMemorySnapshot {
    root: Option<PathBuf>,
    config_path: Option<PathBuf>,
    state: ProjectMemoryState,
    config: Option<ProjectMemoryConfig>,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceFileAccessSnapshot {
    config_path: PathBuf,
    state: WorkspaceFileAccessState,
    allowed_roots: Vec<String>,
    env_override: Option<String>,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceFileAccessState {
    ConfigMissing,
    Unrestricted,
    Restricted,
    Disabled,
    InvalidConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectMemoryConfig {
    name: String,
    project_id: String,
    tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectMemoryState {
    NotSelected,
    MissingRoot,
    MissingConfig,
    Ready,
    InvalidConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectActionKind {
    Init,
    Preview,
}

impl ProjectActionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Init => "create project config",
            Self::Preview => "preview docs",
        }
    }
}

#[derive(Debug)]
enum ProjectActionState {
    Idle,
    Running {
        kind: ProjectActionKind,
        root: PathBuf,
        started_at: std::time::Instant,
    },
    Succeeded {
        kind: ProjectActionKind,
        message: String,
        output: String,
        completed_at: std::time::SystemTime,
    },
    Failed {
        kind: ProjectActionKind,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl ProjectActionState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

struct ProjectActionResult {
    result: Result<ProjectActionSuccess, String>,
}

struct ProjectActionSuccess {
    kind: ProjectActionKind,
    message: String,
    output: String,
}

#[derive(Debug, Clone)]
struct ProjectDocsPreview {
    root: String,
    project_name: String,
    project_id: String,
    files_scanned: usize,
    candidates_found: usize,
    truncated: bool,
    candidates: Vec<ProjectDocCandidate>,
}

#[derive(Debug, Clone)]
struct ProjectDocCandidate {
    path: String,
    label: String,
}

#[derive(Debug)]
enum ProjectDocsImportState {
    Idle,
    Running {
        count: usize,
        started_at: std::time::Instant,
    },
    Succeeded {
        report: NativeImportReport,
        completed_at: std::time::SystemTime,
    },
    Failed {
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl ProjectDocsImportState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

struct ProjectDocsImportResult {
    result: Result<NativeImportReport, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectDecisionVerb {
    Add,
    Search,
}

impl ProjectDecisionVerb {
    fn label(self) -> &'static str {
        match self {
            Self::Add => "save decision",
            Self::Search => "search decisions",
        }
    }
}

#[derive(Debug)]
enum ProjectDecisionActionState {
    Idle,
    Adding {
        started_at: std::time::Instant,
    },
    Added {
        memory_id: String,
        completed_at: std::time::SystemTime,
    },
    Searching {
        query: String,
        started_at: std::time::Instant,
    },
    SearchSucceeded {
        query: String,
        hits: Vec<MemorySearchHit>,
        completed_at: std::time::SystemTime,
    },
    Failed {
        verb: ProjectDecisionVerb,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl ProjectDecisionActionState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Adding { .. } | Self::Searching { .. })
    }
}

struct ProjectDecisionResult {
    result: Result<ProjectDecisionSuccess, String>,
}

enum ProjectDecisionSuccess {
    Added {
        memory_id: String,
    },
    Search {
        query: String,
        hits: Vec<MemorySearchHit>,
    },
}

#[derive(Debug)]
enum ProjectFactsState {
    Idle,
    Loading {
        subject: String,
        started_at: std::time::Instant,
    },
    Loaded {
        subject: String,
        facts: Vec<ProjectFactHit>,
        completed_at: std::time::SystemTime,
    },
    Failed {
        subject: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl ProjectFactsState {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

struct ProjectFactsResult {
    result: Result<ProjectFactsSuccess, String>,
}

struct ProjectFactsSuccess {
    subject: String,
    facts: Vec<ProjectFactHit>,
}

#[derive(Debug, Clone)]
struct ProjectFactHit {
    triple_id: String,
    subject_id: String,
    predicate: String,
    object_id: String,
    object_kind: String,
    valid_from_ms: i64,
    valid_to_ms: Option<i64>,
    confidence: f32,
    cluster_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportSource {
    Markdown,
    Text,
    Json,
    ChatGpt,
    Claude,
    Bookmarks,
}

impl ImportSource {
    const ALL: [Self; 6] = [
        Self::Markdown,
        Self::Text,
        Self::Json,
        Self::ChatGpt,
        Self::Claude,
        Self::Bookmarks,
    ];

    fn command(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Text => "text",
            Self::Json => "json",
            Self::ChatGpt => "chatgpt",
            Self::Claude => "claude",
            Self::Bookmarks => "bookmarks",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Text => "Text",
            Self::Json => "JSON",
            Self::ChatGpt => "ChatGPT",
            Self::Claude => "Claude",
            Self::Bookmarks => "Bookmarks",
        }
    }

    fn picker_label(self) -> String {
        self.label().to_string()
    }
}

#[derive(Debug)]
enum ImportActionState {
    Idle,
    Running {
        source: ImportSource,
        path: PathBuf,
        started_at: std::time::Instant,
    },
    Succeeded {
        source: ImportSource,
        path: PathBuf,
        message: String,
        output: String,
        completed_at: std::time::SystemTime,
    },
    Failed {
        source: ImportSource,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl ImportActionState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

struct ImportActionResult {
    result: Result<ImportActionSuccess, String>,
}

struct ImportActionSuccess {
    source: ImportSource,
    path: PathBuf,
    message: String,
    output: String,
}

#[derive(Debug)]
enum ImportCommitState {
    Idle,
    Running {
        path: PathBuf,
        started_at: std::time::Instant,
    },
    Succeeded {
        report: NativeImportReport,
        completed_at: std::time::SystemTime,
    },
    Failed {
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl ImportCommitState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

struct ImportCommitResult {
    result: Result<NativeImportReport, String>,
}

#[derive(Debug, Clone)]
struct NativeImportReport {
    path: String,
    dry_run: bool,
    recursive: bool,
    truncated: bool,
    total_files: usize,
    total_bytes: u64,
    store_original_file: bool,
    imported: u32,
    deduped: u32,
    failed: u32,
    chunks_persisted: u32,
    assets_retained: u32,
    assets_deduped: u32,
    asset_links: u32,
    asset_failed: u32,
    results: Vec<NativeImportResult>,
}

#[derive(Debug, Clone)]
struct NativeImportResult {
    path: String,
    bytes: u64,
    doc_id: Option<String>,
    chunks_persisted: u32,
    bytes_ingested: u64,
    deduped: bool,
    asset_id: Option<String>,
    asset_error: Option<String>,
    error: Option<String>,
}

#[derive(Debug)]
enum DocumentListState {
    Idle,
    Loading {
        started_at: std::time::Instant,
    },
    Loaded {
        documents: Vec<DocumentSummary>,
        completed_at: std::time::SystemTime,
    },
    Failed {
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl DocumentListState {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

struct DocumentListResult {
    result: Result<Vec<DocumentSummary>, String>,
}

#[derive(Debug)]
enum DocumentSearchState {
    Idle,
    Searching {
        query: String,
        started_at: std::time::Instant,
    },
    Succeeded {
        query: String,
        hits: Vec<DocumentSearchHit>,
        completed_at: std::time::SystemTime,
    },
    Failed {
        query: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl DocumentSearchState {
    fn is_searching(&self) -> bool {
        matches!(self, Self::Searching { .. })
    }
}

struct DocumentSearchResult {
    result: Result<DocumentSearchSuccess, String>,
}

struct DocumentSearchSuccess {
    query: String,
    hits: Vec<DocumentSearchHit>,
}

#[derive(Debug, Clone)]
struct DocumentSearchHit {
    chunk_id: String,
    doc_id: String,
    doc_title: Option<String>,
    doc_source: Option<String>,
    doc_mime_type: Option<String>,
    chunk_index: u32,
    content: String,
    cos_distance: f32,
    start_offset: u32,
    end_offset: u32,
}

#[derive(Debug, Clone)]
struct DocumentSummary {
    doc_id: String,
    title: Option<String>,
    source: Option<String>,
    mime_type: Option<String>,
    ingested_at_ms: Option<i64>,
    chunk_count: u32,
    status: String,
}

#[derive(Debug)]
enum DocumentDetailState {
    Idle,
    Loading {
        doc_id: String,
        started_at: std::time::Instant,
    },
    Loaded {
        detail: DocumentDetail,
        completed_at: std::time::SystemTime,
    },
    Failed {
        doc_id: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl DocumentDetailState {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

struct DocumentDetailResult {
    result: Result<DocumentDetail, String>,
}

#[derive(Debug, Clone)]
struct DocumentDetail {
    doc_id: String,
    title: Option<String>,
    source: Option<String>,
    mime_type: Option<String>,
    ingested_at_ms: Option<i64>,
    modified_at_ms: Option<i64>,
    status: String,
    chunk_count: u32,
    content_hash: Option<String>,
    byte_size: Option<u64>,
    chunks: Vec<DocumentChunkSummary>,
}

#[derive(Debug, Clone)]
struct DocumentChunkSummary {
    chunk_id: String,
    chunk_index: u32,
    content_preview: String,
    token_count: u32,
}

#[derive(Debug)]
enum DocumentForgetState {
    Idle,
    Forgetting {
        doc_id: String,
        started_at: std::time::Instant,
    },
    Forgotten {
        report: DocumentForgetReport,
        completed_at: std::time::SystemTime,
    },
    Failed {
        doc_id: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl DocumentForgetState {
    fn is_forgetting(&self) -> bool {
        matches!(self, Self::Forgetting { .. })
    }
}

struct DocumentForgetResult {
    result: Result<DocumentForgetReport, String>,
}

#[derive(Debug, Clone)]
struct DocumentForgetReport {
    doc_id: String,
    chunks_tombstoned: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryActionVerb {
    Remember,
    Search,
}

impl MemoryActionVerb {
    fn label(self) -> &'static str {
        match self {
            Self::Remember => "remember",
            Self::Search => "search",
        }
    }
}

#[derive(Debug, Clone)]
struct MemorySearchHit {
    memory_id: String,
    content: String,
    source_type: String,
    tier: String,
    fused_score: f32,
    cos_distance: f32,
}

#[derive(Debug, Clone)]
struct RecentMemory {
    memory_id: String,
    label: String,
    preview: String,
    ts_ms: Option<i64>,
    source_type: Option<String>,
    salience: Option<f64>,
    status: Option<String>,
    review_state: Option<String>,
    reviewed_at_ms: Option<i64>,
    review_note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryReviewFilter {
    NeedsReview,
    Approved,
    Dismissed,
    All,
}

impl MemoryReviewFilter {
    const ALL: [Self; 4] = [
        Self::NeedsReview,
        Self::Approved,
        Self::Dismissed,
        Self::All,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::NeedsReview => "Needs review",
            Self::Approved => "Approved",
            Self::Dismissed => "Dismissed",
            Self::All => "All",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemorySourceFilter {
    All,
    HighSalience,
    UserCreated,
    AgentCreated,
    ToolOutput,
    DocumentDerived,
    SoloDesktop,
}

impl MemorySourceFilter {
    const ALL: [Self; 7] = [
        Self::All,
        Self::HighSalience,
        Self::UserCreated,
        Self::AgentCreated,
        Self::ToolOutput,
        Self::DocumentDerived,
        Self::SoloDesktop,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All sources",
            Self::HighSalience => "High salience",
            Self::UserCreated => "User-created",
            Self::AgentCreated => "Agent-created",
            Self::ToolOutput => "Tool output",
            Self::DocumentDerived => "Document-derived",
            Self::SoloDesktop => "Solo app",
        }
    }
}

#[derive(Debug, Clone)]
struct MemoryDetail {
    memory_id: String,
    content: String,
    source_type: String,
    source_id: Option<String>,
    tier: String,
    status: String,
    salience: f64,
    confidence: f64,
    strength: f64,
    created_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
}

#[derive(Debug)]
enum MemoryActionState {
    Idle,
    Remembering {
        started_at: std::time::Instant,
    },
    Remembered {
        memory_id: String,
        completed_at: std::time::SystemTime,
    },
    Searching {
        query: String,
        started_at: std::time::Instant,
    },
    SearchSucceeded {
        query: String,
        hits: Vec<MemorySearchHit>,
        index_len: usize,
        candidates_considered: usize,
        completed_at: std::time::SystemTime,
    },
    Failed {
        verb: MemoryActionVerb,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl MemoryActionState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Remembering { .. } | Self::Searching { .. })
    }
}

struct MemoryActionResult {
    result: Result<MemoryActionSuccess, String>,
}

#[derive(Debug)]
enum MemoryContextState {
    Idle,
    Loading {
        query: String,
        started_at: std::time::Instant,
    },
    Loaded {
        summary: MemoryContextSummary,
        completed_at: std::time::SystemTime,
    },
    Failed {
        query: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl MemoryContextState {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

struct MemoryContextResult {
    result: Result<MemoryContextSummary, String>,
}

#[derive(Debug, Clone)]
struct MemoryContextSummary {
    query: String,
    subject: Option<String>,
    resolved_subject: Option<String>,
    sections: Vec<MemoryContextSection>,
    recall_hits: Vec<MemorySearchHit>,
    facts: Vec<ProjectFactHit>,
    themes: Vec<MemoryContextTheme>,
    graph: MemoryContextGraph,
}

#[derive(Debug, Clone)]
struct MemoryContextSection {
    name: &'static str,
    status: String,
    count: usize,
    warning: Option<String>,
}

#[derive(Debug, Clone)]
struct MemoryContextTheme {
    cluster_id: String,
    abstraction_text: Option<String>,
    episode_count: i64,
    coherence: f32,
    created_at_ms: i64,
}

#[derive(Debug, Clone, Default)]
struct MemoryContextGraph {
    seed_entities: Vec<String>,
    relationship_facts: Vec<MemoryContextGraphFact>,
    literal_facts: Vec<MemoryContextGraphFact>,
    review_warnings: Vec<MemoryContextGraphReviewWarning>,
}

#[derive(Debug, Clone)]
struct MemoryContextGraphFact {
    subject_id: String,
    predicate: String,
    object_id: String,
    object_kind: String,
    confidence: f32,
    evidence_preview: Option<String>,
}

#[derive(Debug, Clone)]
struct MemoryContextGraphReviewWarning {
    reason_code: String,
    subject_id: String,
    predicate: String,
    object_id: String,
}

enum MemoryActionSuccess {
    Remembered {
        memory_id: String,
    },
    Search {
        query: String,
        hits: Vec<MemorySearchHit>,
        index_len: usize,
        candidates_considered: usize,
    },
}

#[derive(Debug)]
enum MemoryRecentState {
    Idle,
    Loading {
        started_at: std::time::Instant,
    },
    Loaded {
        memories: Vec<RecentMemory>,
        completed_at: std::time::SystemTime,
    },
    Failed {
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl MemoryRecentState {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

struct MemoryRecentResult {
    result: Result<Vec<RecentMemory>, String>,
}

#[derive(Debug)]
enum MemoryDetailState {
    Idle,
    Loading {
        memory_id: String,
        started_at: std::time::Instant,
    },
    Loaded {
        detail: MemoryDetail,
        completed_at: std::time::SystemTime,
    },
    Failed {
        memory_id: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl MemoryDetailState {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

struct MemoryDetailResult {
    result: Result<MemoryDetail, String>,
}

#[derive(Debug)]
enum MemoryUpdateState {
    Idle,
    Updating {
        memory_id: String,
        started_at: std::time::Instant,
    },
    Updated {
        memory_id: String,
        updated_at_ms: Option<i64>,
        completed_at: std::time::SystemTime,
    },
    Failed {
        memory_id: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl MemoryUpdateState {
    fn is_updating(&self) -> bool {
        matches!(self, Self::Updating { .. })
    }
}

struct MemoryUpdateResult {
    result: Result<MemoryUpdateSuccess, String>,
}

struct MemoryUpdateSuccess {
    memory_id: String,
    content: String,
    updated_at_ms: Option<i64>,
}

#[derive(Debug)]
enum MemoryForgetState {
    Idle,
    Forgetting {
        memory_id: String,
        started_at: std::time::Instant,
    },
    Forgotten {
        memory_id: String,
        completed_at: std::time::SystemTime,
    },
    Failed {
        memory_id: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl MemoryForgetState {
    fn is_forgetting(&self) -> bool {
        matches!(self, Self::Forgetting { .. })
    }
}

struct MemoryForgetResult {
    result: Result<String, String>,
}

#[derive(Debug)]
enum MemoryContradictionState {
    Idle,
    Loading {
        started_at: std::time::Instant,
    },
    Loaded {
        contradictions: Vec<MemoryContradiction>,
        completed_at: std::time::SystemTime,
    },
    Failed {
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl MemoryContradictionState {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

struct MemoryContradictionResult {
    result: Result<Vec<MemoryContradiction>, String>,
}

#[derive(Debug, Clone)]
struct MemoryContradiction {
    a_id: String,
    b_id: String,
    kind: String,
    explanation: String,
    detected_at_ms: Option<i64>,
    status: String,
    resolved_at_ms: Option<i64>,
    resolution_note: Option<String>,
    winning_triple_id: Option<String>,
    a_triple: Option<MemoryContradictionTriple>,
    b_triple: Option<MemoryContradictionTriple>,
}

#[derive(Debug, Clone)]
struct MemoryContradictionTriple {
    triple_id: String,
    subject_id: String,
    predicate: String,
    object_id: String,
    object_kind: String,
    valid_from_ms: Option<i64>,
    valid_to_ms: Option<i64>,
}

#[derive(Debug)]
enum MemoryContradictionResolveState {
    Idle,
    Resolving {
        label: String,
        started_at: std::time::Instant,
    },
    Resolved {
        resolution: MemoryContradictionResolution,
        completed_at: std::time::SystemTime,
    },
    Failed {
        label: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl MemoryContradictionResolveState {
    fn is_resolving(&self) -> bool {
        matches!(self, Self::Resolving { .. })
    }
}

struct MemoryContradictionResolveResult {
    result: Result<MemoryContradictionResolution, String>,
}

#[derive(Debug, Clone)]
struct MemoryContradictionResolution {
    a_id: String,
    b_id: String,
    kind: String,
    status: String,
    resolved_at_ms: Option<i64>,
    resolution_note: Option<String>,
    winning_triple_id: Option<String>,
}

struct ContradictionResolveRequest {
    a_id: String,
    b_id: String,
    kind: String,
    status: String,
    resolution_note: Option<String>,
    winning_triple_id: Option<String>,
}

struct ContradictionResolveAction {
    a_id: String,
    b_id: String,
    kind: String,
    status: String,
    winning_triple_id: Option<String>,
}

#[derive(Debug)]
enum SetupActionState {
    Idle,
    Running {
        target: SetupTarget,
        verb: SetupActionVerb,
        started_at: std::time::Instant,
    },
    Succeeded {
        target: SetupTarget,
        verb: SetupActionVerb,
        message: String,
        completed_at: std::time::SystemTime,
    },
    Failed {
        target: SetupTarget,
        verb: SetupActionVerb,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

#[derive(Debug)]
enum FirstRunInitState {
    Idle,
    Running {
        started_at: std::time::Instant,
    },
    Succeeded {
        message: String,
        completed_at: std::time::SystemTime,
    },
    Failed {
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl FirstRunInitState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

struct FirstRunInitResult {
    result: Result<FirstRunInitSuccess, String>,
}

struct FirstRunInitSuccess {
    passphrase: Zeroizing<String>,
    data_dir: PathBuf,
    config_path: PathBuf,
    schema_version: u32,
    user_alias_set: bool,
}

impl SetupActionState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    fn target_and_verb(&self) -> Option<(SetupTarget, SetupActionVerb)> {
        match self {
            Self::Running { target, verb, .. }
            | Self::Succeeded { target, verb, .. }
            | Self::Failed { target, verb, .. } => Some((*target, *verb)),
            Self::Idle => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupActionVerb {
    Apply,
    Verify,
}

impl SetupActionVerb {
    fn label(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Verify => "verify",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpectedToolProfileRoute {
    Any,
    DaemonDefault,
}

impl ExpectedToolProfileRoute {
    fn label(&self) -> String {
        match self {
            Self::Any => "any profile route".to_string(),
            Self::DaemonDefault => "daemon default".to_string(),
        }
    }

    fn matches_route(&self, route: &ToolProfileRoute) -> bool {
        matches!(
            (self, route),
            (Self::Any, _) | (Self::DaemonDefault, ToolProfileRoute::DaemonDefault)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupTarget {
    ClaudeDesktop,
    Cursor,
    CodexUser,
    CodexProject,
}

impl SetupTarget {
    const ALL: [Self; 4] = [
        Self::ClaudeDesktop,
        Self::Cursor,
        Self::CodexUser,
        Self::CodexProject,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "Claude Desktop",
            Self::Cursor => "Cursor",
            Self::CodexUser => "Codex (user)",
            Self::CodexProject => "Codex (project)",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "claude-desktop",
            Self::Cursor => "cursor",
            Self::CodexUser => "codex-user",
            Self::CodexProject => "codex-project",
        }
    }

    fn supports_automated_client_check(self) -> bool {
        matches!(self, Self::CodexUser | Self::CodexProject)
    }

    fn apply_args(self, mcp_url: &str, project_root: Option<&Path>) -> Vec<std::ffi::OsString> {
        let mut args = match self {
            Self::ClaudeDesktop => os_args([
                "setup-client",
                "claude-desktop",
                "--transport",
                "http",
                "--url",
                mcp_url,
                "--apply",
            ]),
            Self::Cursor => os_args([
                "setup-client",
                "cursor",
                "--transport",
                "http",
                "--url",
                mcp_url,
                "--apply",
            ]),
            Self::CodexUser | Self::CodexProject => os_args([
                "setup-client",
                "codex",
                "--scope",
                if self == Self::CodexProject {
                    "project"
                } else {
                    "user"
                },
                "--transport",
                "http",
                "--url",
                mcp_url,
                "--apply",
            ]),
        };
        if self == Self::CodexProject
            && let Some(root) = project_root
        {
            args.push("--project-dir".into());
            args.push(root.into());
        }
        args
    }

    fn verify_args(self, project_root: Option<&Path>) -> Vec<std::ffi::OsString> {
        let mut args = match self {
            Self::ClaudeDesktop => os_args(["setup-client", "verify", "claude-desktop"]),
            Self::Cursor => os_args(["setup-client", "verify", "cursor"]),
            Self::CodexUser => os_args(["setup-client", "verify", "codex", "--scope", "user"]),
            Self::CodexProject => {
                os_args(["setup-client", "verify", "codex", "--scope", "project"])
            }
        };
        if self == Self::CodexProject
            && let Some(root) = project_root
        {
            args.push("--project-dir".into());
            args.push(root.into());
        }
        args
    }

    fn doctor_args(self, mcp_url: &str, project_root: Option<&Path>) -> Vec<std::ffi::OsString> {
        let mut args = match self {
            Self::ClaudeDesktop => os_args([
                "setup-client",
                "doctor",
                "claude-desktop",
                "--url",
                mcp_url,
                "--format",
                "json",
            ]),
            Self::Cursor => os_args([
                "setup-client",
                "doctor",
                "cursor",
                "--url",
                mcp_url,
                "--format",
                "json",
            ]),
            Self::CodexUser => os_args([
                "setup-client",
                "doctor",
                "codex",
                "--scope",
                "user",
                "--url",
                mcp_url,
                "--format",
                "json",
            ]),
            Self::CodexProject => os_args([
                "setup-client",
                "doctor",
                "codex",
                "--scope",
                "project",
                "--url",
                mcp_url,
                "--format",
                "json",
            ]),
        };
        if self == Self::CodexProject
            && let Some(root) = project_root
        {
            args.push("--project-dir".into());
            args.push(root.into());
        }
        args
    }
}

#[derive(Clone, Copy)]
struct PolicyPackRow {
    label: &'static str,
    detail: &'static str,
    text: &'static str,
}

fn generic_policy_pack_row() -> PolicyPackRow {
    PolicyPackRow {
        label: "Generic MCP agent",
        detail: "Portable MCP memory policy",
        text: POLICY_GENERIC_MCP_AGENT,
    }
}

fn policy_pack_rows() -> Vec<PolicyPackRow> {
    vec![generic_policy_pack_row()]
}

fn memory_library_agents_description() -> &'static str {
    "Claude, Cursor, and Codex use the Community Memory Library."
}

fn daemon_ready_clients_description() -> &'static str {
    "Solo is ready for Desktop and MCP clients."
}

fn policy_text_for_setup_target(target: SetupTarget) -> &'static str {
    match target {
        SetupTarget::ClaudeDesktop => POLICY_CLAUDE_DESKTOP,
        SetupTarget::Cursor => POLICY_CURSOR,
        SetupTarget::CodexUser | SetupTarget::CodexProject => POLICY_CODEX,
    }
}

fn client_smoke_instruction(target: SetupTarget, project_root: Option<&Path>) -> String {
    match target {
        SetupTarget::CodexUser => "codex mcp list".to_string(),
        SetupTarget::CodexProject => project_root
            .map(|root| format!("cd {}\ncodex mcp list", shell_arg(&display_path(root))))
            .unwrap_or_else(|| {
                "Select a project root in Solo, then run codex mcp list from that project."
                    .to_string()
            }),
        SetupTarget::ClaudeDesktop => {
            "Restart Claude Desktop, open MCP tools, and confirm `solo` is listed.".to_string()
        }
        SetupTarget::Cursor => {
            "Open Cursor MCP settings/tools and confirm `solo` is listed.".to_string()
        }
    }
}

struct SetupActionResult {
    result: Result<SetupActionSuccess, String>,
}

struct SetupActionSuccess {
    message: String,
    verification: ToolVerification,
}

#[derive(Debug, Clone)]
struct ToolVerification {
    state: ToolConfigState,
    transport: ToolTransport,
    profile_route: ToolProfileRoute,
    detail: String,
    config_path: Option<String>,
}

#[derive(Debug)]
enum BackupActionState {
    Idle,
    Running {
        dest: PathBuf,
        started_at: std::time::Instant,
    },
    Succeeded {
        path: PathBuf,
        elapsed_ms: u64,
        completed_at: std::time::SystemTime,
    },
    Failed {
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl BackupActionState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

struct BackupActionResult {
    result: Result<BackupActionSuccess, String>,
}

struct BackupActionSuccess {
    path: PathBuf,
    elapsed_ms: u64,
}

#[derive(Debug)]
enum OllamaMigrationState {
    Idle,
    Running {
        model: String,
        started_at: std::time::Instant,
    },
    Succeeded {
        model: String,
        summary: String,
        completed_at: std::time::SystemTime,
    },
    Failed {
        model: String,
        message: String,
        completed_at: std::time::SystemTime,
    },
}

impl OllamaMigrationState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

struct OllamaMigrationResult {
    model: String,
    result: Result<OllamaMigrationSuccess, String>,
}

struct OllamaMigrationSuccess {
    summary: String,
}

#[derive(Debug)]
enum SecretActionState {
    Idle,
    Succeeded {
        message: String,
        completed_at: std::time::SystemTime,
    },
    Failed {
        message: String,
        completed_at: std::time::SystemTime,
    },
}

struct DaemonSnapshot {
    state: SupervisorState,
    pid: Option<u32>,
    running: bool,
    supervisor_exited: bool,
}

struct StatusSnapshot {
    health: DaemonHealth,
    last_payload: Option<serde_json::Value>,
    last_ok_at: Option<std::time::SystemTime>,
    last_error: Option<String>,
}

impl SoloTrayApp {
    pub fn new(cc: &CreationContext<'_>, mut state: AppState) -> Self {
        // Spawn the repaint pump so `update()` runs (≈ 4 Hz) even
        // when the viewport is minimised — without this the
        // forwarded-menu queue never gets drained and tray-menu
        // clicks vanish.
        tray::spawn_repaint_pump(cc.egui_ctx.clone(), std::time::Duration::from_millis(250));

        // Fast-path dispatcher: parks on muda's channel, handles
        // the high-frequency menu items (Quit, Open web, Open data
        // dir and Show logs) directly so they work even
        // when the eframe viewport is minimised and `update()` has
        // stopped ticking. Other events are forwarded to the eframe
        // queue. MUST be spawned BEFORE `build_tray` so it's the
        // sole consumer of muda's channel from the very first click.
        tray::spawn_menu_dispatcher(
            state.settings.solo_web_url.clone(),
            state.daemon_handle.clone(),
            state.runtime_handle.clone(),
        );

        let menu = tray::build_menu();
        let tray = tray::build_tray(menu, DaemonHealth::Starting);

        // Apply the persisted theme on first frame.
        apply_theme(&cc.egui_ctx, state.settings.theme);

        tracing::info!("SoloTrayApp constructed; entering eframe event loop");

        let initial_passphrase = state.initial_passphrase.take();
        let setup_snapshot = collect_setup_snapshot(&state.settings_path);
        let backup_snapshot = collect_backup_snapshot();
        let library_snapshot = collect_library_snapshot(&backup_snapshot.data_dir);
        let secret_snapshot =
            collect_secret_snapshot(state.settings.remember_passphrase_in_keychain);
        let project_snapshot =
            collect_project_memory_snapshot(state.settings.project_root.as_deref());
        let workspace_file_access_snapshot =
            collect_workspace_file_access_snapshot(&setup_snapshot.data_dir);
        let project_root_input = state
            .settings
            .project_root
            .as_deref()
            .map(display_path)
            .unwrap_or_default();
        let tool_snapshot = collect_tool_snapshot(
            &state.settings.connected_tools,
            state.settings.project_root.as_deref(),
            COMMUNITY_LIBRARY_KEY,
        );
        let mut app = Self {
            state,
            tray,
            last_health: DaemonHealth::Starting,
            started_at: std::time::Instant::now(),
            filter_level: Level::Info,
            auto_scroll: true,
            log_source: LogSource::Daemon,
            tray_log_lines: Vec::new(),
            tray_log_status: "tray log not loaded yet".to_string(),
            tray_log_last_refresh: None,
            active_tab: MainTab::Controls,
            setup_snapshot,
            first_run_init: FirstRunInitState::Idle,
            first_run_init_rx: None,
            setup_action: SetupActionState::Idle,
            setup_result_rx: None,
            tool_snapshot,
            mcp_probe: McpProbeState::Idle,
            mcp_probe_rx: None,
            client_check: ClientCheckState::Idle,
            client_check_rx: None,
            setup_doctor: SetupDoctorState::Idle,
            setup_doctor_rx: None,
            selected_tool_detail: None,
            backup_snapshot,
            library_snapshot,
            memory_capture_text: String::new(),
            memory_search_query: String::new(),
            memory_context_query: String::new(),
            memory_context_subject: String::new(),
            memory_context: MemoryContextState::Idle,
            memory_context_rx: None,
            memory_action: MemoryActionState::Idle,
            memory_result_rx: None,
            memory_recent: MemoryRecentState::Idle,
            memory_recent_rx: None,
            memory_review_filter: MemoryReviewFilter::NeedsReview,
            memory_source_filter: MemorySourceFilter::All,
            memory_detail: MemoryDetailState::Idle,
            memory_detail_rx: None,
            memory_edit_text: String::new(),
            memory_update: MemoryUpdateState::Idle,
            memory_update_rx: None,
            memory_forget_confirmed: false,
            memory_forget: MemoryForgetState::Idle,
            memory_forget_rx: None,
            memory_contradictions: MemoryContradictionState::Idle,
            memory_contradictions_rx: None,
            memory_contradiction_resolve: MemoryContradictionResolveState::Idle,
            memory_contradiction_resolve_rx: None,
            import_source: ImportSource::Markdown,
            import_path_input: String::new(),
            import_action: ImportActionState::Idle,
            import_result_rx: None,
            import_commit_confirmed: false,
            import_commit: ImportCommitState::Idle,
            import_commit_rx: None,
            document_list: DocumentListState::Idle,
            document_list_rx: None,
            document_search_query: String::new(),
            document_search: DocumentSearchState::Idle,
            document_search_rx: None,
            document_detail: DocumentDetailState::Idle,
            document_detail_rx: None,
            document_forget_confirmed: false,
            document_forget: DocumentForgetState::Idle,
            document_forget_rx: None,
            backup_action: BackupActionState::Idle,
            backup_result_rx: None,
            ollama_migration_model: "nomic-embed-text".to_string(),
            ollama_migration_dim: String::new(),
            ollama_migration_base_url: "http://localhost:11434".to_string(),
            ollama_migration_passphrase: String::new(),
            ollama_migration: OllamaMigrationState::Idle,
            ollama_migration_rx: None,
            ollama_migration_restart_passphrase: None,
            secret_snapshot,
            secret_action: SecretActionState::Idle,
            project_snapshot,
            workspace_file_access_snapshot,
            workspace_file_access_message: None,
            workspace_file_access_restart_required: false,
            project_action: ProjectActionState::Idle,
            project_result_rx: None,
            project_init_confirmed: false,
            project_docs_preview: None,
            project_docs_import_confirmed: false,
            project_docs_import: ProjectDocsImportState::Idle,
            project_docs_import_rx: None,
            project_root_input,
            project_decision_text: String::new(),
            project_decision_query: String::new(),
            project_decision_action: ProjectDecisionActionState::Idle,
            project_decision_rx: None,
            project_fact_subject: String::new(),
            project_facts: ProjectFactsState::Idle,
            project_facts_rx: None,
            pending_keychain_passphrase: None,
            last_detection_refresh: std::time::Instant::now(),
            // Start with the log panel rendered. The eframe viewport
            // is also visible at launch (we removed the
            // `with_visible(false)` builder hint that broke menu
            // dispatch) — so the user sees the log viewer + daemon
            // start sequence on first run, which is a much better
            // first impression than "click the tray icon to find
            // out where the app went".
            window_visible: true,
            quitting: false,
            daemon_started: false,
            passphrase_input: String::new(),
            init_passphrase_confirm: String::new(),
            init_first_name: String::new(),
            keychain_passphrase_input: String::new(),
            bearer_token_input: String::new(),
            passphrase_error: None,
            update_ticks: 0,
        };
        if let Some(passphrase) = initial_passphrase {
            app.start_daemon(passphrase);
        }
        app
    }

    fn start_daemon(&mut self, passphrase: Zeroizing<String>) {
        if let Err(message) = self.clear_stale_lock_before_start() {
            self.passphrase_error = Some(message);
            return;
        }
        match self.state.daemon_handle.try_lock() {
            Ok(mut h) => {
                if !h.prepare_start() {
                    tracing::info!(state = ?h.state, "daemon start ignored; supervisor already active");
                    return;
                }
            }
            Err(_) => {
                self.passphrase_error = Some("Daemon state is busy; try again.".to_string());
                return;
            }
        }
        self.daemon_started = true;
        self.passphrase_error = None;
        self.workspace_file_access_restart_required = false;
        if let Ok(mut status) = self.state.status_state.try_lock() {
            status.health = DaemonHealth::Starting;
            status.last_error = None;
        }
        let daemon = self.state.daemon_handle.clone();
        let logs = self.state.log_buffer.clone();
        self.state.runtime_handle.spawn(async move {
            if let Err(e) = crate::daemon::supervise(daemon, logs, passphrase).await {
                tracing::error!(error = %e, "daemon supervisor exited");
            }
        });
    }

    fn clear_stale_lock_before_start(&mut self) -> Result<(), String> {
        self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
        match self.setup_snapshot.lockfile.state {
            LockfileState::Free => Ok(()),
            LockfileState::Stale => {
                clear_stale_lockfile(&self.setup_snapshot.lockfile)?;
                self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
                Ok(())
            }
            LockfileState::Held => Err(format!(
                "Solo lock is held by another process. {}",
                self.setup_snapshot.lockfile.detail
            )),
            LockfileState::Unreadable => Err(format!(
                "Solo lock could not be inspected. {}",
                self.setup_snapshot.lockfile.detail
            )),
        }
    }

    fn clear_stale_lock_now(&mut self) {
        self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
        match self.setup_snapshot.lockfile.state {
            LockfileState::Stale => match clear_stale_lockfile(&self.setup_snapshot.lockfile) {
                Ok(()) => {
                    self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
                    self.passphrase_error =
                        Some("Stale daemon lock cleared. Enter passphrase and Start Solo.".into());
                }
                Err(message) => {
                    self.passphrase_error = Some(message);
                }
            },
            LockfileState::Free => {
                self.passphrase_error = Some("No daemon lock is present.".to_string());
            }
            LockfileState::Held | LockfileState::Unreadable => {
                self.passphrase_error = Some(self.setup_snapshot.lockfile.detail.clone());
            }
        }
    }

    fn start_daemon_from_keychain(&mut self) {
        match crate::secret_store::load_daemon_passphrase() {
            Ok(Some(passphrase)) => {
                self.secret_action = SecretActionState::Succeeded {
                    message: "loaded passphrase from OS keychain".to_string(),
                    completed_at: std::time::SystemTime::now(),
                };
                self.secret_snapshot =
                    collect_secret_snapshot(self.state.settings.remember_passphrase_in_keychain);
                self.start_daemon(passphrase);
            }
            Ok(None) => {
                self.passphrase_error = Some("No daemon passphrase is stored.".to_string());
                self.secret_action = SecretActionState::Failed {
                    message: "no daemon passphrase is stored".to_string(),
                    completed_at: std::time::SystemTime::now(),
                };
                self.secret_snapshot =
                    collect_secret_snapshot(self.state.settings.remember_passphrase_in_keychain);
            }
            Err(message) => {
                self.passphrase_error = Some(message.clone());
                self.secret_action = SecretActionState::Failed {
                    message,
                    completed_at: std::time::SystemTime::now(),
                };
                self.secret_snapshot =
                    collect_secret_snapshot(self.state.settings.remember_passphrase_in_keychain);
            }
        }
    }

    fn start_first_run_init(&mut self) {
        if self.first_run_init.is_running() {
            return;
        }

        self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
        if self.setup_snapshot.solo_config_exists {
            self.passphrase_error =
                Some("Solo memory is already initialized; start Solo with your passphrase.".into());
            return;
        }
        if self.passphrase_input.is_empty() {
            self.passphrase_error = Some("Passphrase must not be empty.".to_string());
            return;
        }
        if self.init_passphrase_confirm.is_empty() {
            self.passphrase_error =
                Some("Confirm the passphrase before creating Solo.".to_string());
            return;
        }
        if self.passphrase_input != self.init_passphrase_confirm {
            self.passphrase_error = Some("Passphrases did not match.".to_string());
            return;
        }

        let passphrase = Zeroizing::new(std::mem::take(&mut self.passphrase_input));
        self.init_passphrase_confirm.zeroize();
        let first_name = std::mem::take(&mut self.init_first_name);
        let data_dir = self.setup_snapshot.data_dir.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.first_run_init_rx = Some(rx);
        self.first_run_init = FirstRunInitState::Running {
            started_at: std::time::Instant::now(),
        };
        self.passphrase_error = None;

        self.state.runtime_handle.spawn(async move {
            let result = run_first_run_init(data_dir, passphrase, first_name).await;
            let _ = tx.send(FirstRunInitResult { result });
        });
    }

    fn request_daemon_restart(&mut self) {
        self.workspace_file_access_restart_required = false;
        let handle = self.state.daemon_handle.clone();
        self.state.runtime_handle.spawn(async move {
            handle.lock().await.request_restart();
        });
    }

    fn queue_keychain_passphrase(&mut self, passphrase: &str) {
        if !self.state.settings.remember_passphrase_in_keychain {
            return;
        }
        self.store_passphrase_in_keychain(passphrase);
    }

    fn store_pending_keychain_passphrase(&mut self) {
        let Some(passphrase) = self.pending_keychain_passphrase.take() else {
            return;
        };
        self.store_passphrase_in_keychain(passphrase.as_str());
    }

    fn clear_pending_keychain_passphrase(&mut self, message: Option<&str>) {
        if self.pending_keychain_passphrase.take().is_some()
            && let Some(message) = message
        {
            self.secret_action = SecretActionState::Failed {
                message: message.to_string(),
                completed_at: std::time::SystemTime::now(),
            };
        }
    }

    fn store_passphrase_in_keychain(&mut self, passphrase: &str) {
        if !self.state.settings.remember_passphrase_in_keychain {
            return;
        }

        match crate::secret_store::store_daemon_passphrase(passphrase) {
            Ok(()) => {
                self.secret_action = SecretActionState::Succeeded {
                    message: "saved passphrase to OS keychain".to_string(),
                    completed_at: std::time::SystemTime::now(),
                };
            }
            Err(message) => {
                self.secret_action = SecretActionState::Failed {
                    message,
                    completed_at: std::time::SystemTime::now(),
                };
            }
        }
        self.secret_snapshot = collect_secret_snapshot(true);
    }

    fn store_keychain_passphrase_from_input(&mut self) {
        if !self.state.settings.remember_passphrase_in_keychain {
            self.secret_action = SecretActionState::Failed {
                message: "enable keychain unlock before saving a passphrase".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let passphrase = Zeroizing::new(std::mem::take(&mut self.keychain_passphrase_input));
        if passphrase.is_empty() {
            self.secret_action = SecretActionState::Failed {
                message: "passphrase must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        self.store_passphrase_in_keychain(passphrase.as_str());
    }

    fn forget_keychain_passphrase(&mut self) {
        match crate::secret_store::forget_daemon_passphrase() {
            Ok(()) => {
                self.secret_action = SecretActionState::Succeeded {
                    message: "forgot stored daemon passphrase".to_string(),
                    completed_at: std::time::SystemTime::now(),
                };
            }
            Err(message) => {
                self.secret_action = SecretActionState::Failed {
                    message,
                    completed_at: std::time::SystemTime::now(),
                };
            }
        }
        self.secret_snapshot = collect_secret_snapshot(true);
    }

    fn store_bearer_token_from_input(&mut self) {
        let token = Zeroizing::new(std::mem::take(&mut self.bearer_token_input));
        if token.is_empty() {
            self.secret_action = SecretActionState::Failed {
                message: "token must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        match crate::secret_store::store_bearer_token(token.as_str()) {
            Ok(()) => {
                self.secret_action = SecretActionState::Succeeded {
                    message: "saved bearer token to OS keychain".to_string(),
                    completed_at: std::time::SystemTime::now(),
                };
            }
            Err(message) => {
                self.secret_action = SecretActionState::Failed {
                    message,
                    completed_at: std::time::SystemTime::now(),
                };
            }
        }
        self.secret_snapshot = collect_secret_snapshot(true);
    }

    fn forget_bearer_token(&mut self) {
        match crate::secret_store::forget_bearer_token() {
            Ok(()) => {
                self.secret_action = SecretActionState::Succeeded {
                    message: "forgot stored bearer token".to_string(),
                    completed_at: std::time::SystemTime::now(),
                };
            }
            Err(message) => {
                self.secret_action = SecretActionState::Failed {
                    message,
                    completed_at: std::time::SystemTime::now(),
                };
            }
        }
        self.secret_snapshot = collect_secret_snapshot(true);
    }

    fn repair_startup_on_login(&mut self) {
        match autostart::set_enabled(true) {
            Ok(()) => {
                self.state.settings.autostart_on_login = true;
                self.state.settings.save(&self.state.settings_path);
                self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
                self.secret_action = SecretActionState::Succeeded {
                    message: "repaired Solo Controls startup on login".to_string(),
                    completed_at: std::time::SystemTime::now(),
                };
            }
            Err(error) => {
                self.secret_action = SecretActionState::Failed {
                    message: format!("repair startup on login failed: {error}"),
                    completed_at: std::time::SystemTime::now(),
                };
            }
        }
    }

    fn set_keychain_remember_enabled(&mut self, enabled: bool) {
        if self.state.settings.remember_passphrase_in_keychain == enabled {
            return;
        }
        self.state.settings.remember_passphrase_in_keychain = enabled;
        self.state.settings.save(&self.state.settings_path);
        self.secret_action = SecretActionState::Succeeded {
            message: if enabled {
                "keychain unlock enabled".to_string()
            } else {
                "keychain unlock disabled".to_string()
            },
            completed_at: std::time::SystemTime::now(),
        };
        self.secret_snapshot = collect_secret_snapshot(enabled);
    }

    fn set_setup_wizard_completed(&mut self, completed: bool) {
        if self.state.settings.setup_wizard_completed == completed {
            return;
        }
        self.state.settings.setup_wizard_completed = completed;
        self.state.settings.save(&self.state.settings_path);
    }

    fn set_workspace_access_scope(&mut self, scope: WorkspaceAccessScope) {
        if self.state.settings.workspace_access_scope == scope {
            return;
        }
        self.state.settings.workspace_access_scope = scope;
        self.state.settings.save(&self.state.settings_path);
    }

    fn restrict_workspace_file_access_to_project_root(&mut self) {
        let Some(root) = self.project_snapshot.root.clone() else {
            self.workspace_file_access_message = Some("select a project root first".to_string());
            return;
        };
        if !root.is_dir() {
            self.workspace_file_access_message = Some(format!(
                "project root is not a directory: {}",
                root.display()
            ));
            return;
        }

        match set_workspace_file_access_allowed_roots(
            &self.workspace_file_access_snapshot.config_path,
            Some(vec![root.clone()]),
        ) {
            Ok(backup) => {
                self.workspace_file_access_restart_required = true;
                self.workspace_file_access_message = Some(format!(
                    "restricted imports to {}; restart Solo to apply. Backup: {}",
                    display_path(&root),
                    display_path(&backup)
                ));
            }
            Err(message) => {
                self.workspace_file_access_message = Some(message);
            }
        }
        self.workspace_file_access_snapshot =
            collect_workspace_file_access_snapshot(&self.setup_snapshot.data_dir);
    }

    fn clear_workspace_file_access_restriction(&mut self) {
        match set_workspace_file_access_allowed_roots(
            &self.workspace_file_access_snapshot.config_path,
            None,
        ) {
            Ok(backup) => {
                self.workspace_file_access_restart_required = true;
                self.workspace_file_access_message = Some(format!(
                    "allowed all local imports; restart Solo to apply. Backup: {}",
                    display_path(&backup)
                ));
            }
            Err(message) => {
                self.workspace_file_access_message = Some(message);
            }
        }
        self.workspace_file_access_snapshot =
            collect_workspace_file_access_snapshot(&self.setup_snapshot.data_dir);
    }

    fn save_project_root_from_input(&mut self) {
        let trimmed = self.project_root_input.trim();
        if trimmed.is_empty() {
            self.clear_project_root();
            return;
        }
        let root = PathBuf::from(trimmed);
        self.state.settings.project_root = Some(root);
        self.state.settings.save(&self.state.settings_path);
        self.project_init_confirmed = false;
        self.reset_project_docs_preview_and_import();
        self.reset_project_decision_results();
        self.reset_project_facts_results();
        self.refresh_project_dependent_snapshots();
    }

    fn clear_project_root(&mut self) {
        self.project_root_input.clear();
        self.state.settings.project_root = None;
        self.state.settings.save(&self.state.settings_path);
        self.project_init_confirmed = false;
        self.reset_project_docs_preview_and_import();
        self.reset_project_decision_results();
        self.reset_project_facts_results();
        self.refresh_project_dependent_snapshots();
    }

    fn use_current_dir_as_project_root(&mut self) {
        match std::env::current_dir() {
            Ok(root) => {
                self.project_root_input = display_path(&root);
                self.state.settings.project_root = Some(root);
                self.state.settings.save(&self.state.settings_path);
                self.project_init_confirmed = false;
                self.reset_project_docs_preview_and_import();
                self.reset_project_decision_results();
                self.reset_project_facts_results();
                self.refresh_project_dependent_snapshots();
            }
            Err(error) => {
                self.project_snapshot = ProjectMemorySnapshot {
                    root: None,
                    config_path: None,
                    state: ProjectMemoryState::InvalidConfig,
                    config: None,
                    detail: format!("current directory unavailable: {error}"),
                };
            }
        }
    }

    fn refresh_project_dependent_snapshots(&mut self) {
        self.project_snapshot =
            collect_project_memory_snapshot(self.state.settings.project_root.as_deref());
        self.tool_snapshot = collect_tool_snapshot(
            &self.state.settings.connected_tools,
            self.state.settings.project_root.as_deref(),
            COMMUNITY_LIBRARY_KEY,
        );
        if !can_offer_project_init(&self.project_snapshot) {
            self.project_init_confirmed = false;
        }
    }

    fn start_project_action(&mut self, kind: ProjectActionKind) {
        if self.project_action.is_running() {
            return;
        }
        if !can_run_project_action(kind, &self.project_snapshot, self.project_init_confirmed) {
            self.project_action = ProjectActionState::Failed {
                kind,
                message: project_action_unavailable_message(
                    kind,
                    &self.project_snapshot,
                    self.project_init_confirmed,
                ),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let Some(root) = self.project_snapshot.root.clone() else {
            self.project_action = ProjectActionState::Failed {
                kind,
                message: "select a project root first".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        };
        let solo_bin = if self.setup_snapshot.sibling_solo_exists {
            self.setup_snapshot.sibling_solo.clone()
        } else {
            PathBuf::from(if cfg!(windows) { "solo.exe" } else { "solo" })
        };
        let args = project_action_args(kind, &root);
        if kind == ProjectActionKind::Preview {
            self.reset_project_docs_preview_and_import();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.project_result_rx = Some(rx);
        self.project_action = ProjectActionState::Running {
            kind,
            root,
            started_at: std::time::Instant::now(),
        };

        std::thread::spawn(move || {
            let result = run_project_action(solo_bin, args, kind);
            let _ = tx.send(ProjectActionResult { result });
        });
    }

    fn start_project_docs_import(&mut self) {
        if self.project_docs_import.is_running() {
            return;
        }
        let Some(preview) = self.project_docs_preview.clone() else {
            self.project_docs_import = ProjectDocsImportState::Failed {
                message: "preview project docs before importing".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        };
        if preview.candidates.is_empty() {
            self.project_docs_import = ProjectDocsImportState::Failed {
                message: "preview found no project docs to import".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if !self.project_docs_import_confirmed {
            self.project_docs_import = ProjectDocsImportState::Failed {
                message: "confirm the project doc import first".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.project_docs_import = ProjectDocsImportState::Failed {
                message: "start Solo before importing project docs".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_documents_import_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let count = preview.candidates.len();
        let (tx, rx) = std::sync::mpsc::channel();
        self.project_docs_import_rx = Some(rx);
        self.project_docs_import = ProjectDocsImportState::Running {
            count,
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_project_docs_import(url, profile, preview).await;
            let _ = tx.send(ProjectDocsImportResult { result });
        });
    }

    fn start_project_decision_add(&mut self) {
        if self.project_decision_action.is_running() {
            return;
        }
        let decision = self.project_decision_text.trim().to_string();
        if decision.is_empty() {
            self.project_decision_action = ProjectDecisionActionState::Failed {
                verb: ProjectDecisionVerb::Add,
                message: "decision text must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.project_decision_action = ProjectDecisionActionState::Failed {
                verb: ProjectDecisionVerb::Add,
                message: "start Solo before saving project decisions".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        let Some((root, config)) = project_decision_context(&self.project_snapshot) else {
            self.project_decision_action = ProjectDecisionActionState::Failed {
                verb: ProjectDecisionVerb::Add,
                message: project_decision_unavailable_message(&self.project_snapshot),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        };

        let project = project_descriptor_json(config, root);
        let url = project_decision_add_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.project_decision_rx = Some(rx);
        self.project_decision_action = ProjectDecisionActionState::Adding {
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_project_decision_add(url, profile, project, decision).await;
            let _ = tx.send(ProjectDecisionResult { result });
        });
    }

    fn start_project_decision_search(&mut self) {
        if self.project_decision_action.is_running() {
            return;
        }
        let query = self.project_decision_query.trim().to_string();
        if query.is_empty() {
            self.project_decision_action = ProjectDecisionActionState::Failed {
                verb: ProjectDecisionVerb::Search,
                message: "decision search query must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.project_decision_action = ProjectDecisionActionState::Failed {
                verb: ProjectDecisionVerb::Search,
                message: "start Solo before searching project decisions".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        let Some((root, config)) = project_decision_context(&self.project_snapshot) else {
            self.project_decision_action = ProjectDecisionActionState::Failed {
                verb: ProjectDecisionVerb::Search,
                message: project_decision_unavailable_message(&self.project_snapshot),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        };

        let project = project_descriptor_json(config, root);
        let project_id = config.project_id.clone();
        let url = project_decision_search_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.project_decision_rx = Some(rx);
        self.project_decision_action = ProjectDecisionActionState::Searching {
            query: query.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result =
                run_daemon_project_decision_search(url, profile, project, query, project_id).await;
            let _ = tx.send(ProjectDecisionResult { result });
        });
    }

    fn start_project_facts_refresh(&mut self) {
        if self.project_facts.is_loading() {
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.project_facts = ProjectFactsState::Failed {
                subject: project_facts_subject_label(
                    &self.project_snapshot,
                    &self.project_fact_subject,
                ),
                message: "start Solo before loading project facts".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        let Some((root, config)) = project_decision_context(&self.project_snapshot) else {
            self.project_facts = ProjectFactsState::Failed {
                subject: project_facts_subject_label(
                    &self.project_snapshot,
                    &self.project_fact_subject,
                ),
                message: project_decision_unavailable_message(&self.project_snapshot),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        };
        let subject = project_facts_subject(config, &self.project_fact_subject);
        let project = project_descriptor_json(config, root);
        let url = project_facts_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.project_facts_rx = Some(rx);
        self.project_facts = ProjectFactsState::Loading {
            subject: subject.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_project_facts(url, profile, project, subject).await;
            let _ = tx.send(ProjectFactsResult { result });
        });
    }

    fn start_import_preview(&mut self) {
        if self.import_action.is_running() {
            return;
        }
        let path = PathBuf::from(self.import_path_input.trim());
        if self.import_path_input.trim().is_empty() || !path.exists() {
            self.import_action = ImportActionState::Failed {
                source: self.import_source,
                message: "choose an existing import file or folder".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        let solo_bin = if self.setup_snapshot.sibling_solo_exists {
            self.setup_snapshot.sibling_solo.clone()
        } else {
            PathBuf::from(if cfg!(windows) { "solo.exe" } else { "solo" })
        };
        let args = import_preview_args(self.import_source, &path, &self.backup_snapshot.data_dir);
        let source = self.import_source;
        self.import_commit_confirmed = false;
        self.import_commit = ImportCommitState::Idle;
        let (tx, rx) = std::sync::mpsc::channel();
        self.import_result_rx = Some(rx);
        self.import_action = ImportActionState::Running {
            source,
            path: path.clone(),
            started_at: std::time::Instant::now(),
        };

        std::thread::spawn(move || {
            let result = run_import_preview(solo_bin, args, source, path);
            let _ = tx.send(ImportActionResult { result });
        });
    }

    fn start_document_import(&mut self) {
        if self.import_commit.is_running() {
            return;
        }
        let path = PathBuf::from(self.import_path_input.trim());
        if self.import_path_input.trim().is_empty() || !path.exists() {
            self.import_commit = ImportCommitState::Failed {
                message: "choose an existing import file or folder".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if !self.import_commit_confirmed {
            self.import_commit = ImportCommitState::Failed {
                message: "confirm the import action first".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if !import_preview_matches(&self.import_action, self.import_source, &path) {
            self.import_commit = ImportCommitState::Failed {
                message: "preview the current source and path before importing".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.import_commit = ImportCommitState::Failed {
                message: "start Solo before importing documents".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_documents_import_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let request_path = path.display().to_string();
        let source = self.import_source.command().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.import_commit_rx = Some(rx);
        self.import_commit = ImportCommitState::Running {
            path,
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result =
                run_daemon_document_import(url, profile, source, request_path, true, 100).await;
            let _ = tx.send(ImportCommitResult { result });
        });
    }

    fn start_document_list_refresh(&mut self) {
        if self.document_list.is_loading() {
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.document_list = DocumentListState::Failed {
                message: "start Solo before loading documents".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_documents_list_url_from_status_url(&self.state.settings.status_url, 20);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.document_list_rx = Some(rx);
        self.document_list = DocumentListState::Loading {
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_document_list(url, profile).await;
            let _ = tx.send(DocumentListResult { result });
        });
    }

    fn start_document_search(&mut self) {
        if self.document_search.is_searching() {
            return;
        }

        let query = self.document_search_query.trim().to_string();
        if query.is_empty() {
            self.document_search = DocumentSearchState::Failed {
                query,
                message: "document search query must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.document_search = DocumentSearchState::Failed {
                query,
                message: "start Solo before searching documents".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_documents_search_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.document_search_rx = Some(rx);
        self.document_search = DocumentSearchState::Searching {
            query: query.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_document_search(url, profile, query).await;
            let _ = tx.send(DocumentSearchResult { result });
        });
    }

    fn start_document_inspect(&mut self, doc_id: &str) {
        if self.document_detail.is_loading() || self.document_forget.is_forgetting() {
            return;
        }
        let doc_id = doc_id.trim().to_string();
        if doc_id.is_empty() {
            self.document_detail = DocumentDetailState::Failed {
                doc_id,
                message: "document id must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.document_detail = DocumentDetailState::Failed {
                doc_id,
                message: "start Solo before inspecting documents".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        self.document_forget_confirmed = false;
        self.document_forget = DocumentForgetState::Idle;
        self.document_forget_rx = None;

        let url =
            memory_document_inspect_url_from_status_url(&self.state.settings.status_url, &doc_id);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.document_detail_rx = Some(rx);
        self.document_detail = DocumentDetailState::Loading {
            doc_id: doc_id.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_document_inspect(url, profile).await;
            let _ = tx.send(DocumentDetailResult { result });
        });
    }

    fn start_document_forget(&mut self, doc_id: &str) {
        if self.document_forget.is_forgetting() {
            return;
        }
        let doc_id = doc_id.trim().to_string();
        if doc_id.is_empty() {
            self.document_forget = DocumentForgetState::Failed {
                doc_id,
                message: "document id must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if !self.document_forget_confirmed {
            self.document_forget = DocumentForgetState::Failed {
                doc_id,
                message: "confirm the document forget action first".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.document_forget = DocumentForgetState::Failed {
                doc_id,
                message: "start Solo before forgetting documents".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url =
            memory_document_forget_url_from_status_url(&self.state.settings.status_url, &doc_id);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.document_forget_rx = Some(rx);
        self.document_forget = DocumentForgetState::Forgetting {
            doc_id: doc_id.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_document_forget(url, profile).await;
            let _ = tx.send(DocumentForgetResult { result });
        });
    }

    fn start_memory_remember(&mut self) {
        if self.memory_action.is_running() {
            return;
        }

        let content = self.memory_capture_text.trim_end().to_string();
        if content.trim().is_empty() {
            self.memory_action = MemoryActionState::Failed {
                verb: MemoryActionVerb::Remember,
                message: "memory text must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_action = MemoryActionState::Failed {
                verb: MemoryActionVerb::Remember,
                message: "start Solo before saving memory".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_result_rx = Some(rx);
        self.memory_action = MemoryActionState::Remembering {
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_memory_remember(url, profile, content).await;
            let _ = tx.send(MemoryActionResult { result });
        });
    }

    fn start_memory_search(&mut self) {
        if self.memory_action.is_running() {
            return;
        }

        let query = self.memory_search_query.trim().to_string();
        if query.is_empty() {
            self.memory_action = MemoryActionState::Failed {
                verb: MemoryActionVerb::Search,
                message: "search query must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_action = MemoryActionState::Failed {
                verb: MemoryActionVerb::Search,
                message: "start Solo before searching memory".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_search_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_result_rx = Some(rx);
        self.memory_action = MemoryActionState::Searching {
            query: query.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_memory_search(url, profile, query).await;
            let _ = tx.send(MemoryActionResult { result });
        });
    }

    fn start_memory_context(&mut self) {
        if self.memory_context.is_loading() {
            return;
        }

        let query = self.memory_context_query.trim().to_string();
        if query.is_empty() {
            self.memory_context = MemoryContextState::Failed {
                query,
                message: "context query must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_context = MemoryContextState::Failed {
                query,
                message: "start Solo before building memory context".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let subject = self.memory_context_subject.trim();
        let subject = if subject.is_empty() {
            None
        } else {
            Some(subject.to_string())
        };
        let url = memory_context_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_context_rx = Some(rx);
        self.memory_context = MemoryContextState::Loading {
            query: query.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_memory_context(url, profile, query, subject).await;
            let _ = tx.send(MemoryContextResult { result });
        });
    }

    fn start_memory_recent_refresh(&mut self) {
        if self.memory_recent.is_loading() {
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_recent = MemoryRecentState::Failed {
                message: "start Solo before loading recent memories".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_inbox_url_from_status_url(
            &self.state.settings.status_url,
            MEMORY_INBOX_RECENT_LIMIT,
        );
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_recent_rx = Some(rx);
        self.memory_recent = MemoryRecentState::Loading {
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_recent_memories(url, profile).await;
            let _ = tx.send(MemoryRecentResult { result });
        });
    }

    fn set_memory_review_state(&mut self, memory_id: &str, state: Option<&str>) {
        let reviewed_at_ms = current_time_ms();
        let changed = set_memory_review_state_cached(
            &mut self.state.settings,
            memory_id,
            state,
            reviewed_at_ms,
        );
        if changed {
            self.state.settings.save(&self.state.settings_path);
        }
        self.apply_memory_review_state_to_loaded(memory_id, state, reviewed_at_ms);
        self.start_memory_review_persist(memory_id.to_string(), state.map(str::to_string));
    }

    fn set_memory_review_states(&mut self, memory_ids: &[String], state: Option<&str>) {
        let reviewed_at_ms = current_time_ms();
        let mut changed = false;
        for memory_id in memory_ids {
            changed |= set_memory_review_state_cached(
                &mut self.state.settings,
                memory_id,
                state,
                reviewed_at_ms,
            );
        }
        if changed {
            self.state.settings.save(&self.state.settings_path);
        }
        for memory_id in memory_ids {
            self.apply_memory_review_state_to_loaded(memory_id, state, reviewed_at_ms);
            self.start_memory_review_persist(memory_id.clone(), state.map(str::to_string));
        }
    }

    fn apply_memory_review_state_to_loaded(
        &mut self,
        memory_id: &str,
        state: Option<&str>,
        reviewed_at_ms: i64,
    ) {
        let MemoryRecentState::Loaded { memories, .. } = &mut self.memory_recent else {
            return;
        };
        let Some(memory) = memories
            .iter_mut()
            .find(|memory| memory.memory_id == memory_id)
        else {
            return;
        };
        memory.review_state = state.map(str::to_string);
        memory.reviewed_at_ms = state.map(|_| reviewed_at_ms);
        memory.review_note = None;
    }

    fn start_memory_review_persist(&self, memory_id: String, state: Option<String>) {
        if self.current_health() != DaemonHealth::Healthy {
            return;
        }
        let url =
            memory_inbox_review_url_from_status_url(&self.state.settings.status_url, &memory_id);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        self.state.runtime_handle.spawn(async move {
            if let Err(e) = run_daemon_memory_review(url, profile, state).await {
                tracing::warn!(memory_id = %memory_id, error = %e, "Memory Inbox review persist failed");
            }
        });
    }

    fn start_memory_inspect(&mut self, memory_id: &str) {
        if self.memory_detail.is_loading() || self.memory_forget.is_forgetting() {
            return;
        }
        let memory_id = memory_id.trim().to_string();
        if memory_id.is_empty() {
            self.memory_detail = MemoryDetailState::Failed {
                memory_id,
                message: "memory id must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_detail = MemoryDetailState::Failed {
                memory_id,
                message: "start Solo before inspecting memory".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        self.memory_forget_confirmed = false;
        self.memory_forget = MemoryForgetState::Idle;
        self.memory_forget_rx = None;

        let url = memory_inspect_url_from_status_url(&self.state.settings.status_url, &memory_id);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_detail_rx = Some(rx);
        self.memory_detail = MemoryDetailState::Loading {
            memory_id: memory_id.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_memory_inspect(url, profile).await;
            let _ = tx.send(MemoryDetailResult { result });
        });
    }

    fn start_memory_update(&mut self, memory_id: &str) {
        if self.memory_update.is_updating() {
            return;
        }
        let memory_id = memory_id.trim().to_string();
        if self.memory_forget.is_forgetting() {
            self.memory_update = MemoryUpdateState::Failed {
                memory_id,
                message: "wait for the forget action to finish before updating".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        let content = self.memory_edit_text.trim().to_string();
        if memory_id.is_empty() {
            self.memory_update = MemoryUpdateState::Failed {
                memory_id,
                message: "memory id must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if content.is_empty() {
            self.memory_update = MemoryUpdateState::Failed {
                memory_id,
                message: "updated memory content must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_update = MemoryUpdateState::Failed {
                memory_id,
                message: "start Solo before updating memory".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_inspect_url_from_status_url(&self.state.settings.status_url, &memory_id);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_update_rx = Some(rx);
        self.memory_update = MemoryUpdateState::Updating {
            memory_id: memory_id.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_memory_update(url, profile, content).await;
            let _ = tx.send(MemoryUpdateResult { result });
        });
    }

    fn start_memory_forget(&mut self, memory_id: &str) {
        if self.memory_forget.is_forgetting() {
            return;
        }
        let memory_id = memory_id.trim().to_string();
        if self.memory_update.is_updating() {
            self.memory_forget = MemoryForgetState::Failed {
                memory_id,
                message: "wait for the update action to finish before forgetting".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if memory_id.is_empty() {
            self.memory_forget = MemoryForgetState::Failed {
                memory_id,
                message: "memory id must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if !self.memory_forget_confirmed {
            self.memory_forget = MemoryForgetState::Failed {
                memory_id,
                message: "confirm the forget action first".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_forget = MemoryForgetState::Failed {
                memory_id,
                message: "start Solo before forgetting memory".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_forget_url_from_status_url(&self.state.settings.status_url, &memory_id);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_forget_rx = Some(rx);
        self.memory_forget = MemoryForgetState::Forgetting {
            memory_id: memory_id.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_memory_forget(url, profile, memory_id).await;
            let _ = tx.send(MemoryForgetResult { result });
        });
    }

    fn start_memory_contradictions_refresh(&mut self) {
        if self.memory_contradictions.is_loading() {
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_contradictions = MemoryContradictionState::Failed {
                message: "start Solo before loading contradictions".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_contradictions_url_from_status_url(&self.state.settings.status_url, 10);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_contradictions_rx = Some(rx);
        self.memory_contradictions = MemoryContradictionState::Loading {
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_memory_contradictions(url, profile).await;
            let _ = tx.send(MemoryContradictionResult { result });
        });
    }

    fn start_memory_contradiction_resolve(
        &mut self,
        a_id: String,
        b_id: String,
        kind: String,
        status: String,
        winning_triple_id: Option<String>,
    ) {
        if self.memory_contradiction_resolve.is_resolving() {
            return;
        }
        let label = format!("{a_id} / {b_id} ({kind})");
        if a_id.trim().is_empty() || b_id.trim().is_empty() || kind.trim().is_empty() {
            self.memory_contradiction_resolve = MemoryContradictionResolveState::Failed {
                label,
                message: "contradiction ids and kind must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if self.current_health() != DaemonHealth::Healthy {
            self.memory_contradiction_resolve = MemoryContradictionResolveState::Failed {
                label,
                message: "start Solo before resolving contradictions".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let url = memory_contradiction_resolve_url_from_status_url(&self.state.settings.status_url);
        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let note = contradiction_resolution_note(&status, winning_triple_id.as_deref());
        let request = ContradictionResolveRequest {
            a_id,
            b_id,
            kind,
            status,
            resolution_note: note,
            winning_triple_id,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.memory_contradiction_resolve_rx = Some(rx);
        self.memory_contradiction_resolve = MemoryContradictionResolveState::Resolving {
            label,
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_memory_contradiction_resolve(url, profile, request).await;
            let _ = tx.send(MemoryContradictionResolveResult { result });
        });
    }

    fn supervisor_state(&self) -> SupervisorState {
        match self.state.daemon_handle.try_lock() {
            Ok(h) => h.state.clone(),
            Err(_) if self.daemon_started => SupervisorState::Running,
            Err(_) => SupervisorState::Starting,
        }
    }

    fn sync_supervisor_state(&mut self) {
        let supervisor_state = self.supervisor_state();
        match &supervisor_state {
            SupervisorState::Locked => {
                self.daemon_started = false;
                self.clear_pending_keychain_passphrase(None);
            }
            SupervisorState::StartupFailed(msg) => {
                self.daemon_started = false;
                let text = format!(
                    "Daemon did not start. Check the passphrase and settings, then try again. Last error: {msg}"
                );
                if self.passphrase_error.as_deref() != Some(text.as_str()) {
                    self.passphrase_error = Some(text);
                }
                self.clear_pending_keychain_passphrase(Some(
                    "passphrase was not saved because daemon start failed",
                ));
            }
            SupervisorState::Stopped if !self.quitting => {
                self.daemon_started = false;
                self.clear_pending_keychain_passphrase(Some(
                    "passphrase was not saved because daemon stopped",
                ));
            }
            SupervisorState::Starting
            | SupervisorState::Restarting
            | SupervisorState::Crashed(_)
            | SupervisorState::Stopped => {
                self.daemon_started = true;
            }
            SupervisorState::Running => {
                self.daemon_started = true;
                self.store_pending_keychain_passphrase();
            }
        }
    }

    /// Read the current daemon-health from the status state. Held
    /// briefly; we only need the snapshot.
    fn current_health(&self) -> DaemonHealth {
        match self.supervisor_state() {
            SupervisorState::Locked => return DaemonHealth::Starting,
            SupervisorState::StartupFailed(_) | SupervisorState::Stopped => {
                return DaemonHealth::Down;
            }
            SupervisorState::Starting | SupervisorState::Restarting => {
                return DaemonHealth::Starting;
            }
            SupervisorState::Running | SupervisorState::Crashed(_) => {}
        }
        // try_lock is safe: failing to lock just returns the previous
        // snapshot, which is fine for a per-frame UI refresh.
        match self.state.status_state.try_lock() {
            Ok(s) => s.health,
            Err(_) => self.last_health,
        }
    }

    fn handle_menu_event(&mut self, ctx: &Context, id: &str) {
        match id {
            id if let Some(route) = tray::desktop_route_for_menu_id(id) => {
                tray::open_solo_desktop_route_async(
                    self.state.settings.solo_web_url.clone(),
                    route,
                );
            }
            tray::MENU_SHOW_LOGS => {
                self.window_visible = true;
                self.active_tab = MainTab::Logs;
                // Un-minimise (no-op if already restored) and focus.
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            tray::MENU_OPEN_WEB => {
                tray::open_solo_web_async(self.state.settings.solo_web_url.clone());
            }
            tray::MENU_OPEN_DATA_DIR => {
                open_path_async(tray::resolve_data_dir(), "data dir");
            }
            tray::MENU_RESTART_DAEMON => {
                self.request_daemon_restart();
            }
            tray::MENU_TOGGLE_AUTOSTART => {
                let new_state = !self.state.settings.autostart_on_login;
                match autostart::set_enabled(new_state) {
                    Ok(()) => {
                        self.state.settings.autostart_on_login = new_state;
                        self.state.settings.save(&self.state.settings_path);
                        tracing::info!(enabled = new_state, "autostart toggled");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "toggle autostart failed");
                    }
                }
            }
            tray::MENU_TOGGLE_NOTIFICATIONS => {
                self.state.settings.notifications_enabled =
                    !self.state.settings.notifications_enabled;
                self.state.settings.save(&self.state.settings_path);
                let new_state = self.state.settings.notifications_enabled;
                let notifier = self.state.notifier.clone();
                self.state.runtime_handle.spawn(async move {
                    notifier.lock().await.set_enabled(new_state);
                });
                tracing::info!(enabled = new_state, "notifications toggled");
            }
            tray::MENU_TOGGLE_THEME => {
                self.state.settings.theme = match self.state.settings.theme {
                    Theme::Dark => Theme::Light,
                    Theme::Light | Theme::System => Theme::Dark,
                };
                self.state.settings.save(&self.state.settings_path);
                apply_theme(ctx, self.state.settings.theme);
                tracing::info!(theme = ?self.state.settings.theme, "theme toggled");
            }
            tray::MENU_QUIT => {
                // Unreachable in normal operation: the menu dispatcher
                // receives Quit directly, asks the supervisor to stop
                // the daemon, waits briefly, then exits. Keep this as
                // a fallback in case an event was forwarded manually.
                tracing::info!("quit requested via eframe fallback path");
                self.quitting = true;
                let handle = self.state.daemon_handle.clone();
                self.state.runtime_handle.spawn(async move {
                    handle.lock().await.request_quit();
                });
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            other => {
                tracing::debug!(menu_id = other, "unrecognised menu event id");
            }
        }
    }

    fn draw_first_run_init_controls(&mut self, ui: &mut egui::Ui) {
        let dark_mode = ui.visuals().dark_mode;
        ui.label(RichText::new("Create encrypted Solo memory").strong());
        ui.label(
            RichText::new("Choose the passphrase you will use to unlock Solo on this computer.")
                .color(muted_text_color(dark_mode)),
        );
        ui.add_space(6.0);

        let response = ui.add_sized(
            [320.0, 28.0],
            egui::TextEdit::singleline(&mut self.passphrase_input)
                .password(true)
                .hint_text("Passphrase"),
        );
        if self.passphrase_input.is_empty() {
            response.request_focus();
        }
        ui.add_space(4.0);
        ui.add_sized(
            [320.0, 28.0],
            egui::TextEdit::singleline(&mut self.init_passphrase_confirm)
                .password(true)
                .hint_text("Confirm passphrase"),
        );
        ui.add_space(4.0);
        ui.add_sized(
            [320.0, 28.0],
            egui::TextEdit::singleline(&mut self.init_first_name)
                .hint_text("First name (optional)"),
        );
        ui.add_space(8.0);

        let mut remember = self.state.settings.remember_passphrase_in_keychain;
        if ui
            .checkbox(&mut remember, "Remember in OS keychain")
            .changed()
        {
            self.set_keychain_remember_enabled(remember);
        }
        ui.add_space(4.0);

        let running = self.first_run_init.is_running();
        let submit = ui
            .add_enabled(!running, egui::Button::new("Create Solo memory"))
            .clicked()
            || (!running && ui.input(|i| i.key_pressed(Key::Enter)));
        if submit {
            self.start_first_run_init();
        }

        if let Some(err) = &self.passphrase_error {
            ui.add_space(8.0);
            ui.label(RichText::new(err).color(error_color(dark_mode)));
        }
        ui.add_space(8.0);
        ui.label(RichText::new(first_run_init_status(&self.first_run_init)).weak());
        ui.label(RichText::new(secret_action_status(&self.secret_action)).weak());
    }

    fn draw_passphrase_controls(&mut self, ui: &mut egui::Ui) {
        let status = self.status_snapshot();
        let supervisor = self.supervisor_state();
        let health = self.current_health();

        let response = ui.add_sized(
            [320.0, 28.0],
            egui::TextEdit::singleline(&mut self.passphrase_input)
                .password(true)
                .hint_text("Passphrase"),
        );
        response.request_focus();
        ui.add_space(8.0);

        let mut remember = self.state.settings.remember_passphrase_in_keychain;
        if ui
            .checkbox(&mut remember, "Remember in OS keychain")
            .changed()
        {
            self.set_keychain_remember_enabled(remember);
        }
        ui.add_space(8.0);

        egui::Grid::new("passphrase_runtime_status")
            .num_columns(3)
            .spacing([14.0, 5.0])
            .show(ui, |ui| {
                let (state, tone, detail) = keychain_passphrase_status(
                    &self.secret_snapshot,
                    self.state.settings.remember_passphrase_in_keychain,
                    self.pending_keychain_passphrase.is_some(),
                );
                render_state_row(ui, "Keychain", &state, tone, &detail);

                let (state, tone, detail) = daemon_lifecycle_status(Some(&supervisor), health);
                render_state_row(ui, "Daemon", &state, tone, &detail);

                let (state, tone, detail) = embedder_runtime_status(
                    status.last_payload.as_ref(),
                    health,
                    status.last_error.as_deref(),
                );
                render_state_row(ui, "Embedder", &state, tone, &detail);

                let (state, tone, detail) =
                    steward_runtime_status(status.last_payload.as_ref(), health);
                render_state_row(ui, "Steward", &state, tone, &detail);
            });
        ui.add_space(8.0);

        let can_start = matches!(
            supervisor,
            SupervisorState::Locked | SupervisorState::StartupFailed(_) | SupervisorState::Stopped
        );
        let submit = ui
            .add_enabled(can_start, egui::Button::new("Start Solo"))
            .clicked()
            || (can_start && ui.input(|i| i.key_pressed(Key::Enter)));
        let start_from_keychain = ui
            .add_enabled(
                can_start && self.state.settings.remember_passphrase_in_keychain,
                egui::Button::new("Start from keychain"),
            )
            .clicked();
        if submit {
            if self.passphrase_input.is_empty() {
                self.passphrase_error = Some("Passphrase must not be empty.".to_string());
            } else {
                let passphrase = Zeroizing::new(std::mem::take(&mut self.passphrase_input));
                self.queue_keychain_passphrase(passphrase.as_str());
                self.start_daemon(passphrase);
            }
        }
        if start_from_keychain {
            self.start_daemon_from_keychain();
        }
        if ui
            .add_enabled(
                self.state.settings.remember_passphrase_in_keychain,
                egui::Button::new("Refresh keychain"),
            )
            .clicked()
        {
            self.secret_snapshot =
                collect_secret_snapshot(self.state.settings.remember_passphrase_in_keychain);
        }

        if !self.state.settings.remember_passphrase_in_keychain {
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Keychain unlock is off; Solo will need this passphrase after each restart.",
                )
                .color(muted_text_color(ui.visuals().dark_mode)),
            );
        }

        if let Some(err) = &self.passphrase_error {
            ui.add_space(8.0);
            ui.label(RichText::new(err).color(error_color(ui.visuals().dark_mode)));
        }
        ui.add_space(8.0);
        ui.label(RichText::new(secret_action_status(&self.secret_action)).weak());
    }

    fn refresh_detected_info_if_needed(&mut self) {
        if self.last_detection_refresh.elapsed() < std::time::Duration::from_secs(10) {
            return;
        }
        self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
        self.tool_snapshot = collect_tool_snapshot(
            &self.state.settings.connected_tools,
            self.state.settings.project_root.as_deref(),
            COMMUNITY_LIBRARY_KEY,
        );
        self.backup_snapshot = collect_backup_snapshot();
        self.library_snapshot = collect_library_snapshot(&self.backup_snapshot.data_dir);
        self.project_snapshot =
            collect_project_memory_snapshot(self.state.settings.project_root.as_deref());
        self.workspace_file_access_snapshot =
            collect_workspace_file_access_snapshot(&self.setup_snapshot.data_dir);
        self.last_detection_refresh = std::time::Instant::now();
    }

    fn refresh_detected_snapshots_now(&mut self) {
        self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
        self.tool_snapshot = collect_tool_snapshot(
            &self.state.settings.connected_tools,
            self.state.settings.project_root.as_deref(),
            COMMUNITY_LIBRARY_KEY,
        );
        self.backup_snapshot = collect_backup_snapshot();
        self.library_snapshot = collect_library_snapshot(&self.backup_snapshot.data_dir);
        self.project_snapshot =
            collect_project_memory_snapshot(self.state.settings.project_root.as_deref());
        self.workspace_file_access_snapshot =
            collect_workspace_file_access_snapshot(&self.setup_snapshot.data_dir);
        self.secret_snapshot = collect_secret_snapshot(true);
        self.last_detection_refresh = std::time::Instant::now();
    }

    fn reset_project_docs_preview_and_import(&mut self) {
        self.project_docs_preview = None;
        self.reset_project_docs_import_results();
    }

    fn reset_project_docs_import_results(&mut self) {
        self.project_docs_import_confirmed = false;
        self.project_docs_import = ProjectDocsImportState::Idle;
        self.project_docs_import_rx = None;
    }

    fn reset_project_decision_results(&mut self) {
        self.project_decision_text.clear();
        self.project_decision_query.clear();
        self.project_decision_action = ProjectDecisionActionState::Idle;
        self.project_decision_rx = None;
    }

    fn reset_project_facts_results(&mut self) {
        self.project_fact_subject.clear();
        self.project_facts = ProjectFactsState::Idle;
        self.project_facts_rx = None;
    }

    fn daemon_snapshot(&self) -> Option<DaemonSnapshot> {
        self.state
            .daemon_handle
            .try_lock()
            .ok()
            .map(|h| DaemonSnapshot {
                state: h.state.clone(),
                pid: h.pid,
                running: h.running,
                supervisor_exited: h.supervisor_exited,
            })
    }

    fn status_snapshot(&self) -> StatusSnapshot {
        match self.state.status_state.try_lock() {
            Ok(s) => StatusSnapshot {
                health: s.health,
                last_payload: s.last_payload.clone(),
                last_ok_at: s.last_ok_at,
                last_error: s.last_error.clone(),
            },
            Err(_) => StatusSnapshot {
                health: self.last_health,
                last_payload: None,
                last_ok_at: None,
                last_error: Some("status state locked; will refresh".to_string()),
            },
        }
    }

    fn poll_first_run_init_result(&mut self) {
        let Some(rx) = self.first_run_init_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.first_run_init_rx = None;
                match result.result {
                    Ok(success) => {
                        let alias_note = if success.user_alias_set {
                            "; saved first-name alias"
                        } else {
                            ""
                        };
                        self.first_run_init = FirstRunInitState::Succeeded {
                            message: format!(
                                "created encrypted Solo memory in {} (config {}; schema {schema}){alias_note}",
                                display_path(&success.data_dir),
                                display_path(&success.config_path),
                                schema = success.schema_version
                            ),
                            completed_at: std::time::SystemTime::now(),
                        };
                        self.passphrase_error = None;
                        self.refresh_detected_snapshots_now();
                        self.queue_keychain_passphrase(success.passphrase.as_str());
                        self.start_daemon(success.passphrase);
                    }
                    Err(message) => {
                        self.passphrase_error = Some(message.clone());
                        self.first_run_init = FirstRunInitState::Failed {
                            message,
                            completed_at: std::time::SystemTime::now(),
                        };
                        self.refresh_detected_snapshots_now();
                    }
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.first_run_init_rx = None;
                if matches!(self.first_run_init, FirstRunInitState::Running { .. }) {
                    self.first_run_init = FirstRunInitState::Failed {
                        message: "first-run init worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                    self.refresh_detected_snapshots_now();
                }
            }
        }
    }

    fn poll_setup_result(&mut self) {
        let Some(rx) = self.setup_result_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.setup_result_rx = None;
                let (target, verb) = self
                    .setup_action
                    .target_and_verb()
                    .unwrap_or((SetupTarget::CodexUser, SetupActionVerb::Verify));
                self.persist_tool_action_status(target, verb, &result.result);
                let status = self.status_snapshot();
                let setup_succeeded = result.result.is_ok();
                self.setup_action = match result.result {
                    Ok(success) => SetupActionState::Succeeded {
                        target,
                        verb,
                        message: success.message,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => SetupActionState::Failed {
                        target,
                        verb,
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
                self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
                self.tool_snapshot = collect_tool_snapshot(
                    &self.state.settings.connected_tools,
                    self.state.settings.project_root.as_deref(),
                    COMMUNITY_LIBRARY_KEY,
                );
                self.library_snapshot = collect_library_snapshot(&self.setup_snapshot.data_dir);
                self.last_detection_refresh = std::time::Instant::now();
                if setup_succeeded
                    && status.health == DaemonHealth::Healthy
                    && !self.mcp_probe.is_running()
                {
                    self.start_mcp_probe();
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.setup_result_rx = None;
                if matches!(self.setup_action, SetupActionState::Running { .. }) {
                    let (target, verb) = self
                        .setup_action
                        .target_and_verb()
                        .unwrap_or((SetupTarget::CodexUser, SetupActionVerb::Verify));
                    self.setup_action = SetupActionState::Failed {
                        target,
                        verb,
                        message: "setup worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn persist_tool_action_status(
        &mut self,
        target: SetupTarget,
        verb: SetupActionVerb,
        result: &Result<SetupActionSuccess, String>,
    ) {
        let live_row = inspect_tool_config(target, self.state.settings.project_root.as_deref());
        let daemon_default_profile = "Community Memory Library".to_string();
        let (status, detail, verification) = match result {
            Ok(success) => (
                match verb {
                    SetupActionVerb::Apply => "applied_verified",
                    SetupActionVerb::Verify => "verified",
                }
                .to_string(),
                success.message.clone(),
                success.verification.clone(),
            ),
            Err(message) => {
                let status = match live_row.state {
                    ToolConfigState::NeedsSetup => "needs_setup",
                    ToolConfigState::NeedsRepair => "needs_repair",
                    ToolConfigState::Unknown => "unknown",
                    ToolConfigState::Verified => "failed",
                };
                (
                    status.to_string(),
                    message.clone(),
                    tool_verification_from_row(&live_row),
                )
            }
        };
        let resolved_profile =
            probe_profile_for_route(&verification.profile_route, &daemon_default_profile);
        let history_profile = resolved_profile
            .clone()
            .unwrap_or_else(|| COMMUNITY_LIBRARY_KEY.to_string());
        let history_key = connected_tool_status_key(target, &history_profile);
        self.state.settings.connected_tools.insert(
            history_key,
            ConnectedToolLastStatus {
                status,
                detail,
                config_path: verification.config_path,
                config_state: Some(verification.state.label().to_string()),
                transport: Some(verification.transport.label().to_string()),
                profile_route: Some(verification.profile_route.label()),
                resolved_profile,
                updated_at_ms: Some(current_time_ms()),
            },
        );
        self.state.settings.save(&self.state.settings_path);
    }

    fn setup_solo_bin(&self) -> PathBuf {
        if self.setup_snapshot.sibling_solo_exists {
            self.setup_snapshot.sibling_solo.clone()
        } else {
            PathBuf::from(if cfg!(windows) { "solo.exe" } else { "solo" })
        }
    }

    fn start_setup_client_action(&mut self, target: SetupTarget, verb: SetupActionVerb) {
        if self.setup_action.is_running() {
            return;
        }
        let project_root = self.state.settings.project_root.clone();
        if target == SetupTarget::CodexProject {
            match project_root.as_deref() {
                Some(root) if root.is_dir() => {}
                Some(root) => {
                    self.setup_action = SetupActionState::Failed {
                        target,
                        verb,
                        message: format!(
                            "project root is missing; update Projects before running Codex project setup: {}",
                            display_path(root)
                        ),
                        completed_at: std::time::SystemTime::now(),
                    };
                    return;
                }
                None => {
                    self.setup_action = SetupActionState::Failed {
                        target,
                        verb,
                        message:
                            "select a project root in Projects before running Codex project setup"
                                .to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                    return;
                }
            }
        }

        let solo_bin = self.setup_solo_bin();
        let mcp_url = mcp_url_from_status_url(&self.state.settings.status_url);
        let args = match verb {
            SetupActionVerb::Apply => target.apply_args(&mcp_url, project_root.as_deref()),
            SetupActionVerb::Verify => target.verify_args(project_root.as_deref()),
        };
        let expected_profile_route = match verb {
            SetupActionVerb::Apply => ExpectedToolProfileRoute::DaemonDefault,
            SetupActionVerb::Verify => ExpectedToolProfileRoute::Any,
        };

        let (tx, rx) = std::sync::mpsc::channel();
        self.setup_result_rx = Some(rx);
        self.setup_action = SetupActionState::Running {
            target,
            verb,
            started_at: std::time::Instant::now(),
        };

        std::thread::spawn(move || {
            let result = run_setup_client_action(
                solo_bin,
                args,
                target,
                verb,
                expected_profile_route,
                project_root,
            );
            let _ = tx.send(SetupActionResult { result });
        });
    }

    fn poll_mcp_probe_result(&mut self) {
        let Some(rx) = self.mcp_probe_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.mcp_probe_rx = None;
                self.mcp_probe = match result.result {
                    Ok(summary) => McpProbeState::Succeeded {
                        summary,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => McpProbeState::Failed {
                        profile: mcp_probe_profile_label(&self.mcp_probe),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.mcp_probe_rx = None;
                if self.mcp_probe.is_running() {
                    self.mcp_probe = McpProbeState::Failed {
                        profile: mcp_probe_profile_label(&self.mcp_probe),
                        message: "MCP probe worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn start_mcp_probe(&mut self) {
        if self.mcp_probe.is_running() {
            return;
        }

        let profile = COMMUNITY_LIBRARY_KEY.to_string();
        let url = mcp_url_from_status_url(&self.state.settings.status_url);
        let (tx, rx) = std::sync::mpsc::channel();
        self.mcp_probe_rx = Some(rx);
        self.mcp_probe = McpProbeState::Running {
            profile: profile.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_mcp_probe(url, profile).await;
            let _ = tx.send(McpProbeResult { result });
        });
    }

    fn poll_client_check_result(&mut self) {
        let Some(rx) = self.client_check_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.client_check_rx = None;
                self.client_check = match result.result {
                    Ok(success) => ClientCheckState::Succeeded {
                        target: result.target,
                        summary: success.summary,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => ClientCheckState::Failed {
                        target: result.target,
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.client_check_rx = None;
                if let ClientCheckState::Running { target, .. } = self.client_check {
                    self.client_check = ClientCheckState::Failed {
                        target,
                        message: "client check worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_setup_doctor_result(&mut self) {
        let Some(rx) = self.setup_doctor_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.setup_doctor_rx = None;
                self.setup_doctor = match result.result {
                    Ok(report) => SetupDoctorState::Succeeded {
                        target: result.target,
                        report,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => SetupDoctorState::Failed {
                        target: result.target,
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
                self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
                self.tool_snapshot = collect_tool_snapshot(
                    &self.state.settings.connected_tools,
                    self.state.settings.project_root.as_deref(),
                    COMMUNITY_LIBRARY_KEY,
                );
                self.last_detection_refresh = std::time::Instant::now();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.setup_doctor_rx = None;
                if let SetupDoctorState::Running { target, .. } = self.setup_doctor {
                    self.setup_doctor = SetupDoctorState::Failed {
                        target,
                        message: "doctor worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn start_client_check(&mut self, target: SetupTarget) {
        if self.client_check.is_running() {
            return;
        }
        if !target.supports_automated_client_check() {
            self.client_check = ClientCheckState::Failed {
                target,
                message: "this client check is manual in Solo for now".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        if target == SetupTarget::CodexProject {
            match self.state.settings.project_root.as_deref() {
                Some(root) if root.is_dir() => {}
                Some(root) => {
                    self.client_check = ClientCheckState::Failed {
                        target,
                        message: format!(
                            "project root is missing; update Projects first: {}",
                            display_path(root)
                        ),
                        completed_at: std::time::SystemTime::now(),
                    };
                    return;
                }
                None => {
                    self.client_check = ClientCheckState::Failed {
                        target,
                        message: "select a project root in Projects before running the Codex project check".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                    return;
                }
            }
        }

        let project_root = self.state.settings.project_root.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.client_check_rx = Some(rx);
        self.client_check = ClientCheckState::Running {
            target,
            started_at: std::time::Instant::now(),
        };

        std::thread::spawn(move || {
            let result = run_client_check(target, project_root);
            let _ = tx.send(ClientCheckResult { target, result });
        });
    }

    fn start_setup_client_doctor(&mut self, target: SetupTarget) {
        if self.setup_doctor.is_running() {
            return;
        }
        if !self.setup_snapshot.solo_command_available {
            self.setup_doctor = SetupDoctorState::Failed {
                target,
                message: "install solo beside solo-tray or put solo on PATH to run Doctor"
                    .to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        let project_root = self.state.settings.project_root.clone();
        if target == SetupTarget::CodexProject {
            match project_root.as_deref() {
                Some(root) if root.is_dir() => {}
                Some(root) => {
                    self.setup_doctor = SetupDoctorState::Failed {
                        target,
                        message: format!(
                            "project root is missing; update Projects before running Codex project Doctor: {}",
                            display_path(root)
                        ),
                        completed_at: std::time::SystemTime::now(),
                    };
                    return;
                }
                None => {
                    self.setup_doctor = SetupDoctorState::Failed {
                        target,
                        message:
                            "select a project root in Projects before running Codex project Doctor"
                                .to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                    return;
                }
            }
        }

        let solo_bin = self.setup_solo_bin();
        let mcp_url = mcp_url_from_status_url(&self.state.settings.status_url);
        let args = target.doctor_args(&mcp_url, project_root.as_deref());
        let (tx, rx) = std::sync::mpsc::channel();
        self.setup_doctor_rx = Some(rx);
        self.setup_doctor = SetupDoctorState::Running {
            target,
            started_at: std::time::Instant::now(),
        };

        std::thread::spawn(move || {
            let result = run_setup_client_doctor(solo_bin, args);
            let _ = tx.send(SetupDoctorResult { target, result });
        });
    }

    fn poll_backup_result(&mut self) {
        let Some(rx) = self.backup_result_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.backup_result_rx = None;
                self.backup_action = match result.result {
                    Ok(success) => {
                        self.backup_snapshot = collect_backup_snapshot();
                        self.library_snapshot =
                            collect_library_snapshot(&self.backup_snapshot.data_dir);
                        self.last_detection_refresh = std::time::Instant::now();
                        BackupActionState::Succeeded {
                            path: success.path,
                            elapsed_ms: success.elapsed_ms,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => BackupActionState::Failed {
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.backup_result_rx = None;
                if self.backup_action.is_running() {
                    self.backup_action = BackupActionState::Failed {
                        message: "backup worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_ollama_migration_result(&mut self) {
        let Some(rx) = self.ollama_migration_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.ollama_migration_rx = None;
                let restart_passphrase = self.ollama_migration_restart_passphrase.take();
                self.ollama_migration = match result.result {
                    Ok(success) => OllamaMigrationState::Succeeded {
                        model: result.model,
                        summary: success.summary,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => OllamaMigrationState::Failed {
                        model: result.model,
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
                self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
                self.backup_snapshot = collect_backup_snapshot();
                self.library_snapshot = collect_library_snapshot(&self.backup_snapshot.data_dir);
                self.last_detection_refresh = std::time::Instant::now();
                if let Some(passphrase) = restart_passphrase {
                    self.start_daemon(passphrase);
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.ollama_migration_rx = None;
                self.ollama_migration_restart_passphrase.take();
                if self.ollama_migration.is_running() {
                    self.ollama_migration = OllamaMigrationState::Failed {
                        model: self.ollama_migration_model.trim().to_string(),
                        message: "embedder migration worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_project_result(&mut self) {
        let Some(rx) = self.project_result_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.project_result_rx = None;
                self.project_action = match result.result {
                    Ok(success) => {
                        self.refresh_project_dependent_snapshots();
                        if success.kind == ProjectActionKind::Init {
                            self.project_init_confirmed = false;
                        }
                        let output = if success.kind == ProjectActionKind::Preview {
                            match parse_project_docs_preview(&success.output) {
                                Ok(preview) => {
                                    let output = format_project_docs_preview(&preview);
                                    self.project_docs_preview = Some(preview);
                                    self.reset_project_docs_import_results();
                                    output
                                }
                                Err(error) => {
                                    self.project_docs_preview = None;
                                    format!(
                                        "Could not parse structured project preview: {error}\n\n{}",
                                        success.output
                                    )
                                }
                            }
                        } else {
                            self.project_docs_preview = None;
                            self.reset_project_docs_import_results();
                            success.output
                        };
                        ProjectActionState::Succeeded {
                            kind: success.kind,
                            message: success.message,
                            output,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => ProjectActionState::Failed {
                        kind: project_action_kind(&self.project_action)
                            .unwrap_or(ProjectActionKind::Preview),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.project_result_rx = None;
                if self.project_action.is_running() {
                    self.project_action = ProjectActionState::Failed {
                        kind: project_action_kind(&self.project_action)
                            .unwrap_or(ProjectActionKind::Preview),
                        message: "project action worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_project_docs_import_result(&mut self) {
        let Some(rx) = self.project_docs_import_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.project_docs_import_rx = None;
                self.project_docs_import_confirmed = false;
                self.project_docs_import = match result.result {
                    Ok(report) => {
                        self.document_search = DocumentSearchState::Idle;
                        self.document_search_rx = None;
                        if self.current_health() == DaemonHealth::Healthy {
                            self.start_document_list_refresh();
                        }
                        ProjectDocsImportState::Succeeded {
                            report,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => ProjectDocsImportState::Failed {
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.project_docs_import_rx = None;
                if self.project_docs_import.is_running() {
                    self.project_docs_import = ProjectDocsImportState::Failed {
                        message: "project docs import worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_project_decision_result(&mut self) {
        let Some(rx) = self.project_decision_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.project_decision_rx = None;
                self.project_decision_action = match result.result {
                    Ok(ProjectDecisionSuccess::Added { memory_id }) => {
                        self.project_decision_text.clear();
                        ProjectDecisionActionState::Added {
                            memory_id,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Ok(ProjectDecisionSuccess::Search { query, hits }) => {
                        ProjectDecisionActionState::SearchSucceeded {
                            query,
                            hits,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => ProjectDecisionActionState::Failed {
                        verb: project_decision_verb(&self.project_decision_action)
                            .unwrap_or(ProjectDecisionVerb::Search),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.project_decision_rx = None;
                if self.project_decision_action.is_running() {
                    self.project_decision_action = ProjectDecisionActionState::Failed {
                        verb: project_decision_verb(&self.project_decision_action)
                            .unwrap_or(ProjectDecisionVerb::Search),
                        message: "project decision worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_project_facts_result(&mut self) {
        let Some(rx) = self.project_facts_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.project_facts_rx = None;
                self.project_facts = match result.result {
                    Ok(success) => ProjectFactsState::Loaded {
                        subject: success.subject,
                        facts: success.facts,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => ProjectFactsState::Failed {
                        subject: project_facts_state_subject(&self.project_facts),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.project_facts_rx = None;
                if self.project_facts.is_loading() {
                    self.project_facts = ProjectFactsState::Failed {
                        subject: project_facts_state_subject(&self.project_facts),
                        message: "project facts worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_import_result(&mut self) {
        let Some(rx) = self.import_result_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.import_result_rx = None;
                self.import_action = match result.result {
                    Ok(success) => ImportActionState::Succeeded {
                        source: success.source,
                        path: success.path,
                        message: success.message,
                        output: success.output,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => ImportActionState::Failed {
                        source: import_action_source(&self.import_action)
                            .unwrap_or(self.import_source),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.import_result_rx = None;
                if self.import_action.is_running() {
                    self.import_action = ImportActionState::Failed {
                        source: import_action_source(&self.import_action)
                            .unwrap_or(self.import_source),
                        message: "import preview worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_import_commit_result(&mut self) {
        let Some(rx) = self.import_commit_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.import_commit_rx = None;
                self.import_commit_confirmed = false;
                self.import_commit = match result.result {
                    Ok(report) => {
                        self.document_search = DocumentSearchState::Idle;
                        self.document_search_rx = None;
                        if self.current_health() == DaemonHealth::Healthy {
                            self.start_document_list_refresh();
                        }
                        ImportCommitState::Succeeded {
                            report,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => ImportCommitState::Failed {
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.import_commit_rx = None;
                if self.import_commit.is_running() {
                    self.import_commit = ImportCommitState::Failed {
                        message: "document import worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_document_list_result(&mut self) {
        let Some(rx) = self.document_list_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.document_list_rx = None;
                self.document_list = match result.result {
                    Ok(documents) => DocumentListState::Loaded {
                        documents,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => DocumentListState::Failed {
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.document_list_rx = None;
                if self.document_list.is_loading() {
                    self.document_list = DocumentListState::Failed {
                        message: "document list worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_document_search_result(&mut self) {
        let Some(rx) = self.document_search_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.document_search_rx = None;
                self.document_search = match result.result {
                    Ok(success) => DocumentSearchState::Succeeded {
                        query: success.query,
                        hits: success.hits,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => DocumentSearchState::Failed {
                        query: document_search_query(&self.document_search),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.document_search_rx = None;
                if self.document_search.is_searching() {
                    self.document_search = DocumentSearchState::Failed {
                        query: document_search_query(&self.document_search),
                        message: "document search worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_document_detail_result(&mut self) {
        let Some(rx) = self.document_detail_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.document_detail_rx = None;
                self.document_detail = match result.result {
                    Ok(detail) => {
                        self.document_forget_confirmed = false;
                        self.document_forget = DocumentForgetState::Idle;
                        self.document_forget_rx = None;
                        DocumentDetailState::Loaded {
                            detail,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => DocumentDetailState::Failed {
                        doc_id: document_detail_id(&self.document_detail),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.document_detail_rx = None;
                if self.document_detail.is_loading() {
                    self.document_detail = DocumentDetailState::Failed {
                        doc_id: document_detail_id(&self.document_detail),
                        message: "document inspect worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_document_forget_result(&mut self) {
        let Some(rx) = self.document_forget_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.document_forget_rx = None;
                self.document_forget_confirmed = false;
                self.document_forget = match result.result {
                    Ok(report) => {
                        self.document_detail = DocumentDetailState::Idle;
                        self.document_detail_rx = None;
                        self.document_search = DocumentSearchState::Idle;
                        self.document_search_rx = None;
                        if self.current_health() == DaemonHealth::Healthy {
                            self.start_document_list_refresh();
                        }
                        DocumentForgetState::Forgotten {
                            report,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => DocumentForgetState::Failed {
                        doc_id: document_forget_id(&self.document_forget),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.document_forget_rx = None;
                if self.document_forget.is_forgetting() {
                    self.document_forget = DocumentForgetState::Failed {
                        doc_id: document_forget_id(&self.document_forget),
                        message: "document forget worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_memory_result(&mut self) {
        let Some(rx) = self.memory_result_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.memory_result_rx = None;
                self.memory_action = match result.result {
                    Ok(MemoryActionSuccess::Remembered { memory_id }) => {
                        self.memory_capture_text.clear();
                        let action = MemoryActionState::Remembered {
                            memory_id,
                            completed_at: std::time::SystemTime::now(),
                        };
                        if self.current_health() == DaemonHealth::Healthy {
                            self.start_memory_recent_refresh();
                        }
                        action
                    }
                    Ok(MemoryActionSuccess::Search {
                        query,
                        hits,
                        index_len,
                        candidates_considered,
                    }) => MemoryActionState::SearchSucceeded {
                        query,
                        hits,
                        index_len,
                        candidates_considered,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => MemoryActionState::Failed {
                        verb: memory_action_verb(&self.memory_action)
                            .unwrap_or(MemoryActionVerb::Search),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.memory_result_rx = None;
                if self.memory_action.is_running() {
                    self.memory_action = MemoryActionState::Failed {
                        verb: memory_action_verb(&self.memory_action)
                            .unwrap_or(MemoryActionVerb::Search),
                        message: "memory worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_memory_context_result(&mut self) {
        let Some(rx) = self.memory_context_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.memory_context_rx = None;
                self.memory_context = match result.result {
                    Ok(summary) => MemoryContextState::Loaded {
                        summary,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => MemoryContextState::Failed {
                        query: memory_context_query(&self.memory_context),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.memory_context_rx = None;
                if self.memory_context.is_loading() {
                    self.memory_context = MemoryContextState::Failed {
                        query: memory_context_query(&self.memory_context),
                        message: "memory context worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_memory_recent_result(&mut self) {
        let Some(rx) = self.memory_recent_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.memory_recent_rx = None;
                self.memory_recent = match result.result {
                    Ok(memories) => MemoryRecentState::Loaded {
                        memories,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => MemoryRecentState::Failed {
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.memory_recent_rx = None;
                if self.memory_recent.is_loading() {
                    self.memory_recent = MemoryRecentState::Failed {
                        message: "recent-memory worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_memory_detail_result(&mut self) {
        let Some(rx) = self.memory_detail_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.memory_detail_rx = None;
                self.memory_detail = match result.result {
                    Ok(detail) => {
                        self.memory_edit_text = detail.content.clone();
                        self.memory_forget_confirmed = false;
                        self.memory_forget = MemoryForgetState::Idle;
                        self.memory_forget_rx = None;
                        MemoryDetailState::Loaded {
                            detail,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => MemoryDetailState::Failed {
                        memory_id: memory_detail_id(&self.memory_detail),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.memory_detail_rx = None;
                if self.memory_detail.is_loading() {
                    self.memory_detail = MemoryDetailState::Failed {
                        memory_id: memory_detail_id(&self.memory_detail),
                        message: "memory inspect worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_memory_update_result(&mut self) {
        let Some(rx) = self.memory_update_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.memory_update_rx = None;
                self.memory_update = match result.result {
                    Ok(success) => {
                        self.memory_edit_text = success.content;
                        let memory_id = success.memory_id;
                        let updated_at_ms = success.updated_at_ms;
                        if self.current_health() == DaemonHealth::Healthy {
                            self.start_memory_inspect(&memory_id);
                            self.start_memory_recent_refresh();
                        }
                        MemoryUpdateState::Updated {
                            memory_id,
                            updated_at_ms,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => MemoryUpdateState::Failed {
                        memory_id: memory_update_id(&self.memory_update),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.memory_update_rx = None;
                if self.memory_update.is_updating() {
                    self.memory_update = MemoryUpdateState::Failed {
                        memory_id: memory_update_id(&self.memory_update),
                        message: "memory update worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_memory_forget_result(&mut self) {
        let Some(rx) = self.memory_forget_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.memory_forget_rx = None;
                self.memory_forget_confirmed = false;
                self.memory_forget = match result.result {
                    Ok(memory_id) => {
                        self.memory_detail = MemoryDetailState::Idle;
                        self.memory_detail_rx = None;
                        self.memory_edit_text.clear();
                        self.memory_update = MemoryUpdateState::Idle;
                        self.memory_update_rx = None;
                        if self.current_health() == DaemonHealth::Healthy {
                            self.start_memory_recent_refresh();
                            self.start_memory_contradictions_refresh();
                        }
                        MemoryForgetState::Forgotten {
                            memory_id,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => MemoryForgetState::Failed {
                        memory_id: memory_forget_id(&self.memory_forget),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.memory_forget_rx = None;
                if self.memory_forget.is_forgetting() {
                    self.memory_forget = MemoryForgetState::Failed {
                        memory_id: memory_forget_id(&self.memory_forget),
                        message: "memory forget worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_memory_contradictions_result(&mut self) {
        let Some(rx) = self.memory_contradictions_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.memory_contradictions_rx = None;
                self.memory_contradictions = match result.result {
                    Ok(contradictions) => MemoryContradictionState::Loaded {
                        contradictions,
                        completed_at: std::time::SystemTime::now(),
                    },
                    Err(message) => MemoryContradictionState::Failed {
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.memory_contradictions_rx = None;
                if self.memory_contradictions.is_loading() {
                    self.memory_contradictions = MemoryContradictionState::Failed {
                        message: "contradiction worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn poll_memory_contradiction_resolve_result(&mut self) {
        let Some(rx) = self.memory_contradiction_resolve_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(result) => {
                self.memory_contradiction_resolve_rx = None;
                self.memory_contradiction_resolve = match result.result {
                    Ok(resolution) => {
                        if self.current_health() == DaemonHealth::Healthy {
                            self.start_memory_contradictions_refresh();
                        }
                        MemoryContradictionResolveState::Resolved {
                            resolution,
                            completed_at: std::time::SystemTime::now(),
                        }
                    }
                    Err(message) => MemoryContradictionResolveState::Failed {
                        label: memory_contradiction_resolve_label(
                            &self.memory_contradiction_resolve,
                        ),
                        message,
                        completed_at: std::time::SystemTime::now(),
                    },
                };
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.memory_contradiction_resolve_rx = None;
                if self.memory_contradiction_resolve.is_resolving() {
                    self.memory_contradiction_resolve = MemoryContradictionResolveState::Failed {
                        label: memory_contradiction_resolve_label(
                            &self.memory_contradiction_resolve,
                        ),
                        message: "contradiction resolve worker exited without a result".to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                }
            }
        }
    }

    fn start_daemon_backup(&mut self) {
        if self.backup_action.is_running() {
            return;
        }

        let backup_dir = self.backup_snapshot.data_dir.join("backups");
        let dest = backup_dir.join(default_backup_file_name());
        let url = backup_url_from_status_url(&self.state.settings.status_url);
        let (tx, rx) = std::sync::mpsc::channel();
        self.backup_result_rx = Some(rx);
        self.backup_action = BackupActionState::Running {
            dest: dest.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result = run_daemon_backup(url, backup_dir, dest).await;
            let _ = tx.send(BackupActionResult { result });
        });
    }

    fn start_ollama_embedder_migration(&mut self) {
        if self.ollama_migration.is_running() {
            return;
        }
        self.setup_snapshot = collect_setup_snapshot(&self.state.settings_path);
        if !self.setup_snapshot.solo_command_available {
            self.ollama_migration = OllamaMigrationState::Failed {
                model: self.ollama_migration_model.trim().to_string(),
                message: "install solo beside solo-tray or put solo on PATH to run migration"
                    .to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let model = self.ollama_migration_model.trim().to_string();
        if model.is_empty() {
            self.ollama_migration = OllamaMigrationState::Failed {
                model,
                message: "Ollama model must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        let base_url = self.ollama_migration_base_url.trim().to_string();
        if base_url.is_empty() {
            self.ollama_migration = OllamaMigrationState::Failed {
                model,
                message: "Ollama base URL must not be empty".to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }
        let dim = self.ollama_migration_dim.trim().to_string();
        if !dim.is_empty() && dim.parse::<u32>().ok().filter(|value| *value > 0).is_none() {
            self.ollama_migration = OllamaMigrationState::Failed {
                model,
                message: "Dimension must be a positive number, or leave it empty to probe Ollama"
                    .to_string(),
                completed_at: std::time::SystemTime::now(),
            };
            return;
        }

        let passphrase = if self.ollama_migration_passphrase.is_empty() {
            match crate::secret_store::load_daemon_passphrase() {
                Ok(Some(passphrase)) => passphrase,
                Ok(None) => {
                    self.ollama_migration = OllamaMigrationState::Failed {
                        model,
                        message:
                            "enter the daemon passphrase here or save it to the OS keychain first"
                                .to_string(),
                        completed_at: std::time::SystemTime::now(),
                    };
                    return;
                }
                Err(message) => {
                    self.ollama_migration = OllamaMigrationState::Failed {
                        model,
                        message,
                        completed_at: std::time::SystemTime::now(),
                    };
                    return;
                }
            }
        } else {
            Zeroizing::new(std::mem::take(&mut self.ollama_migration_passphrase))
        };
        let worker_passphrase = Zeroizing::new(passphrase.as_str().to_string());
        self.ollama_migration_restart_passphrase = Some(passphrase);

        let mut args: Vec<std::ffi::OsString> = vec![
            "migrate-embedder".into(),
            "ollama".into(),
            "--model".into(),
            model.clone().into(),
            "--base-url".into(),
            base_url.into(),
            "--data-dir".into(),
            self.setup_snapshot.data_dir.as_os_str().into(),
        ];
        if !dim.is_empty() {
            args.push("--dim".into());
            args.push(dim.into());
        }

        let solo_bin = self.setup_solo_bin();
        let daemon_handle = self.state.daemon_handle.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.ollama_migration_rx = Some(rx);
        self.ollama_migration = OllamaMigrationState::Running {
            model: model.clone(),
            started_at: std::time::Instant::now(),
        };

        self.state.runtime_handle.spawn(async move {
            let result =
                run_ollama_embedder_migration(daemon_handle, solo_bin, args, worker_passphrase)
                    .await;
            let _ = tx.send(OllamaMigrationResult { model, result });
        });
    }

    fn draw_main_window(&mut self, ctx: &Context) {
        // Always draw the central panel — the previous `window_visible`
        // gate caused a "blank window" regression: if the user
        // X-closed (→ minimised + `window_visible = false`) and then
        // restored from the Windows taskbar (which doesn't go through
        // our MENU_SHOW_LOGS handler, so `window_visible` stayed
        // false), the panel never re-rendered. The OS itself handles
        // "is the user actually looking at this" via the minimise
        // state; we don't need to gate render on it.
        let dark_mode = ctx.style().visuals.dark_mode;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(content_fill(dark_mode))
                    .inner_margin(egui::Margin::symmetric(18, 16)),
            )
            .show(ctx, |ui| match self.active_tab {
                MainTab::Controls => self.draw_control_window(ui),
                MainTab::Dashboard => self.draw_dashboard_tab(ui),
                MainTab::Health => self.draw_health_tab(ui),
                MainTab::Mcp => self.draw_mcp_tab(ui),
                MainTab::Memory => self.draw_memory_tab(ui),
                MainTab::Projects => self.draw_projects_tab(ui),
                MainTab::Tools => self.draw_tools_tab(ui),
                MainTab::Settings => self.draw_settings_tab(ui),
                MainTab::Data => self.draw_data_tab(ui),
                MainTab::Logs => self.draw_logs_tab(ui),
            });
    }

    fn draw_control_window(&mut self, ui: &mut egui::Ui) {
        let status = self.status_snapshot();
        let daemon = self.daemon_snapshot();
        let dark_mode = ui.visuals().dark_mode;

        ui.horizontal(|ui| {
            ui.heading("Solo Controls");
            ui.separator();
            ui.label(daemon_lifecycle_label(
                daemon.as_ref().map(|snapshot| &snapshot.state),
                status.health,
                dark_mode,
            ));
        });
        ui.label(RichText::new("Start, unlock, and open Solo").color(muted_text_color(dark_mode)));
        ui.add_space(10.0);

        self.draw_lifecycle_panel(ui, &status, daemon.as_ref(), true);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Health").clicked() {
                self.active_tab = MainTab::Health;
            }
            if ui.button("Connected tools").clicked() {
                self.active_tab = MainTab::Tools;
            }
        });
        ui.add_space(12.0);

        egui::Grid::new("control_status_grid")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Memory library").strong());
                ui.label(COMMUNITY_LIBRARY_KEY);
                ui.end_row();

                ui.label(RichText::new("HTTP status").strong());
                ui.label(&self.state.settings.status_url);
                ui.end_row();

                ui.label(RichText::new("MCP URL").strong());
                ui.label(mcp_url_from_status_url(&self.state.settings.status_url));
                ui.end_row();

                ui.label(RichText::new("Solo app").strong());
                ui.label(&self.state.settings.solo_web_url);
                ui.end_row();

                if let Some(at) = status.last_ok_at {
                    ui.label(RichText::new("Last poll").strong());
                    ui.label(format_age(at));
                    ui.end_row();
                }
            });

        if let Some(err) = status.last_error.as_ref() {
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("Last poll error: {err}"))
                    .color(egui::Color32::from_rgb(220, 80, 80)),
            );
        }
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        ui.heading("Logs");
        ui.add_space(4.0);
        self.draw_logs_tab(ui);
    }

    fn draw_dashboard_tab(&mut self, ui: &mut egui::Ui) {
        let status = self.status_snapshot();
        let daemon = self.daemon_snapshot();
        let dark_mode = ui.visuals().dark_mode;

        ui.horizontal(|ui| {
            ui.heading("Dashboard");
            ui.separator();
            ui.label(daemon_lifecycle_label(
                daemon.as_ref().map(|snapshot| &snapshot.state),
                status.health,
                dark_mode,
            ));
        });
        ui.add_space(8.0);

        let show_setup_wizard = !self.state.settings.setup_wizard_completed;
        if show_setup_wizard {
            self.draw_setup_wizard_panel(ui, &status, daemon.as_ref());
            ui.add_space(12.0);
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Setup guide hidden").color(muted_text_color(dark_mode)));
                if ui.button("Show setup guide").clicked() {
                    self.set_setup_wizard_completed(false);
                }
            });
            ui.add_space(12.0);
        }

        self.draw_lifecycle_panel(ui, &status, daemon.as_ref(), !show_setup_wizard);
        ui.add_space(12.0);

        egui::Grid::new("dashboard_status_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Memory library").strong());
                ui.label("Community Memory Library");
                ui.end_row();

                ui.label(RichText::new("Database").strong());
                ui.label(display_path(&self.library_snapshot.db_path));
                ui.end_row();

                ui.label(RichText::new("Supervisor").strong());
                match &daemon {
                    Some(snapshot) => {
                        ui.label(supervisor_state_text(&snapshot.state));
                    }
                    None => {
                        ui.label("busy");
                    }
                }
                ui.end_row();

                if let Some(snapshot) = &daemon {
                    ui.label(RichText::new("PID").strong());
                    ui.label(
                        snapshot
                            .pid
                            .map(|pid| pid.to_string())
                            .unwrap_or_else(|| "none".to_string()),
                    );
                    ui.end_row();

                    ui.label(RichText::new("Process").strong());
                    ui.label(if snapshot.running {
                        "running"
                    } else {
                        "not running"
                    });
                    ui.end_row();

                    ui.label(RichText::new("Supervisor exited").strong());
                    ui.label(if snapshot.supervisor_exited {
                        "yes"
                    } else {
                        "no"
                    });
                    ui.end_row();
                }

                ui.label(RichText::new("HTTP status").strong());
                ui.label(&self.state.settings.status_url);
                ui.end_row();

                ui.label(RichText::new("MCP URL").strong());
                ui.label(mcp_url_from_status_url(&self.state.settings.status_url));
                ui.end_row();

                ui.label(RichText::new("Solo app").strong());
                ui.label(&self.state.settings.solo_web_url);
                ui.end_row();

                if let Some(at) = status.last_ok_at {
                    ui.label(RichText::new("Last successful poll").strong());
                    ui.label(format_age(at));
                    ui.end_row();
                }
            });

        if let Some(err) = status.last_error.as_ref() {
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("Last poll error: {err}"))
                    .color(egui::Color32::from_rgb(220, 80, 80)),
            );
        }

        ui.add_space(12.0);
        match status.last_payload {
            Some(json) => render_status_summary(ui, &json, status.last_ok_at, &status.last_error),
            None => {
                ui.label(
                    "No /v1/status payload yet. Start Solo to enable MCP and library actions.",
                );
            }
        };
    }

    fn draw_health_tab(&mut self, ui: &mut egui::Ui) {
        let status = self.status_snapshot();
        let daemon = self.daemon_snapshot();
        let dark_mode = ui.visuals().dark_mode;
        let payload = status.last_payload.as_ref();

        ui.horizontal(|ui| {
            ui.heading("Health");
            ui.separator();
            ui.label(daemon_lifecycle_label(
                daemon.as_ref().map(|snapshot| &snapshot.state),
                status.health,
                dark_mode,
            ));
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Open Solo").clicked() {
                tray::open_solo_desktop_async(self.state.settings.solo_web_url.clone());
            }
            if ui.button("Connected Tools").clicked() {
                self.active_tab = MainTab::Tools;
            }
            if ui.button("MCP Status").clicked() {
                self.active_tab = MainTab::Mcp;
            }
            if ui.button("Logs").clicked() {
                self.active_tab = MainTab::Logs;
            }
            if ui.button("Settings").clicked() {
                self.active_tab = MainTab::Settings;
            }
        });
        ui.add_space(12.0);

        ui.heading("Daemon");
        egui::Grid::new("health_daemon_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                draw_health_row(
                    ui,
                    "Supervisor",
                    daemon
                        .as_ref()
                        .map(|snapshot| supervisor_state_text(&snapshot.state))
                        .unwrap_or_else(|| "busy".to_string()),
                );
                draw_health_row(ui, "HTTP health", health_state_text(status.health));
                if let Some(snapshot) = &daemon {
                    draw_health_row(
                        ui,
                        "PID",
                        snapshot
                            .pid
                            .map(|pid| pid.to_string())
                            .unwrap_or_else(|| "none".to_string()),
                    );
                    draw_health_row(
                        ui,
                        "Process",
                        if snapshot.running {
                            "running"
                        } else {
                            "not running"
                        },
                    );
                    draw_health_row(
                        ui,
                        "Supervisor exited",
                        if snapshot.supervisor_exited {
                            "yes"
                        } else {
                            "no"
                        },
                    );
                }
                draw_health_row(
                    ui,
                    "Last successful poll",
                    status
                        .last_ok_at
                        .map(format_age)
                        .unwrap_or_else(|| "not yet".to_string()),
                );
                if let Some(err) = status.last_error.as_ref() {
                    draw_health_row(ui, "Last poll error", err);
                }
            });

        ui.add_space(14.0);
        ui.heading("Endpoints");
        egui::Grid::new("health_endpoint_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                draw_health_row(ui, "Status URL", &self.state.settings.status_url);
                draw_health_row(
                    ui,
                    "MCP URL",
                    mcp_url_from_status_url(&self.state.settings.status_url),
                );
                draw_health_row(ui, "Solo app", &self.state.settings.solo_web_url);
            });

        ui.add_space(14.0);
        ui.heading("Memory Library And Storage");
        egui::Grid::new("health_profile_storage_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                draw_health_row(ui, "Memory library", "Community Memory Library");
                draw_health_row(ui, "Database", display_path(&self.library_snapshot.db_path));
                draw_health_row(ui, "Data dir", display_path(&self.setup_snapshot.data_dir));
                draw_health_row(ui, "Settings file", display_path(&self.state.settings_path));
                draw_health_row(
                    ui,
                    "Settings file exists",
                    if self.setup_snapshot.settings_exists {
                        "yes"
                    } else {
                        "no"
                    },
                );
                draw_health_row(
                    ui,
                    "Tray executable",
                    self.setup_snapshot
                        .current_exe
                        .as_deref()
                        .map(display_path)
                        .unwrap_or_else(|| "unknown".to_string()),
                );
                draw_health_row(
                    ui,
                    "Solo command",
                    if self.setup_snapshot.solo_command_available {
                        if self.setup_snapshot.sibling_solo_exists {
                            display_path(&self.setup_snapshot.sibling_solo)
                        } else {
                            "solo on PATH".to_string()
                        }
                    } else {
                        "missing".to_string()
                    },
                );
                draw_health_row(
                    ui,
                    "Solo on PATH",
                    if self.setup_snapshot.solo_on_path_exists {
                        "yes"
                    } else {
                        "no"
                    },
                );
            });

        ui.add_space(14.0);
        ui.heading("Runtime");
        egui::Grid::new("health_runtime_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                let tray_build = solo_core::build_info::version_with_build_metadata();
                draw_health_row(ui, "Tray build", tray_build.as_str());
                draw_health_row(
                    ui,
                    "Daemon version",
                    status_payload_string(payload, "/version"),
                );
                draw_health_row(
                    ui,
                    "Memory library",
                    status_payload_string(payload, "/library/name"),
                );
                draw_health_row(
                    ui,
                    "MCP sessions",
                    status_payload_string(payload, "/mcp/sessions"),
                );
                draw_health_row(
                    ui,
                    "Library ready",
                    status_payload_string(payload, "/library/ready"),
                );
                draw_health_row(ui, "Embedder", status_embedder_summary(payload));
                draw_health_row(ui, "Steward model", status_steward_summary(payload));
            });

        ui.add_space(14.0);
        ui.heading("Secrets");
        egui::Grid::new("health_secret_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                draw_health_row(ui, "Keychain backend", self.secret_snapshot.backend);
                draw_health_row(
                    ui,
                    "Stored secrets",
                    secret_snapshot_status(&self.secret_snapshot),
                );
            });

        ui.add_space(14.0);
        if let Some(json) = payload {
            ui.collapsing("Status payload", |ui| {
                render_status_summary(ui, json, status.last_ok_at, &status.last_error);
            });
        } else {
            ui.label("No /v1/status payload yet. Start Solo to populate runtime fields.");
        }
    }

    fn draw_mcp_tab(&mut self, ui: &mut egui::Ui) {
        let status = self.status_snapshot();
        let payload = status.last_payload.as_ref();
        let dark_mode = ui.visuals().dark_mode;
        let mcp_url = mcp_url_from_status_url(&self.state.settings.status_url);
        let daemon_default_profile = "default".to_string();
        let (mcp_text, mcp_tone, mcp_detail) =
            mcp_status(status.health, payload, &self.state.settings.status_url);
        let (probe_text, probe_tone, probe_detail) = mcp_probe_status(&self.mcp_probe);
        ui.horizontal(|ui| {
            ui.heading("MCP Status");
            ui.separator();
            ui.label(state_text(&mcp_text, mcp_tone, dark_mode));
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Copy MCP URL").clicked() {
                ui.ctx().copy_text(mcp_url.clone());
            }
            if ui
                .add_enabled(!self.mcp_probe.is_running(), egui::Button::new("Probe MCP"))
                .clicked()
            {
                self.start_mcp_probe();
            }
            if ui.button("Refresh").clicked() {
                self.refresh_detected_snapshots_now();
            }
            if ui.button("Connected Tools").clicked() {
                self.active_tab = MainTab::Tools;
            }
        });
        ui.add_space(4.0);
        ui.label(mcp_probe_action_status(&self.mcp_probe));

        ui.add_space(14.0);
        ui.heading("Readiness");
        egui::Grid::new("mcp_readiness_grid")
            .num_columns(3)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Check").strong());
                ui.label(RichText::new("State").strong());
                ui.label(RichText::new("Detail").strong());
                ui.end_row();

                render_state_row(
                    ui,
                    "Daemon HTTP",
                    health_state_text(status.health),
                    if status.health == DaemonHealth::Healthy {
                        StateTone::Good
                    } else {
                        StateTone::Warn
                    },
                    &self.state.settings.status_url,
                );
                render_state_row(ui, "MCP endpoint", &mcp_text, mcp_tone, &mcp_detail);
                render_state_row(ui, "Tray probe", &probe_text, probe_tone, &probe_detail);
                let (doctor_text, doctor_tone, doctor_detail) =
                    mcp_doctor_endpoint_status(&self.setup_doctor);
                render_state_row(
                    ui,
                    "Doctor endpoint",
                    &doctor_text,
                    doctor_tone,
                    &doctor_detail,
                );
            });

        ui.add_space(14.0);
        ui.heading("Runtime");
        egui::Grid::new("mcp_runtime_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for (label, value) in mcp_runtime_rows(
                    payload,
                    &self.state.settings.status_url,
                    &self.secret_snapshot,
                ) {
                    draw_health_row(ui, label, value);
                }
            });

        ui.add_space(14.0);
        ui.heading("Configured Clients");
        ui.add_space(4.0);
        let mut requested_client_check: Option<SetupTarget> = None;
        let client_check_busy = self.client_check.is_running();
        egui::Grid::new("mcp_clients_grid")
            .num_columns(7)
            .spacing([10.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Client").strong());
                ui.label(RichText::new("Config").strong());
                ui.label(RichText::new("Access").strong());
                ui.label(RichText::new("Daemon MCP").strong());
                ui.label(RichText::new("Client load").strong());
                ui.label(RichText::new("Last action").strong());
                ui.label(RichText::new("Action").strong());
                ui.end_row();

                for row in &self.tool_snapshot.rows {
                    let (config_text, config_tone, config_detail) = tool_config_status(row);
                    let (daemon_text, daemon_tone, daemon_detail) = tool_daemon_mcp_status(
                        row,
                        status.health,
                        &self.mcp_probe,
                        &daemon_default_profile,
                    );
                    let (client_text, client_tone, client_detail) =
                        tool_client_load_status(row, &self.client_check);
                    let (access_text, access_tone, access_detail) = workspace_scope_target_status(
                        self.state.settings.workspace_access_scope,
                        row.target,
                        &self.project_snapshot,
                    );
                    let target_access_ready = workspace_access_target_ready(
                        self.state.settings.workspace_access_scope,
                        row.target,
                        &self.project_snapshot,
                    );
                    ui.label(row.target.label());
                    ui.label(state_text(
                        &config_text,
                        config_tone,
                        ui.visuals().dark_mode,
                    ))
                    .on_hover_text(config_detail);
                    ui.label(state_text(
                        &access_text,
                        access_tone,
                        ui.visuals().dark_mode,
                    ))
                    .on_hover_text(&access_detail);
                    ui.label(state_text(
                        &daemon_text,
                        daemon_tone,
                        ui.visuals().dark_mode,
                    ))
                    .on_hover_text(daemon_detail);
                    ui.label(state_text(
                        &client_text,
                        client_tone,
                        ui.visuals().dark_mode,
                    ))
                    .on_hover_text(client_detail);
                    ui.label(tool_last_status_label(row.last_status.as_ref()))
                        .on_hover_text(tool_last_status_detail(row.last_status.as_ref()));
                    ui.horizontal(|ui| {
                        if row.target.supports_automated_client_check()
                            && ui
                                .add_enabled(
                                    target_access_ready && !client_check_busy,
                                    egui::Button::new("Run check"),
                                )
                                .clicked()
                        {
                            requested_client_check = Some(row.target);
                        }
                        if ui
                            .add_enabled(target_access_ready, egui::Button::new("Copy check"))
                            .clicked()
                        {
                            ui.ctx().copy_text(client_smoke_instruction(
                                row.target,
                                self.state.settings.project_root.as_deref(),
                            ));
                        }
                    });
                    ui.end_row();
                }
            });
        if let Some(target) = requested_client_check {
            self.start_client_check(target);
        }
        ui.add_space(6.0);
        ui.label(client_check_status(&self.client_check));
        ui.label(setup_doctor_status(&self.setup_doctor));
        draw_setup_doctor_report(ui, &self.setup_doctor);
    }

    fn draw_setup_wizard_panel(
        &mut self,
        ui: &mut egui::Ui,
        status: &StatusSnapshot,
        daemon: Option<&DaemonSnapshot>,
    ) {
        let dark_mode = ui.visuals().dark_mode;
        let daemon_state = daemon.map(|snapshot| &snapshot.state);
        let daemon_ready = setup_wizard_daemon_ready(daemon_state, status.health);
        let library_ready = setup_wizard_library_ready(&self.library_snapshot);
        let mcp_ready =
            setup_wizard_mcp_ready(status.health, &self.mcp_probe, COMMUNITY_LIBRARY_KEY);
        let verified_tools = setup_wizard_verified_tool_count(&self.tool_snapshot);
        let tool_ready = verified_tools > 0;
        let import_ready = setup_wizard_import_ready(
            &self.import_commit,
            &self.document_list,
            &self.project_docs_import,
        );
        let review_ready = setup_wizard_review_ready(&self.state.settings, &self.memory_recent);
        let all_done = setup_wizard_is_complete(
            daemon_state,
            status.health,
            &self.library_snapshot,
            COMMUNITY_LIBRARY_KEY,
            &self.tool_snapshot,
            &self.mcp_probe,
            import_ready,
            review_ready,
        );
        let completed_count = [
            daemon_ready,
            library_ready,
            mcp_ready,
            tool_ready,
            import_ready,
            review_ready,
        ]
        .into_iter()
        .filter(|done| *done)
        .count();

        let (daemon_text, _, daemon_detail) = daemon_lifecycle_status(daemon_state, status.health);
        let (library_text, _, library_detail) = library_status(&self.library_snapshot);
        let (mcp_text, _, mcp_detail) = if daemon_ready {
            mcp_probe_status(&self.mcp_probe)
        } else {
            mcp_status(
                status.health,
                status.last_payload.as_ref(),
                &self.state.settings.status_url,
            )
        };
        let tool_detail = if tool_ready {
            format!("{verified_tools} client config(s) verified")
        } else {
            "Connect Codex, Claude Desktop, or Cursor to this daemon.".to_string()
        };
        let import_detail = setup_wizard_import_detail(
            &self.import_commit,
            &self.document_list,
            &self.project_docs_import,
        );
        let review_detail = setup_wizard_review_detail(&self.state.settings, &self.memory_recent);

        egui::Frame::new()
            .fill(if dark_mode {
                egui::Color32::from_rgb(28, 34, 36)
            } else {
                egui::Color32::from_rgb(238, 247, 245)
            })
            .stroke(egui::Stroke::new(1.0_f32, border_color(dark_mode)))
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Setup guide");
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{completed_count}/6 ready"))
                            .color(muted_text_color(dark_mode)),
                    );
                });
                ui.add_space(8.0);

                egui::Grid::new("setup_wizard_grid")
                    .num_columns(4)
                    .spacing([14.0, 6.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(RichText::new("Step").strong());
                        ui.label(RichText::new("Status").strong());
                        ui.label(RichText::new("Current").strong());
                        ui.label(RichText::new("Detail").strong());
                        ui.end_row();

                        let mut prior_done = true;
                        let state = setup_wizard_step_state(daemon_ready, prior_done);
                        render_wizard_step(ui, "Start Solo", state, &daemon_text, &daemon_detail);
                        prior_done &= daemon_ready;

                        let state = setup_wizard_step_state(library_ready, prior_done);
                        render_wizard_step(
                            ui,
                            "Open memory library",
                            state,
                            &library_text,
                            &library_detail,
                        );
                        prior_done &= library_ready;

                        let state = setup_wizard_step_state(mcp_ready, prior_done);
                        render_wizard_step(ui, "Verify MCP", state, &mcp_text, &mcp_detail);
                        prior_done &= mcp_ready;

                        let state = setup_wizard_step_state(tool_ready, prior_done);
                        render_wizard_step(
                            ui,
                            "Connect a client",
                            state,
                            if tool_ready {
                                "Verified"
                            } else {
                                "Needs setup"
                            },
                            &tool_detail,
                        );
                        prior_done &= tool_ready;

                        let state = setup_wizard_step_state(import_ready, prior_done);
                        render_wizard_step(
                            ui,
                            "Import data",
                            state,
                            if import_ready {
                                "Imported"
                            } else {
                                "Needs import"
                            },
                            &import_detail,
                        );
                        prior_done &= import_ready;

                        let state = setup_wizard_step_state(review_ready, prior_done);
                        render_wizard_step(
                            ui,
                            "Review memory",
                            state,
                            if review_ready {
                                "Reviewed"
                            } else {
                                "Needs review"
                            },
                            &review_detail,
                        );
                    });

                ui.add_space(10.0);
                if !daemon_ready && !self.setup_snapshot.solo_config_exists {
                    self.draw_first_run_init_controls(ui);
                } else if !daemon_ready && should_show_start_controls(daemon_state) {
                    self.draw_passphrase_controls(ui);
                } else {
                    ui.horizontal(|ui| {
                        if !library_ready {
                            ui.label("The Community memory library is not ready yet.");
                        }
                        if daemon_ready && !mcp_ready {
                            let probe_running = self.mcp_probe.is_running();
                            if ui
                                .add_enabled(!probe_running, egui::Button::new("Probe MCP"))
                                .clicked()
                            {
                                self.start_mcp_probe();
                            }
                            if matches!(&self.mcp_probe, McpProbeState::Failed { .. })
                                && ui.button("Restart Solo").clicked()
                            {
                                self.request_daemon_restart();
                            }
                        }
                        if mcp_ready && !tool_ready {
                            let codex_user_scope_allowed = workspace_access_scope_allows_target(
                                self.state.settings.workspace_access_scope,
                                SetupTarget::CodexUser,
                            );
                            let can_run_setup = self.setup_snapshot.solo_command_available
                                && !self.setup_action.is_running()
                                && codex_user_scope_allowed;
                            if ui
                                .add_enabled(can_run_setup, egui::Button::new("Setup Codex"))
                                .on_hover_text(workspace_scope_action_detail(
                                    codex_user_scope_allowed,
                                    "Project-only mode skips user-level Codex setup. Use Connected Tools after selecting a project root.",
                                    "Set up Codex user config for the active profile.",
                                ))
                                .clicked()
                            {
                                self.start_setup_client_action(
                                    SetupTarget::CodexUser,
                                    SetupActionVerb::Apply,
                                );
                            }
                            if ui.button("Connected tools").clicked() {
                                self.active_tab = MainTab::Tools;
                            }
                        }
                        if daemon_ready && ui.button("Open Solo").clicked() {
                            tray::open_solo_desktop_async(self.state.settings.solo_web_url.clone());
                        }
                        if tool_ready && !import_ready {
                            if ui.button("Import data").clicked() {
                                self.active_tab = MainTab::Data;
                            }
                            if daemon_ready
                                && !self.document_list.is_loading()
                                && ui.button("Refresh documents").clicked()
                            {
                                self.start_document_list_refresh();
                            }
                        }
                        if import_ready && !review_ready {
                            if ui.button("Review memory").clicked() {
                                self.active_tab = MainTab::Memory;
                            }
                            if daemon_ready
                                && !self.memory_recent.is_loading()
                                && ui.button("Load recent").clicked()
                            {
                                self.start_memory_recent_refresh();
                            }
                        }
                        if all_done
                            && ui
                                .add(egui::Button::new("Finish setup").selected(true))
                                .clicked()
                        {
                            self.set_setup_wizard_completed(true);
                        }
                        if ui.button("Skip guide").clicked() {
                            self.set_setup_wizard_completed(true);
                        }
                    });
                    if mcp_ready && !tool_ready {
                        ui.add_space(4.0);
                        ui.label(setup_action_status(&self.setup_action));
                    }
                }
            });
    }

    fn draw_lifecycle_panel(
        &mut self,
        ui: &mut egui::Ui,
        status: &StatusSnapshot,
        daemon: Option<&DaemonSnapshot>,
        show_start_controls: bool,
    ) {
        let dark_mode = ui.visuals().dark_mode;
        let state = daemon.map(|snapshot| &snapshot.state);
        ui.label(RichText::new("Solo readiness").strong());
        ui.add_space(4.0);
        egui::Grid::new("dashboard_lifecycle_grid")
            .num_columns(3)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Step").strong());
                ui.label(RichText::new("State").strong());
                ui.label(RichText::new("Detail").strong());
                ui.end_row();

                let (daemon_text, daemon_tone, daemon_detail) =
                    daemon_lifecycle_status(state, status.health);
                render_state_row(ui, "Daemon", &daemon_text, daemon_tone, &daemon_detail);

                let (lock_text, lock_tone, lock_detail) =
                    lockfile_status(&self.setup_snapshot.lockfile);
                render_state_row(ui, "Lock file", &lock_text, lock_tone, &lock_detail);

                let (passphrase_text, passphrase_tone, passphrase_detail) = passphrase_status(
                    state,
                    &self.secret_snapshot,
                    self.state.settings.remember_passphrase_in_keychain,
                );
                render_state_row(
                    ui,
                    "Passphrase",
                    &passphrase_text,
                    passphrase_tone,
                    &passphrase_detail,
                );

                let (library_text, library_tone, library_detail) =
                    library_status(&self.library_snapshot);
                render_state_row(
                    ui,
                    "Memory library",
                    &library_text,
                    library_tone,
                    &library_detail,
                );

                let (mcp_text, mcp_tone, mcp_detail) = mcp_status(
                    status.health,
                    status.last_payload.as_ref(),
                    &self.state.settings.status_url,
                );
                render_state_row(ui, "MCP", &mcp_text, mcp_tone, &mcp_detail);

                let (probe_text, probe_tone, probe_detail) = mcp_probe_status(&self.mcp_probe);
                render_state_row(ui, "MCP probe", &probe_text, probe_tone, &probe_detail);
            });

        if self.setup_snapshot.lockfile.state == LockfileState::Stale {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("A stale daemon lock is blocking startup.")
                        .color(warning_color(dark_mode)),
                );
                if ui.button("Clear stale lock").clicked() {
                    self.clear_stale_lock_now();
                }
            });
        }

        if show_start_controls && !self.setup_snapshot.solo_config_exists {
            ui.add_space(12.0);
            self.draw_first_run_init_controls(ui);
        } else if show_start_controls && should_show_start_controls(state) {
            ui.add_space(12.0);
            ui.label(RichText::new("Enter passphrase").strong());
            ui.add_space(4.0);
            self.draw_passphrase_controls(ui);
        } else if matches!(state, Some(SupervisorState::Running))
            && status.health == DaemonHealth::Healthy
        {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Open Solo").clicked() {
                    tray::open_solo_desktop_async(self.state.settings.solo_web_url.clone());
                }
                if ui.button("Restart Solo").clicked() {
                    self.request_daemon_restart();
                }
                if ui
                    .add_enabled(!self.mcp_probe.is_running(), egui::Button::new("Probe MCP"))
                    .clicked()
                {
                    self.start_mcp_probe();
                }
            });
        } else {
            ui.add_space(8.0);
            ui.label(
                RichText::new("Solo is starting; MCP and library actions will unlock once /v1/status is healthy.")
                    .color(warning_color(dark_mode)),
            );
        }
    }

    fn draw_logs_tab(&mut self, ui: &mut egui::Ui) {
        if self.log_source == LogSource::Tray {
            self.refresh_tray_log_tail(false);
        }

        ui.horizontal(|ui| {
            ui.label("Source:");
            egui::ComboBox::from_id_salt("log_source")
                .selected_text(self.log_source.label())
                .show_ui(ui, |ui| {
                    for source in LogSource::ALL {
                        ui.selectable_value(&mut self.log_source, source, source.label())
                            .on_hover_text(source.description());
                    }
                });
            ui.label("Filter:");
            egui::ComboBox::from_id_salt("filter_level")
                .selected_text(format!("{:?}+", self.filter_level))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filter_level, Level::Trace, "Trace+");
                    ui.selectable_value(&mut self.filter_level, Level::Debug, "Debug+");
                    ui.selectable_value(&mut self.filter_level, Level::Info, "Info+");
                    ui.selectable_value(&mut self.filter_level, Level::Warn, "Warn+");
                    ui.selectable_value(&mut self.filter_level, Level::Error, "Error only");
                });
            ui.checkbox(&mut self.auto_scroll, "Auto-scroll");
            if self.log_source == LogSource::Tray && ui.button("Refresh").clicked() {
                self.refresh_tray_log_tail(true);
            }
            if ui.button("Clear buffer").clicked() {
                match self.log_source {
                    LogSource::Daemon => {
                        let buf = self.state.log_buffer.clone();
                        self.state.runtime_handle.spawn(async move {
                            buf.lock().await.clear();
                        });
                    }
                    LogSource::Tray => {
                        self.tray_log_lines.clear();
                        self.tray_log_status =
                            "cleared the displayed tray-log tail; refresh to reload".to_string();
                    }
                }
            }
            ui.separator();
            ui.label(health_label(self.current_health(), ui.visuals().dark_mode));
        });

        ui.separator();

        let (lines, status_text) = match self.log_source {
            LogSource::Daemon => match self.state.log_buffer.try_lock() {
                Ok(buf) => {
                    let visible = daemon_log_visible_lines(&buf, self.filter_level);
                    let status = daemon_log_status(&buf);
                    (visible, status)
                }
                Err(_) => (Vec::new(), "buffer locked; will refresh".to_string()),
            },
            LogSource::Tray => (
                tray_log_visible_lines(&self.tray_log_lines, self.filter_level),
                self.tray_log_status.clone(),
            ),
        };

        ui.horizontal(|ui| {
            if ui.button("Copy visible").clicked() {
                ui.ctx().copy_text(format_log_copy(&lines, lines.len()));
            }
            if ui.button("Copy last 200").clicked() {
                ui.ctx().copy_text(format_log_copy(&lines, 200));
            }
            if self.log_source == LogSource::Tray {
                let tray_log = crate::logs::tray_log_path();
                if ui.button("Open tray.log").clicked() {
                    if tray_log.is_file() {
                        self.tray_log_status =
                            "opening tray.log; use Copy path if nothing appears".to_string();
                        open_path_async(tray_log.clone(), "tray log");
                    } else {
                        self.tray_log_status =
                            format!("tray.log does not exist yet at {}", display_path(&tray_log));
                    }
                }
                if ui.button("Open log folder").clicked() {
                    let dir = tray_log
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(tray::resolve_data_dir);
                    if dir.is_dir() {
                        self.tray_log_status =
                            "opening log folder; use Copy path if nothing appears".to_string();
                        open_path_async(dir, "log folder");
                    } else {
                        self.tray_log_status =
                            format!("log folder does not exist yet at {}", display_path(&dir));
                    }
                }
                if ui.button("Copy path").clicked() {
                    ui.ctx().copy_text(display_path(&tray_log));
                    self.tray_log_status = "copied tray.log path".to_string();
                }
            }
        });
        ui.add_space(4.0);
        ui.label(RichText::new(self.log_source.description()).weak());
        ui.add_space(6.0);

        let mono = TextStyle::Monospace;
        let scroll = ScrollArea::vertical().auto_shrink([false, false]);
        let scroll = if self.auto_scroll {
            scroll.stick_to_bottom(true)
        } else {
            scroll
        };
        scroll.show(ui, |ui| {
            for (level, text) in &lines {
                let color = level_color(*level, ui.visuals().dark_mode);
                ui.label(RichText::new(text).text_style(mono.clone()).color(color));
            }
        });

        ui.separator();
        ui.label(RichText::new(status_text).weak());
    }

    fn refresh_tray_log_tail(&mut self, force: bool) {
        const TRAY_LOG_TAIL_LINES: usize = 400;
        const TRAY_LOG_REFRESH_SECS: u64 = 2;

        let now = std::time::Instant::now();
        if !force
            && self
                .tray_log_last_refresh
                .is_some_and(|last| now.duration_since(last).as_secs() < TRAY_LOG_REFRESH_SECS)
        {
            return;
        }

        let path = crate::logs::tray_log_path();
        match crate::logs::read_tail_lines(&path, TRAY_LOG_TAIL_LINES) {
            Ok(lines) => {
                let retained = lines.len();
                self.tray_log_lines = lines;
                self.tray_log_status = format!(
                    "tray.log tail: retained {} from {}",
                    retained,
                    display_path(&path)
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.tray_log_lines.clear();
                self.tray_log_status =
                    format!("tray.log does not exist yet at {}", display_path(&path));
            }
            Err(e) => {
                self.tray_log_lines.clear();
                self.tray_log_status =
                    format!("could not read tray.log at {}: {e}", display_path(&path));
            }
        }
        self.tray_log_last_refresh = Some(now);
    }

    fn draw_tools_tab(&mut self, ui: &mut egui::Ui) {
        ScrollArea::vertical()
            .id_salt("tools_tab_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| self.draw_tools_tab_content(ui));
    }

    fn draw_tools_tab_content(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connected Tools");
        ui.add_space(8.0);

        let status = self.status_snapshot();
        let mcp_url = mcp_url_from_status_url(&self.state.settings.status_url);
        let (mcp_text, mcp_tone, mcp_detail) = mcp_status(
            status.health,
            status.last_payload.as_ref(),
            &self.state.settings.status_url,
        );
        let (probe_text, probe_tone, probe_detail) = mcp_probe_status(&self.mcp_probe);
        egui::Grid::new("tools_overview_grid")
            .num_columns(3)
            .spacing([16.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Surface").strong());
                ui.label(RichText::new("Status").strong());
                ui.label(RichText::new("Value").strong());
                ui.end_row();

                ui.label("Daemon");
                ui.label(health_label(status.health, ui.visuals().dark_mode));
                ui.label(self.state.settings.status_url.clone());
                ui.end_row();

                ui.label("MCP HTTP");
                ui.label(state_text(&mcp_text, mcp_tone, ui.visuals().dark_mode));
                ui.label(&mcp_detail);
                ui.end_row();

                ui.label("MCP probe");
                ui.label(state_text(&probe_text, probe_tone, ui.visuals().dark_mode));
                ui.label(&probe_detail);
                ui.end_row();

                ui.label("Memory library");
                ui.label(state_text(
                    "Community",
                    if status.health == DaemonHealth::Healthy {
                        StateTone::Good
                    } else {
                        StateTone::Warn
                    },
                    ui.visuals().dark_mode,
                ));
                ui.label("One local encrypted library");
                ui.end_row();

                ui.label("Solo app");
                ui.label("owned app window");
                ui.label(self.state.settings.solo_web_url.clone());
                ui.end_row();

                ui.label("Keychain");
                ui.label(secret_snapshot_status(&self.secret_snapshot));
                ui.label(self.secret_snapshot.backend);
                ui.end_row();
            });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Copy MCP URL").clicked() {
                ui.ctx().copy_text(mcp_url.clone());
            }
            if ui
                .add_enabled(!self.mcp_probe.is_running(), egui::Button::new("Probe MCP"))
                .clicked()
            {
                self.start_mcp_probe();
            }
            if ui.button("Refresh").clicked() {
                self.refresh_detected_snapshots_now();
            }
            if ui.button("Settings").clicked() {
                self.active_tab = MainTab::Settings;
            }
        });
        ui.add_space(4.0);
        ui.label(mcp_probe_action_status(&self.mcp_probe));

        ui.add_space(14.0);
        ui.label(RichText::new("Memory policy pack").strong());
        ui.add_space(4.0);
        egui::Grid::new("tools_policy_pack_grid")
            .num_columns(3)
            .spacing([12.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Client").strong());
                ui.label(RichText::new("Policy").strong());
                ui.label(RichText::new("Action").strong());
                ui.end_row();

                for row in policy_pack_rows() {
                    ui.label(row.label);
                    ui.add(egui::Label::new(row.detail).wrap());
                    if ui.button("Copy policy").clicked() {
                        ui.ctx().copy_text(row.text.to_string());
                    }
                    ui.end_row();
                }
            });

        ui.add_space(14.0);
        ui.label(RichText::new("Clients").strong());
        ui.add_space(4.0);
        let mut requested_action: Option<(SetupTarget, SetupActionVerb)> = None;
        let mut requested_client_check: Option<SetupTarget> = None;
        let mut requested_tool_detail: Option<SetupTarget> = None;
        let mut requested_doctor: Option<SetupTarget> = None;
        let setup_busy = self.setup_action.is_running();
        let client_check_busy = self.client_check.is_running();
        let doctor_busy = self.setup_doctor.is_running();
        let can_run_setup =
            self.setup_snapshot.solo_command_available && !setup_busy && !doctor_busy;
        let daemon_default_profile = "Community Memory Library".to_string();
        egui::Grid::new("tools_clients_grid")
            .num_columns(8)
            .spacing([10.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Tool").strong());
                ui.label(RichText::new("Config").strong());
                ui.label(RichText::new("Daemon MCP").strong());
                ui.label(RichText::new("Client").strong());
                ui.label(RichText::new("Access").strong());
                ui.label(RichText::new("Config file").strong());
                ui.label(RichText::new("Last action").strong());
                ui.label(RichText::new("Actions").strong());
                ui.end_row();

                for row in &self.tool_snapshot.rows {
                    let target = row.target;
                    let target_access_ready = workspace_access_target_ready(
                        self.state.settings.workspace_access_scope,
                        target,
                        &self.project_snapshot,
                    );
                    let can_run_target_setup = can_run_setup && target_access_ready;
                    let (config_text, config_tone, config_detail) = tool_config_status(row);
                    let (daemon_text, daemon_tone, daemon_detail) = tool_daemon_mcp_status(
                        row,
                        status.health,
                        &self.mcp_probe,
                        &daemon_default_profile,
                    );
                    let (client_text, client_tone, client_detail) =
                        tool_client_load_status(row, &self.client_check);
                    let (access_text, access_tone, access_detail) =
                        workspace_scope_target_status(
                            self.state.settings.workspace_access_scope,
                            target,
                            &self.project_snapshot,
                        );
                    ui.label(target.label());
                    ui.label(state_text(
                        &config_text,
                        config_tone,
                        ui.visuals().dark_mode,
                    ))
                    .on_hover_text(config_detail);
                    ui.label(state_text(
                        &daemon_text,
                        daemon_tone,
                        ui.visuals().dark_mode,
                    ))
                    .on_hover_text(daemon_detail);
                    ui.label(state_text(
                        &client_text,
                        client_tone,
                        ui.visuals().dark_mode,
                    ))
                    .on_hover_text(client_detail);
                    ui.label(state_text(
                        &access_text,
                        access_tone,
                        ui.visuals().dark_mode,
                    ))
                    .on_hover_text(&access_detail);
                    ui.add(
                        egui::Label::new(
                            row.path
                                .as_deref()
                                .map(display_path)
                                .unwrap_or_else(|| row.detail.clone()),
                        )
                        .wrap(),
                    );
                    ui.label(tool_last_status_label(row.last_status.as_ref()))
                        .on_hover_text(tool_last_status_detail(row.last_status.as_ref()));
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                can_run_target_setup,
                                egui::Button::new("Setup / repair"),
                            )
                            .on_hover_text(workspace_scope_action_detail(
                                target_access_ready,
                                &access_detail,
                                &row.detail,
                            ))
                            .clicked()
                        {
                            requested_action = Some((target, SetupActionVerb::Apply));
                        }
                        if ui
                            .add_enabled(
                                can_run_target_setup,
                                egui::Button::new("Verify"),
                            )
                            .on_hover_text(workspace_scope_action_detail(
                                target_access_ready,
                                &access_detail,
                                &row.detail,
                            ))
                            .clicked()
                        {
                            requested_action = Some((target, SetupActionVerb::Verify));
                        }
                        if ui
                            .add_enabled(
                                can_run_target_setup && !doctor_busy,
                                egui::Button::new("Doctor"),
                            )
                            .on_hover_text(
                                "Runs setup-client doctor for this client and the MCP endpoint.",
                            )
                            .clicked()
                        {
                            requested_doctor = Some(target);
                        }
                        if ui.button("Copy policy").clicked() {
                            ui.ctx()
                                .copy_text(policy_text_for_setup_target(target).to_string());
                        }
                        if ui
                            .add_enabled(
                                target_access_ready,
                                egui::Button::new("Copy check"),
                            )
                            .clicked()
                        {
                            ui.ctx().copy_text(client_smoke_instruction(
                                target,
                                self.state.settings.project_root.as_deref(),
                            ));
                        }
                        let detail_label = if self.selected_tool_detail == Some(target) {
                            "Hide details"
                        } else {
                            "Details"
                        };
                        if ui.button(detail_label).clicked() {
                            requested_tool_detail = Some(target);
                        }
                        if target.supports_automated_client_check()
                            && ui
                                .add_enabled(
                                    target_access_ready && !client_check_busy,
                                    egui::Button::new("Run check"),
                                )
                                .on_hover_text(
                                    "Runs `codex mcp list` when the Codex CLI is available on PATH.",
                                )
                                .clicked()
                        {
                            requested_client_check = Some(target);
                        }
                    });
                    ui.end_row();
                }
            });
        if let Some(target) = requested_tool_detail {
            self.selected_tool_detail = if self.selected_tool_detail == Some(target) {
                None
            } else {
                Some(target)
            };
        }
        if let Some((target, verb)) = requested_action {
            self.start_setup_client_action(target, verb);
        }
        if let Some(target) = requested_client_check {
            self.start_client_check(target);
        }
        if let Some(target) = requested_doctor {
            self.start_setup_client_doctor(target);
        }
        if let Some(target) = self.selected_tool_detail {
            if let Some(row) = self
                .tool_snapshot
                .rows
                .iter()
                .find(|row| row.target == target)
            {
                ui.add_space(8.0);
                draw_tool_verification_details(ui, row, &daemon_default_profile);
            } else {
                self.selected_tool_detail = None;
            }
        }
        ui.add_space(6.0);
        ui.label(setup_action_status(&self.setup_action));
        ui.label(client_check_status(&self.client_check));
        ui.label(setup_doctor_status(&self.setup_doctor));
        draw_setup_doctor_report(ui, &self.setup_doctor);
        if !self.setup_snapshot.solo_command_available {
            ui.label(
                RichText::new(
                    "Install solo beside solo-tray or put solo on PATH to enable one-click setup.",
                )
                .color(egui::Color32::from_rgb(220, 180, 60)),
            );
        }

        ui.add_space(12.0);
        if ui.button("Copy setup commands").clicked() {
            let command_block = setup_client_command_block(
                &self.state.settings.status_url,
                &self.setup_snapshot.data_dir,
            );
            ui.ctx().copy_text(command_block);
        }
        if ui.button("Open command fallback").clicked() {
            self.active_tab = MainTab::Settings;
        }
    }

    fn draw_memory_tab(&mut self, ui: &mut egui::Ui) {
        let status = self.status_snapshot();
        let daemon = self.daemon_snapshot();
        let dark_mode = ui.visuals().dark_mode;
        let memory_ready = status.health == DaemonHealth::Healthy;
        if memory_ready && matches!(self.memory_recent, MemoryRecentState::Idle) {
            self.start_memory_recent_refresh();
        }
        if memory_ready && matches!(self.memory_contradictions, MemoryContradictionState::Idle) {
            self.start_memory_contradictions_refresh();
        }
        let mut inspect_memory_id: Option<String> = None;
        let mut review_memory_action: Option<(String, Option<&'static str>)> = None;
        let mut review_memory_batch_action: Option<(Vec<String>, Option<&'static str>)> = None;
        let mut contradiction_resolve_action: Option<ContradictionResolveAction> = None;

        ui.heading("Memory");
        ui.add_space(8.0);

        egui::Grid::new("memory_readiness_grid")
            .num_columns(3)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Item").strong());
                ui.label(RichText::new("State").strong());
                ui.label(RichText::new("Detail").strong());
                ui.end_row();

                let (daemon_text, daemon_tone, daemon_detail) = daemon_lifecycle_status(
                    daemon.as_ref().map(|snapshot| &snapshot.state),
                    status.health,
                );
                render_state_row(ui, "Daemon", &daemon_text, daemon_tone, &daemon_detail);

                let (mcp_text, mcp_tone, mcp_detail) = mcp_status(
                    status.health,
                    status.last_payload.as_ref(),
                    &self.state.settings.status_url,
                );
                render_state_row(ui, "MCP", &mcp_text, mcp_tone, &mcp_detail);

                ui.label("Memory library");
                ui.label(state_text(
                    COMMUNITY_LIBRARY_KEY,
                    if memory_ready {
                        StateTone::Good
                    } else {
                        StateTone::Warn
                    },
                    dark_mode,
                ));
                ui.add(
                    egui::Label::new("Memory writes and recalls use this local library.").wrap(),
                );
                ui.end_row();
            });

        if !memory_ready {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Start Solo to add or search memory.")
                        .color(warning_color(dark_mode)),
                );
                if ui.button("Dashboard").clicked() {
                    self.active_tab = MainTab::Dashboard;
                }
            });
        }

        ui.add_space(14.0);
        ui.label(RichText::new("Inbox").strong());
        ui.add_space(4.0);
        ui.add_sized(
            [ui.available_width(), 96.0],
            egui::TextEdit::multiline(&mut self.memory_capture_text)
                .desired_rows(4)
                .hint_text("A durable preference, decision, or fact"),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_remember = memory_ready
                && !self.memory_action.is_running()
                && !self.memory_capture_text.trim().is_empty();
            if ui
                .add_enabled(can_remember, egui::Button::new("Save memory"))
                .clicked()
            {
                self.start_memory_remember();
            }
            if ui
                .add_enabled(
                    !self.memory_capture_text.is_empty(),
                    egui::Button::new("Clear"),
                )
                .clicked()
            {
                self.memory_capture_text.clear();
            }
        });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "Recent memories (last {MEMORY_INBOX_RECENT_LIMIT})"
                ))
                .strong(),
            );
            let can_refresh = memory_ready && !self.memory_recent.is_loading();
            if ui
                .add_enabled(can_refresh, egui::Button::new("Refresh"))
                .clicked()
            {
                self.start_memory_recent_refresh();
            }
        });
        ui.add_space(4.0);
        ui.label(memory_recent_status(&self.memory_recent));
        if let Some(memories) = memory_recent_items(&self.memory_recent) {
            let counts = memory_review_counts(memories, &self.state.settings);
            let visible_memories: Vec<&RecentMemory> = memories
                .iter()
                .filter(|memory| {
                    let review = memory_effective_review_status(&self.state.settings, memory);
                    memory_review_matches_filter(review.as_ref(), self.memory_review_filter)
                        && memory_source_matches_filter(memory, self.memory_source_filter)
                })
                .collect();
            let visible_memory_ids = visible_memories
                .iter()
                .map(|memory| memory.memory_id.clone())
                .collect::<Vec<_>>();
            ui.horizontal(|ui| {
                ui.label(RichText::new(memory_review_counts_label(&counts)).weak());
                egui::ComboBox::from_id_salt("memory_review_filter")
                    .selected_text(self.memory_review_filter.label())
                    .show_ui(ui, |ui| {
                        for filter in MemoryReviewFilter::ALL {
                            ui.selectable_value(
                                &mut self.memory_review_filter,
                                filter,
                                filter.label(),
                            );
                        }
                    });
                egui::ComboBox::from_id_salt("memory_source_filter")
                    .selected_text(self.memory_source_filter.label())
                    .show_ui(ui, |ui| {
                        for filter in MemorySourceFilter::ALL {
                            ui.selectable_value(
                                &mut self.memory_source_filter,
                                filter,
                                filter.label(),
                            );
                        }
                    });
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(state_text(
                    &memory_review_status_label(&counts),
                    memory_review_status_tone(&counts),
                    dark_mode,
                ))
                .on_hover_text(memory_review_scope_detail(&counts));
                ui.label(
                    RichText::new(memory_review_visible_label(
                        visible_memories.len(),
                        counts.total,
                        self.memory_review_filter,
                        self.memory_source_filter,
                    ))
                    .color(muted_text_color(dark_mode)),
                );
                if ui.button("Copy summary").clicked() {
                    ui.ctx().copy_text(memory_review_clipboard_summary(
                        &counts,
                        visible_memories.len(),
                        self.memory_review_filter,
                        self.memory_source_filter,
                    ));
                }
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let can_apply_visible = !visible_memory_ids.is_empty();
                if ui
                    .add_enabled(can_apply_visible, egui::Button::new("Approve shown"))
                    .on_hover_text("Marks the visible memories reviewed in this library.")
                    .clicked()
                {
                    review_memory_batch_action =
                        Some((visible_memory_ids.clone(), Some("approved")));
                }
                if ui
                    .add_enabled(can_apply_visible, egui::Button::new("Dismiss shown"))
                    .on_hover_text(
                        "Hides the visible memories from Needs review without deleting them.",
                    )
                    .clicked()
                {
                    review_memory_batch_action =
                        Some((visible_memory_ids.clone(), Some("dismissed")));
                }
                let can_reset_visible = visible_memories.iter().any(|memory| {
                    memory_effective_review_status(&self.state.settings, memory).is_some()
                });
                if ui
                    .add_enabled(can_reset_visible, egui::Button::new("Reset shown"))
                    .on_hover_text("Clears review state for the visible memories.")
                    .clicked()
                {
                    review_memory_batch_action = Some((visible_memory_ids.clone(), None));
                }
            });
            ui.add_space(4.0);
            if memories.is_empty() {
                ui.label("No memories in this library yet.");
            } else if visible_memories.is_empty() {
                ui.label("No memories match the selected filters.");
            } else {
                ScrollArea::vertical()
                    .id_salt("recent_memory_results")
                    .max_height(190.0)
                    .show(ui, |ui| {
                        for (idx, memory) in visible_memories.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                            }
                            let review =
                                memory_effective_review_status(&self.state.settings, memory);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(memory_age_label(memory.ts_ms)).strong());
                                ui.label(state_text(
                                    memory_review_label(review.as_ref()),
                                    memory_review_tone(review.as_ref()),
                                    dark_mode,
                                ))
                                .on_hover_text(memory_review_detail(review.as_ref()));
                                ui.label(
                                    RichText::new(memory_source_summary(memory))
                                        .color(muted_text_color(dark_mode)),
                                );
                                if ui.button("Inspect").clicked() {
                                    inspect_memory_id = Some(memory.memory_id.clone());
                                }
                                if ui.button("Approve").clicked() {
                                    review_memory_action =
                                        Some((memory.memory_id.clone(), Some("approved")));
                                }
                                let dismiss_clicked = ui
                                    .button("Dismiss")
                                    .on_hover_text("Hide locally without deleting memory.")
                                    .clicked();
                                if dismiss_clicked {
                                    review_memory_action =
                                        Some((memory.memory_id.clone(), Some("dismissed")));
                                }
                                if review.is_some() && ui.button("Needs review").clicked() {
                                    review_memory_action = Some((memory.memory_id.clone(), None));
                                }
                                if ui.button("Copy id").clicked() {
                                    ui.ctx().copy_text(memory.memory_id.clone());
                                }
                            });
                            ui.label(RichText::new(&memory.label).strong());
                            ui.add(egui::Label::new(&memory.preview).wrap());
                        }
                    });
            }
        }

        ui.add_space(12.0);
        ui.label(RichText::new("Recall").strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let search_width = (ui.available_width() - 88.0).max(180.0);
            ui.add_sized(
                [search_width, 28.0],
                egui::TextEdit::singleline(&mut self.memory_search_query)
                    .hint_text("Search remembered context"),
            );
            let can_search = memory_ready
                && !self.memory_action.is_running()
                && !self.memory_search_query.trim().is_empty();
            if ui
                .add_enabled(can_search, egui::Button::new("Search"))
                .clicked()
            {
                self.start_memory_search();
            }
        });

        ui.add_space(6.0);
        ui.label(memory_action_status(&self.memory_action));
        if let Some((query, hits, index_len, candidates_considered)) =
            memory_search_results(&self.memory_action)
        {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "{} result(s) for `{query}`; index {}; candidates {}",
                    hits.len(),
                    index_len,
                    candidates_considered
                ))
                .color(muted_text_color(dark_mode)),
            );
            ui.add_space(4.0);
            if hits.is_empty() {
                ui.label("No matching memories found.");
            } else {
                ScrollArea::vertical()
                    .id_salt("memory_search_results")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for (idx, hit) in hits.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&hit.source_type).strong());
                                ui.label(
                                    RichText::new(&hit.tier).color(muted_text_color(dark_mode)),
                                );
                                ui.label(
                                    RichText::new(format!("score {:.3}", hit.fused_score))
                                        .color(muted_text_color(dark_mode)),
                                );
                                ui.label(
                                    RichText::new(format!("distance {:.3}", hit.cos_distance))
                                        .color(muted_text_color(dark_mode)),
                                );
                                if ui.button("Inspect").clicked() {
                                    inspect_memory_id = Some(hit.memory_id.clone());
                                }
                                if ui.button("Copy id").clicked() {
                                    ui.ctx().copy_text(hit.memory_id.clone());
                                }
                            });
                            ui.add(egui::Label::new(&hit.content).wrap());
                        }
                    });
            }
        }

        ui.add_space(12.0);
        ui.label(RichText::new("Context preview").strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let query_width = (ui.available_width() * 0.62).max(180.0);
            ui.add_sized(
                [query_width, 28.0],
                egui::TextEdit::singleline(&mut self.memory_context_query)
                    .hint_text("Question or task"),
            );
            let subject_width = (ui.available_width() - 98.0).max(120.0);
            ui.add_sized(
                [subject_width, 28.0],
                egui::TextEdit::singleline(&mut self.memory_context_subject)
                    .hint_text("Subject optional"),
            );
            let can_context = memory_ready
                && !self.memory_context.is_loading()
                && !self.memory_context_query.trim().is_empty();
            if ui
                .add_enabled(can_context, egui::Button::new("Build"))
                .clicked()
            {
                self.start_memory_context();
            }
        });
        ui.add_space(4.0);
        ui.label(memory_context_status(&self.memory_context, status.health));
        if let Some(summary) = memory_context_summary(&self.memory_context) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "subject: {}",
                    memory_context_subject_label(summary)
                ))
                .color(muted_text_color(dark_mode)),
            );
            egui::Grid::new("memory_context_sections_grid")
                .num_columns(4)
                .spacing([12.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label(RichText::new("Section").strong());
                    ui.label(RichText::new("Status").strong());
                    ui.label(RichText::new("Count").strong());
                    ui.label(RichText::new("Warning").strong());
                    ui.end_row();
                    for section in &summary.sections {
                        ui.label(section.name);
                        ui.label(&section.status);
                        ui.label(section.count.to_string());
                        ui.add(egui::Label::new(section.warning.as_deref().unwrap_or("-")).wrap());
                        ui.end_row();
                    }
                });
            ui.add_space(6.0);
            ScrollArea::vertical()
                .id_salt("memory_context_preview_results")
                .max_height(220.0)
                .show(ui, |ui| {
                    ui.label(RichText::new("Recall").strong());
                    if summary.recall_hits.is_empty() {
                        ui.label("No recalled memories.");
                    } else {
                        for hit in summary.recall_hits.iter().take(5) {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&hit.source_type).strong());
                                ui.label(
                                    RichText::new(format!("score {:.3}", hit.fused_score))
                                        .color(muted_text_color(dark_mode)),
                                );
                                if ui.button("Inspect").clicked() {
                                    inspect_memory_id = Some(hit.memory_id.clone());
                                }
                            });
                            ui.add(egui::Label::new(&hit.content).wrap());
                        }
                    }
                    ui.separator();
                    ui.label(RichText::new("Facts").strong());
                    if summary.facts.is_empty() {
                        ui.label("No facts.");
                    } else {
                        for fact in summary.facts.iter().take(5) {
                            ui.label(project_fact_label(fact));
                        }
                    }
                    ui.separator();
                    ui.label(RichText::new("Themes").strong());
                    if summary.themes.is_empty() {
                        ui.label("No themes.");
                    } else {
                        for theme in summary.themes.iter().take(5) {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("{} episode(s)", theme.episode_count))
                                        .color(muted_text_color(dark_mode)),
                                );
                                ui.label(
                                    RichText::new(format!("coherence {:.2}", theme.coherence))
                                        .color(muted_text_color(dark_mode)),
                                );
                                ui.label(
                                    RichText::new(memory_timestamp_label(Some(
                                        theme.created_at_ms,
                                    )))
                                    .color(muted_text_color(dark_mode)),
                                );
                            });
                            ui.add(
                                egui::Label::new(
                                    theme
                                        .abstraction_text
                                        .as_deref()
                                        .unwrap_or(&theme.cluster_id),
                                )
                                .wrap(),
                            );
                        }
                    }
                    ui.separator();
                    ui.label(RichText::new("Graph").strong());
                    if summary.graph.seed_entities.is_empty()
                        && summary.graph.relationship_facts.is_empty()
                        && summary.graph.literal_facts.is_empty()
                        && summary.graph.review_warnings.is_empty()
                    {
                        ui.label("No graph context.");
                    } else {
                        if !summary.graph.seed_entities.is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "seeds: {}",
                                    summary.graph.seed_entities.join(", ")
                                ))
                                .color(muted_text_color(dark_mode)),
                            );
                        }
                        for fact in summary.graph.relationship_facts.iter().take(5) {
                            ui.add(egui::Label::new(memory_context_graph_fact_label(fact)).wrap());
                        }
                        for fact in summary.graph.literal_facts.iter().take(3) {
                            ui.add(egui::Label::new(memory_context_graph_fact_label(fact)).wrap());
                        }
                        if !summary.graph.review_warnings.is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "{} review warning(s)",
                                    summary.graph.review_warnings.len()
                                ))
                                .color(warning_color(dark_mode)),
                            );
                            for warning in summary.graph.review_warnings.iter().take(3) {
                                ui.add(
                                    egui::Label::new(memory_context_graph_warning_label(warning))
                                        .wrap(),
                                );
                            }
                        }
                    }
                });
        }

        if let Some(memory_id) = inspect_memory_id {
            self.start_memory_inspect(&memory_id);
        }
        if let Some((memory_id, state)) = review_memory_action {
            self.set_memory_review_state(&memory_id, state);
        }
        if let Some((memory_ids, state)) = review_memory_batch_action {
            self.set_memory_review_states(&memory_ids, state);
        }
        ui.add_space(12.0);
        ui.label(RichText::new("Selected memory").strong());
        ui.add_space(4.0);
        ui.label(memory_detail_status(&self.memory_detail));
        if !matches!(self.memory_detail, MemoryDetailState::Loaded { .. })
            && !matches!(self.memory_forget, MemoryForgetState::Idle)
        {
            ui.label(memory_forget_status(&self.memory_forget));
        }
        if let Some(detail) = memory_detail_loaded(&self.memory_detail).cloned() {
            ui.add_space(4.0);
            egui::Grid::new("memory_detail_grid")
                .num_columns(2)
                .spacing([14.0, 5.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label(RichText::new("ID").strong());
                    ui.horizontal(|ui| {
                        ui.label(&detail.memory_id);
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(detail.memory_id.clone());
                        }
                    });
                    ui.end_row();

                    ui.label(RichText::new("Status").strong());
                    ui.label(&detail.status);
                    ui.end_row();

                    ui.label(RichText::new("Source").strong());
                    ui.label(memory_detail_source_label(&detail));
                    ui.end_row();

                    ui.label(RichText::new("Tier").strong());
                    ui.label(&detail.tier);
                    ui.end_row();

                    ui.label(RichText::new("Signals").strong());
                    ui.label(format!(
                        "salience {:.2}; confidence {:.2}; strength {:.2}",
                        detail.salience, detail.confidence, detail.strength
                    ));
                    ui.end_row();

                    ui.label(RichText::new("Created").strong());
                    ui.label(memory_timestamp_label(detail.created_at_ms));
                    ui.end_row();

                    ui.label(RichText::new("Updated").strong());
                    ui.label(memory_timestamp_label(detail.updated_at_ms));
                    ui.end_row();
                });
            ui.add_space(6.0);
            ScrollArea::vertical()
                .id_salt("memory_detail_content")
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(&detail.content).wrap());
                });
            ui.add_space(8.0);
            ui.label(RichText::new("Edit content").strong());
            ui.add_sized(
                [ui.available_width(), 96.0],
                egui::TextEdit::multiline(&mut self.memory_edit_text).desired_rows(4),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let can_update = memory_ready
                    && detail.status == "active"
                    && !self.memory_update.is_updating()
                    && !self.memory_forget.is_forgetting()
                    && !self.memory_edit_text.trim().is_empty()
                    && self.memory_edit_text.trim() != detail.content.trim();
                if ui
                    .add_enabled(can_update, egui::Button::new("Update memory"))
                    .clicked()
                {
                    self.start_memory_update(&detail.memory_id);
                }
                if ui.button("Reset edit").clicked() {
                    self.memory_edit_text = detail.content.clone();
                }
            });
            ui.add_space(4.0);
            ui.label(memory_update_status(&self.memory_update));
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            ui.checkbox(
                &mut self.memory_forget_confirmed,
                "Allow forget for this memory",
            );
            ui.horizontal(|ui| {
                let can_forget = memory_ready
                    && detail.status == "active"
                    && self.memory_forget_confirmed
                    && !self.memory_update.is_updating()
                    && !self.memory_forget.is_forgetting();
                if ui
                    .add_enabled(can_forget, egui::Button::new("Forget memory"))
                    .clicked()
                {
                    self.start_memory_forget(&detail.memory_id);
                }
            });
            ui.add_space(4.0);
            ui.label(memory_forget_status(&self.memory_forget));
        }

        ui.add_space(14.0);
        ui.separator();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Contradictions").strong());
            let can_refresh = memory_ready && !self.memory_contradictions.is_loading();
            if ui
                .add_enabled(can_refresh, egui::Button::new("Refresh"))
                .clicked()
            {
                self.start_memory_contradictions_refresh();
            }
        });
        ui.add_space(4.0);
        ui.label(memory_contradiction_status(&self.memory_contradictions));
        ui.label(memory_contradiction_resolve_status(
            &self.memory_contradiction_resolve,
        ));
        if let Some(contradictions) = memory_contradiction_items(&self.memory_contradictions) {
            ui.add_space(4.0);
            if contradictions.is_empty() {
                ui.label("No contradictions have been flagged for this profile.");
            } else {
                ScrollArea::vertical()
                    .id_salt("memory_contradiction_results")
                    .max_height(230.0)
                    .show(ui, |ui| {
                        for (idx, contradiction) in contradictions.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&contradiction.status).strong());
                                ui.label(
                                    RichText::new(&contradiction.kind)
                                        .color(muted_text_color(dark_mode)),
                                );
                                ui.label(
                                    RichText::new(memory_timestamp_label(
                                        contradiction.detected_at_ms,
                                    ))
                                    .color(muted_text_color(dark_mode)),
                                );
                                if ui.button("Copy ids").clicked() {
                                    ui.ctx().copy_text(format!(
                                        "a_id={} b_id={} kind={}",
                                        contradiction.a_id, contradiction.b_id, contradiction.kind
                                    ));
                                }
                            });
                            ui.add(egui::Label::new(&contradiction.explanation).wrap());
                            ui.label(contradiction_side_label(
                                "A",
                                contradiction.a_triple.as_ref(),
                                &contradiction.a_id,
                            ));
                            ui.label(contradiction_side_label(
                                "B",
                                contradiction.b_triple.as_ref(),
                                &contradiction.b_id,
                            ));
                            if let Some(note) = contradiction.resolution_note.as_deref() {
                                ui.add(egui::Label::new(format!("Resolution: {note}")).wrap());
                            }
                            if let Some(resolved_at_ms) = contradiction.resolved_at_ms {
                                ui.label(
                                    RichText::new(format!(
                                        "Resolved {}",
                                        memory_timestamp_label(Some(resolved_at_ms))
                                    ))
                                    .color(muted_text_color(dark_mode)),
                                );
                            }
                            if let Some(winner) = contradiction.winning_triple_id.as_deref() {
                                ui.label(
                                    RichText::new(format!("Winner: {winner}"))
                                        .color(muted_text_color(dark_mode)),
                                );
                            }
                            ui.horizontal(|ui| {
                                let can_resolve = memory_ready
                                    && !self.memory_contradiction_resolve.is_resolving();
                                if contradiction.status == "resolved" {
                                    if ui
                                        .add_enabled(can_resolve, egui::Button::new("Reopen"))
                                        .clicked()
                                    {
                                        contradiction_resolve_action =
                                            Some(ContradictionResolveAction {
                                                a_id: contradiction.a_id.clone(),
                                                b_id: contradiction.b_id.clone(),
                                                kind: contradiction.kind.clone(),
                                                status: "reopened".to_string(),
                                                winning_triple_id: None,
                                            });
                                    }
                                } else {
                                    if ui
                                        .add_enabled(can_resolve, egui::Button::new("A current"))
                                        .clicked()
                                    {
                                        contradiction_resolve_action =
                                            Some(ContradictionResolveAction {
                                                a_id: contradiction.a_id.clone(),
                                                b_id: contradiction.b_id.clone(),
                                                kind: contradiction.kind.clone(),
                                                status: "resolved".to_string(),
                                                winning_triple_id: Some(contradiction.a_id.clone()),
                                            });
                                    }
                                    if ui
                                        .add_enabled(can_resolve, egui::Button::new("B current"))
                                        .clicked()
                                    {
                                        contradiction_resolve_action =
                                            Some(ContradictionResolveAction {
                                                a_id: contradiction.a_id.clone(),
                                                b_id: contradiction.b_id.clone(),
                                                kind: contradiction.kind.clone(),
                                                status: "resolved".to_string(),
                                                winning_triple_id: Some(contradiction.b_id.clone()),
                                            });
                                    }
                                }
                            });
                        }
                    });
            }
        }
        if let Some(action) = contradiction_resolve_action {
            self.start_memory_contradiction_resolve(
                action.a_id,
                action.b_id,
                action.kind,
                action.status,
                action.winning_triple_id,
            );
        }

        ui.add_space(14.0);
        ui.label(RichText::new("Scope").strong());
        ui.add_space(4.0);
        egui::Grid::new("memory_scope_grid")
            .num_columns(3)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Layer").strong());
                ui.label(RichText::new("Scope").strong());
                ui.label(RichText::new("Detail").strong());
                ui.end_row();

                ui.label("Agents");
                ui.label("shared");
                ui.add(egui::Label::new(memory_library_agents_description()).wrap());
                ui.end_row();

                ui.label("Memory library");
                ui.label("local");
                ui.add(
                    egui::Label::new("All Community memories live in one private local library.")
                        .wrap(),
                );
                ui.end_row();

                ui.label("Project context");
                ui.label(project_memory_state_label(self.project_snapshot.state));
                ui.add(egui::Label::new(project_memory_summary(&self.project_snapshot)).wrap());
                ui.end_row();
            });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Projects").clicked() {
                self.active_tab = MainTab::Projects;
            }
            if ui.button("Connected tools").clicked() {
                self.active_tab = MainTab::Tools;
            }
        });
    }

    fn draw_projects_tab(&mut self, ui: &mut egui::Ui) {
        let health = self.current_health();
        let dark_mode = ui.visuals().dark_mode;
        let mut inspect_memory_id: Option<String> = None;

        ui.heading("Projects");
        ui.add_space(8.0);
        ui.add(
            egui::Label::new(
                "Project memory uses `.solo/project.toml` to keep codebase context explicit.",
            )
            .wrap(),
        );

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_sized(
                [420.0, 28.0],
                egui::TextEdit::singleline(&mut self.project_root_input).hint_text("Project root"),
            );
            if ui.button("Save").clicked() {
                self.save_project_root_from_input();
            }
            if ui.button("Current dir").clicked() {
                self.use_current_dir_as_project_root();
            }
            if ui.button("Clear").clicked() {
                self.clear_project_root();
            }
            if ui.button("Refresh").clicked() {
                self.refresh_project_dependent_snapshots();
            }
        });

        ui.add_space(12.0);
        egui::Grid::new("project_memory_grid")
            .num_columns(3)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Item").strong());
                ui.label(RichText::new("State").strong());
                ui.label(RichText::new("Detail").strong());
                ui.end_row();

                render_state_row(
                    ui,
                    "Project root",
                    project_memory_state_label(self.project_snapshot.state),
                    project_memory_state_tone(self.project_snapshot.state),
                    &self
                        .project_snapshot
                        .root
                        .as_deref()
                        .map(display_path)
                        .unwrap_or_else(|| "not selected".to_string()),
                );
                render_state_row(
                    ui,
                    "Project config",
                    project_memory_config_label(&self.project_snapshot),
                    project_memory_state_tone(self.project_snapshot.state),
                    &self
                        .project_snapshot
                        .config_path
                        .as_deref()
                        .map(display_path)
                        .unwrap_or_else(|| ".solo/project.toml".to_string()),
                );
                let project_id = self
                    .project_snapshot
                    .config
                    .as_ref()
                    .map(|config| config.project_id.as_str())
                    .unwrap_or("not loaded");
                render_state_row(
                    ui,
                    "Project id",
                    project_id,
                    project_memory_state_tone(self.project_snapshot.state),
                    &project_memory_summary(&self.project_snapshot),
                );
                render_state_row(
                    ui,
                    "Active profile",
                    COMMUNITY_LIBRARY_KEY,
                    StateTone::Good,
                    "Project memories are still stored in the selected profile.",
                );
            });

        ui.add_space(14.0);
        ui.heading("Client Setup Scope");
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for scope in [
                WorkspaceAccessScope::GlobalOnly,
                WorkspaceAccessScope::ProjectOnly,
                WorkspaceAccessScope::GlobalAndProject,
            ] {
                if ui
                    .selectable_label(
                        self.state.settings.workspace_access_scope == scope,
                        scope.label(),
                    )
                    .on_hover_text(scope.detail())
                    .clicked()
                {
                    self.set_workspace_access_scope(scope);
                }
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(self.state.settings.workspace_access_scope.detail())
                .color(muted_text_color(dark_mode)),
        );
        ui.add_space(4.0);
        egui::Grid::new("workspace_permission_scope_grid")
            .num_columns(3)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Access").strong());
                ui.label(RichText::new("State").strong());
                ui.label(RichText::new("Detail").strong());
                ui.end_row();

                let (global_text, global_tone, global_detail) =
                    workspace_scope_global_status(self.state.settings.workspace_access_scope);
                render_state_row(
                    ui,
                    "Global memory",
                    &global_text,
                    global_tone,
                    &global_detail,
                );

                let (project_text, project_tone, project_detail) = workspace_scope_project_status(
                    self.state.settings.workspace_access_scope,
                    &self.project_snapshot,
                );
                render_state_row(
                    ui,
                    "Project memory",
                    &project_text,
                    project_tone,
                    &project_detail,
                );
            });

        ui.add_space(14.0);
        ui.heading("Import File Access");
        ui.add_space(4.0);
        egui::Grid::new("workspace_file_access_grid")
            .num_columns(3)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Item").strong());
                ui.label(RichText::new("State").strong());
                ui.label(RichText::new("Detail").strong());
                ui.end_row();

                render_state_row(
                    ui,
                    "Config",
                    workspace_file_access_state_label(self.workspace_file_access_snapshot.state),
                    workspace_file_access_state_tone(self.workspace_file_access_snapshot.state),
                    &self.workspace_file_access_snapshot.detail,
                );
                render_state_row(
                    ui,
                    "Allowed roots",
                    &workspace_file_access_roots_label(&self.workspace_file_access_snapshot),
                    workspace_file_access_state_tone(self.workspace_file_access_snapshot.state),
                    &workspace_file_access_roots_detail(&self.workspace_file_access_snapshot),
                );
                let (project_access_text, project_access_tone, project_access_detail) =
                    workspace_file_access_project_status(
                        &self.workspace_file_access_snapshot,
                        &self.project_snapshot,
                    );
                render_state_row(
                    ui,
                    "Project root",
                    &project_access_text,
                    project_access_tone,
                    &project_access_detail,
                );
                let (runtime_text, runtime_tone, runtime_detail) =
                    workspace_file_access_runtime_status(
                        &self.workspace_file_access_snapshot,
                        self.workspace_file_access_restart_required,
                    );
                render_state_row(ui, "Runtime", &runtime_text, runtime_tone, &runtime_detail);
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let can_restrict = project_root_exists(&self.project_snapshot)
                && self.workspace_file_access_snapshot.state
                    != WorkspaceFileAccessState::ConfigMissing;
            if ui
                .add_enabled(
                    can_restrict,
                    egui::Button::new("Restrict imports to project root"),
                )
                .clicked()
            {
                self.restrict_workspace_file_access_to_project_root();
            }
            if ui
                .add_enabled(
                    self.workspace_file_access_snapshot.state
                        != WorkspaceFileAccessState::ConfigMissing,
                    egui::Button::new("Allow all local imports"),
                )
                .clicked()
            {
                self.clear_workspace_file_access_restriction();
            }
            if ui.button("Refresh file access").clicked() {
                self.workspace_file_access_snapshot =
                    collect_workspace_file_access_snapshot(&self.setup_snapshot.data_dir);
            }
        });
        if let Some(message) = self.workspace_file_access_message.as_ref() {
            ui.add_space(4.0);
            ui.label(RichText::new(message).color(muted_text_color(dark_mode)));
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let has_root = project_root_exists(&self.project_snapshot);
            let project_busy = self.project_action.is_running();
            let can_init = !project_busy
                && can_run_project_action(
                    ProjectActionKind::Init,
                    &self.project_snapshot,
                    self.project_init_confirmed,
                );
            let can_preview = !project_busy
                && can_run_project_action(
                    ProjectActionKind::Preview,
                    &self.project_snapshot,
                    self.project_init_confirmed,
                );
            let can_confirm_init = !project_busy && can_offer_project_init(&self.project_snapshot);
            ui.add_enabled(
                can_confirm_init,
                egui::Checkbox::new(&mut self.project_init_confirmed, "Allow init write"),
            )
            .on_hover_text(
                "Allows Create project config to write `.solo/project.toml` without --force.",
            );
            if ui
                .add_enabled(can_init, egui::Button::new("Create project config"))
                .on_hover_text(project_action_unavailable_message(
                    ProjectActionKind::Init,
                    &self.project_snapshot,
                    self.project_init_confirmed,
                ))
                .clicked()
            {
                self.start_project_action(ProjectActionKind::Init);
            }
            if ui
                .add_enabled(can_preview, egui::Button::new("Preview docs"))
                .on_hover_text(project_action_unavailable_message(
                    ProjectActionKind::Preview,
                    &self.project_snapshot,
                    self.project_init_confirmed,
                ))
                .clicked()
            {
                self.start_project_action(ProjectActionKind::Preview);
            }
            if ui
                .add_enabled(has_root, egui::Button::new("Copy init"))
                .clicked()
            {
                if let Some(root) = &self.project_snapshot.root {
                    ui.ctx().copy_text(project_init_command(root));
                }
            }
            if ui
                .add_enabled(has_root, egui::Button::new("Copy preview command"))
                .clicked()
            {
                if let Some(root) = &self.project_snapshot.root {
                    ui.ctx().copy_text(project_ingest_dry_run_command(root));
                }
            }
            if ui
                .add_enabled(has_root, egui::Button::new("Copy Codex project setup"))
                .clicked()
            {
                if let Some(root) = &self.project_snapshot.root {
                    ui.ctx().copy_text(project_codex_setup_command(
                        root,
                        &mcp_url_from_status_url(&self.state.settings.status_url),
                    ));
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let can_copy_policy = can_copy_project_policy(&self.project_snapshot);
            ui.label(RichText::new("Agent policy").strong());
            for client in [
                ProjectPolicyClient::Codex,
                ProjectPolicyClient::Claude,
                ProjectPolicyClient::Cursor,
            ] {
                if ui
                    .add_enabled(
                        can_copy_policy,
                        egui::Button::new(format!("Copy {}", client.label())),
                    )
                    .on_hover_text(project_policy_status(&self.project_snapshot))
                    .clicked()
                    && let Some(policy) = project_agent_policy(&self.project_snapshot, client)
                {
                    ui.ctx().copy_text(policy);
                }
            }
            if ui
                .add_enabled(can_copy_policy, egui::Button::new("Copy command"))
                .on_hover_text(project_policy_status(&self.project_snapshot))
                .clicked()
                && let Some(root) = &self.project_snapshot.root
            {
                ui.ctx().copy_text(project_agent_policy_command(
                    root,
                    ProjectPolicyClient::Codex,
                ));
            }
        });
        ui.add_space(4.0);
        ui.label(project_policy_status(&self.project_snapshot));
        ui.add_space(4.0);
        ui.label(project_action_status(&self.project_action));
        if let Some(output) = project_action_output(&self.project_action) {
            ui.add_space(4.0);
            ScrollArea::vertical()
                .id_salt("project_preview_output")
                .max_height(160.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(output).monospace()).wrap());
                });
        }
        let project_docs_preview_summary = self
            .project_docs_preview
            .as_ref()
            .map(|preview| (preview.candidates.len(), preview.project_name.clone()));
        if let Some((candidate_count, project_name)) = project_docs_preview_summary {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Project docs").strong());
                ui.label(
                    RichText::new(format!(
                        "{} candidate(s) in {}",
                        candidate_count, project_name
                    ))
                    .color(muted_text_color(dark_mode)),
                );
            });
            ui.horizontal(|ui| {
                let can_import = can_import_project_docs(
                    self.project_docs_preview.as_ref(),
                    health,
                    self.project_docs_import.is_running(),
                    self.project_docs_import_confirmed,
                );
                ui.add_enabled(
                    !self.project_docs_import.is_running() && candidate_count > 0,
                    egui::Checkbox::new(
                        &mut self.project_docs_import_confirmed,
                        "Allow import into active profile",
                    ),
                )
                .on_hover_text("Imports exactly the previewed project docs through the daemon.");
                if ui
                    .add_enabled(can_import, egui::Button::new("Import previewed docs"))
                    .on_hover_text(project_docs_import_unavailable_message(
                        self.project_docs_preview.as_ref(),
                        health,
                        self.project_docs_import_confirmed,
                    ))
                    .clicked()
                {
                    self.start_project_docs_import();
                }
            });
            ui.label(project_docs_import_status(
                &self.project_docs_import,
                health,
                self.project_docs_preview.as_ref(),
            ));
            if let Some(output) = project_docs_import_output(&self.project_docs_import) {
                ui.add_space(4.0);
                ScrollArea::vertical()
                    .id_salt("project_docs_import_output")
                    .max_height(120.0)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(output).monospace()).wrap());
                    });
            }
        }

        ui.add_space(14.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(RichText::new("Project facts").strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let subject_width = (ui.available_width() - 216.0).max(180.0);
            let hint = self
                .project_snapshot
                .config
                .as_ref()
                .map(|config| format!("Subject, defaults to {}", config.name))
                .unwrap_or_else(|| "Subject".to_string());
            ui.add_sized(
                [subject_width, 28.0],
                egui::TextEdit::singleline(&mut self.project_fact_subject).hint_text(hint),
            );
            let can_load = can_load_project_facts(
                &self.project_snapshot,
                health,
                self.project_facts.is_loading(),
            );
            if ui
                .add_enabled(can_load, egui::Button::new("Load facts"))
                .on_hover_text(project_facts_unavailable_message(&self.project_snapshot))
                .clicked()
            {
                self.start_project_facts_refresh();
            }
            if ui
                .add_enabled(
                    project_decision_context(&self.project_snapshot).is_some(),
                    egui::Button::new("Copy JSON"),
                )
                .on_hover_text("Copy the equivalent `solo project facts --json` command.")
                .clicked()
                && let Some((root, _)) = project_decision_context(&self.project_snapshot)
            {
                ui.ctx().copy_text(project_facts_json_command(
                    root,
                    &self.project_fact_subject,
                    &self.setup_snapshot.data_dir,
                ));
            }
        });
        ui.add_space(4.0);
        ui.label(project_facts_status(
            &self.project_facts,
            &self.project_snapshot,
            health,
        ));
        if let Some((subject, facts)) = project_facts_results(&self.project_facts) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{} fact(s) for `{subject}`", facts.len()))
                    .color(muted_text_color(dark_mode)),
            );
            if facts.is_empty() {
                ui.label("No project facts found yet.");
            } else {
                ScrollArea::vertical()
                    .id_salt("project_facts_results")
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for (idx, fact) in facts.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&fact.predicate).strong());
                                ui.label(
                                    RichText::new(format!("conf {:.2}", fact.confidence))
                                        .color(muted_text_color(dark_mode)),
                                );
                                if ui.button("Copy triple id").clicked() {
                                    ui.ctx().copy_text(fact.triple_id.clone());
                                }
                                if let Some(cluster_id) = fact.cluster_id.as_ref()
                                    && ui.button("Copy cluster").clicked()
                                {
                                    ui.ctx().copy_text(cluster_id.clone());
                                }
                            });
                            ui.add(egui::Label::new(project_fact_label(fact)).wrap());
                            ui.label(
                                RichText::new(project_fact_validity_label(fact))
                                    .color(muted_text_color(dark_mode)),
                            );
                        }
                    });
            }
        }

        ui.add_space(14.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(RichText::new("Project decisions").strong());
        ui.add_space(4.0);
        ui.add_sized(
            [ui.available_width(), 72.0],
            egui::TextEdit::multiline(&mut self.project_decision_text)
                .desired_rows(3)
                .hint_text("A durable implementation decision for this project"),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_add = can_use_project_decisions(
                &self.project_snapshot,
                health,
                self.project_decision_action.is_running(),
            ) && !self.project_decision_text.trim().is_empty();
            if ui
                .add_enabled(can_add, egui::Button::new("Save decision"))
                .on_hover_text(project_decision_unavailable_message(&self.project_snapshot))
                .clicked()
            {
                self.start_project_decision_add();
            }
            if ui
                .add_enabled(
                    !self.project_decision_text.is_empty(),
                    egui::Button::new("Clear"),
                )
                .clicked()
            {
                self.project_decision_text.clear();
            }
            if ui
                .add_enabled(can_add, egui::Button::new("Copy JSON"))
                .on_hover_text("Copy the equivalent `solo project decisions --add --json` command.")
                .clicked()
                && let Some((root, _)) = project_decision_context(&self.project_snapshot)
            {
                ui.ctx().copy_text(project_decision_add_json_command(
                    root,
                    &self.project_decision_text,
                    &self.setup_snapshot.data_dir,
                ));
            }
        });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let search_width = (ui.available_width() - 196.0).max(180.0);
            ui.add_sized(
                [search_width, 28.0],
                egui::TextEdit::singleline(&mut self.project_decision_query)
                    .hint_text("Search decisions for this project"),
            );
            let can_search = can_use_project_decisions(
                &self.project_snapshot,
                health,
                self.project_decision_action.is_running(),
            ) && !self.project_decision_query.trim().is_empty();
            if ui
                .add_enabled(can_search, egui::Button::new("Search"))
                .on_hover_text(project_decision_unavailable_message(&self.project_snapshot))
                .clicked()
            {
                self.start_project_decision_search();
            }
            if ui
                .add_enabled(can_search, egui::Button::new("Copy JSON"))
                .on_hover_text(
                    "Copy the equivalent `solo project decisions --query --json` command.",
                )
                .clicked()
                && let Some((root, _)) = project_decision_context(&self.project_snapshot)
            {
                ui.ctx().copy_text(project_decision_search_json_command(
                    root,
                    &self.project_decision_query,
                    &self.setup_snapshot.data_dir,
                ));
            }
        });
        ui.add_space(4.0);
        ui.label(project_decision_status(
            &self.project_decision_action,
            &self.project_snapshot,
            health,
        ));
        if let Some((query, hits)) = project_decision_results(&self.project_decision_action) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{} decision result(s) for `{query}`", hits.len()))
                    .color(muted_text_color(dark_mode)),
            );
            if hits.is_empty() {
                ui.label("No matching project decisions found.");
            } else {
                ScrollArea::vertical()
                    .id_salt("project_decision_results")
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for (idx, hit) in hits.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&hit.tier).strong());
                                ui.label(
                                    RichText::new(format!("score {:.3}", hit.fused_score))
                                        .color(muted_text_color(dark_mode)),
                                );
                                if ui.button("Inspect").clicked() {
                                    inspect_memory_id = Some(hit.memory_id.clone());
                                }
                                if ui.button("Copy id").clicked() {
                                    ui.ctx().copy_text(hit.memory_id.clone());
                                }
                            });
                            ui.add(egui::Label::new(&hit.content).wrap());
                        }
                    });
            }
        }

        if let Some(memory_id) = inspect_memory_id {
            self.active_tab = MainTab::Memory;
            self.start_memory_inspect(&memory_id);
        }
    }

    fn draw_settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(8.0);

        let startup_enabled = autostart::is_enabled();
        egui::Grid::new("setup_detection_grid")
            .num_columns(3)
            .spacing([16.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Item").strong());
                ui.label(RichText::new("Status").strong());
                ui.label(RichText::new("Value").strong());
                ui.end_row();

                render_detection_row(
                    ui,
                    "solo-tray",
                    self.setup_snapshot.current_exe.is_some(),
                    self.setup_snapshot
                        .current_exe
                        .as_deref()
                        .map(display_path)
                        .unwrap_or_else(|| "current executable unavailable".to_string()),
                );
                render_detection_row(
                    ui,
                    "solo CLI sibling",
                    self.setup_snapshot.sibling_solo_exists,
                    display_path(&self.setup_snapshot.sibling_solo),
                );
                render_detection_row(
                    ui,
                    "solo CLI on PATH",
                    self.setup_snapshot.solo_on_path_exists,
                    solo_command_name().to_string(),
                );
                render_detection_row(
                    ui,
                    "tray settings",
                    self.setup_snapshot.settings_exists,
                    display_path(&self.state.settings_path),
                );
                render_detection_row(
                    ui,
                    "startup on login",
                    startup_enabled,
                    if startup_enabled {
                        "OS autostart entry is present".to_string()
                    } else if self.state.settings.autostart_on_login {
                        "setting is on, OS entry needs repair".to_string()
                    } else {
                        "off".to_string()
                    },
                );
                render_detection_row(
                    ui,
                    "data dir",
                    self.setup_snapshot.data_dir.is_dir(),
                    display_path(&self.setup_snapshot.data_dir),
                );
                render_detection_row(
                    ui,
                    "project root",
                    self.project_snapshot.state == ProjectMemoryState::Ready,
                    self.project_snapshot
                        .root
                        .as_deref()
                        .map(display_path)
                        .unwrap_or_else(|| "not selected".to_string()),
                );
                render_detection_row(
                    ui,
                    "import file access",
                    self.workspace_file_access_snapshot.state
                        == WorkspaceFileAccessState::Restricted,
                    workspace_file_access_state_label(self.workspace_file_access_snapshot.state)
                        .to_string(),
                );
                render_detection_row(
                    ui,
                    "Desktop URL",
                    true,
                    self.state.settings.solo_web_url.clone(),
                );
                render_detection_row(
                    ui,
                    "MCP HTTP URL",
                    true,
                    mcp_url_from_status_url(&self.state.settings.status_url),
                );
            });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !startup_enabled,
                    egui::Button::new("Repair startup on login"),
                )
                .clicked()
            {
                self.repair_startup_on_login();
            }
        });
        ui.add_space(12.0);
        egui::Grid::new("setup_settings_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Autostart").strong());
                ui.label(if self.state.settings.autostart_on_login {
                    "enabled"
                } else {
                    "disabled"
                });
                ui.end_row();

                ui.label(RichText::new("Notifications").strong());
                ui.label(if self.state.settings.notifications_enabled {
                    "enabled"
                } else {
                    "disabled"
                });
                ui.end_row();

                ui.label(RichText::new("Theme").strong());
                ui.label(format!("{:?}", self.state.settings.theme));
                ui.end_row();

                ui.label(RichText::new("Memory library").strong());
                ui.label("Community Memory Library");
                ui.end_row();

                ui.label(RichText::new("Setup guide").strong());
                ui.label(if self.state.settings.setup_wizard_completed {
                    "hidden"
                } else {
                    "visible"
                });
                ui.end_row();

                ui.label(RichText::new("Client config writes").strong());
                ui.label("apply writes backed-up client configs; verify is read-only");
                ui.end_row();
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Show setup guide").clicked() {
                self.set_setup_wizard_completed(false);
                self.active_tab = MainTab::Controls;
            }
            if ui.button("Hide setup guide").clicked() {
                self.set_setup_wizard_completed(true);
            }
        });

        ui.add_space(14.0);
        ui.label(RichText::new("Embedder Migration").strong());
        ui.add_space(4.0);
        let status = self.status_snapshot();
        let payload = status.last_payload.as_ref();
        egui::Grid::new("embedder_migration_status_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label("Current embedder");
                ui.label(status_embedder_summary(payload));
                ui.end_row();

                ui.label("Target");
                ui.label("Ollama local embeddings");
                ui.end_row();

                ui.label("State");
                ui.label(ollama_migration_status(&self.ollama_migration));
                ui.end_row();
            });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_sized(
                [180.0, 28.0],
                egui::TextEdit::singleline(&mut self.ollama_migration_model)
                    .hint_text("nomic-embed-text"),
            );
            ui.add_sized(
                [72.0, 28.0],
                egui::TextEdit::singleline(&mut self.ollama_migration_dim).hint_text("probe"),
            );
            ui.add_sized(
                [240.0, 28.0],
                egui::TextEdit::singleline(&mut self.ollama_migration_base_url)
                    .hint_text("http://localhost:11434"),
            );
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_sized(
                [260.0, 28.0],
                egui::TextEdit::singleline(&mut self.ollama_migration_passphrase)
                    .password(true)
                    .hint_text("Passphrase (blank uses keychain)"),
            );
            if ui
                .add_enabled(
                    !self.ollama_migration.is_running(),
                    egui::Button::new("Migrate to Ollama and restart"),
                )
                .clicked()
            {
                self.start_ollama_embedder_migration();
            }
            if ui.button("Copy CLI command").clicked() {
                ui.ctx().copy_text(ollama_migration_command(
                    &self.ollama_migration_model,
                    &self.ollama_migration_dim,
                    &self.ollama_migration_base_url,
                    &self.setup_snapshot.data_dir,
                ));
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("Stops Solo, backs up config and HNSW snapshots, re-embeds profiles, then starts Solo again.")
                .color(muted_text_color(ui.visuals().dark_mode)),
        );

        ui.add_space(14.0);
        ui.label(RichText::new("Secrets").strong());
        ui.add_space(4.0);
        egui::Grid::new("secrets_status_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label("Backend");
                ui.label(self.secret_snapshot.backend);
                ui.end_row();

                ui.label("Daemon passphrase");
                ui.label(secret_item_status(self.secret_snapshot.passphrase_stored));
                ui.end_row();

                ui.label("Bearer token");
                ui.label(secret_item_status(self.secret_snapshot.bearer_token_stored));
                ui.end_row();
            });
        ui.add_space(8.0);
        let mut remember = self.state.settings.remember_passphrase_in_keychain;
        if ui
            .checkbox(&mut remember, "Unlock daemon from OS keychain")
            .changed()
        {
            self.set_keychain_remember_enabled(remember);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_enabled(
                self.state.settings.remember_passphrase_in_keychain,
                egui::TextEdit::singleline(&mut self.keychain_passphrase_input)
                    .password(true)
                    .hint_text("Daemon passphrase"),
            );
            if ui
                .add_enabled(
                    self.state.settings.remember_passphrase_in_keychain,
                    egui::Button::new("Save passphrase"),
                )
                .clicked()
            {
                self.store_keychain_passphrase_from_input();
            }
            if ui
                .add_enabled(
                    self.state.settings.remember_passphrase_in_keychain,
                    egui::Button::new("Forget passphrase"),
                )
                .clicked()
            {
                self.forget_keychain_passphrase();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.bearer_token_input)
                    .password(true)
                    .hint_text("Bearer token"),
            );
            if ui.button("Save token").clicked() {
                self.store_bearer_token_from_input();
            }
            if ui.button("Forget token").clicked() {
                self.forget_bearer_token();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.state.settings.remember_passphrase_in_keychain,
                    egui::Button::new("Start from keychain"),
                )
                .clicked()
            {
                self.start_daemon_from_keychain();
            }
            if ui.button("Refresh keychain").clicked() {
                self.secret_snapshot = collect_secret_snapshot(true);
            }
        });
        ui.add_space(4.0);
        ui.label(secret_action_status(&self.secret_action));
        if self.secret_snapshot.last_error.is_some() {
            ui.label(
                RichText::new(secret_snapshot_status(&self.secret_snapshot))
                    .color(egui::Color32::from_rgb(220, 180, 60)),
            );
        }

        ui.add_space(12.0);
        ui.label(RichText::new("Setup-client command fallback").strong());
        ui.add_space(4.0);
        egui::Grid::new("setup_commands_grid")
            .num_columns(3)
            .spacing([12.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Target").strong());
                ui.label(RichText::new("Command").strong());
                ui.label("");
                ui.end_row();

                for (label, command) in setup_client_commands(
                    &self.state.settings.status_url,
                    &self.setup_snapshot.data_dir,
                ) {
                    render_command_row(ui, label, &command);
                }
            });
    }

    fn draw_data_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Data");
        ui.add_space(8.0);
        let health = self.current_health();
        if health == DaemonHealth::Healthy && matches!(self.document_list, DocumentListState::Idle)
        {
            self.start_document_list_refresh();
        }
        let mut inspect_document_id: Option<String> = None;

        egui::Grid::new("backup_status_grid")
            .num_columns(3)
            .spacing([16.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("Item").strong());
                ui.label(RichText::new("Status").strong());
                ui.label(RichText::new("Path").strong());
                ui.end_row();

                render_detection_row(
                    ui,
                    "data dir",
                    self.backup_snapshot.data_dir.is_dir(),
                    display_path(&self.backup_snapshot.data_dir),
                );
                render_detection_row(
                    ui,
                    "memory library",
                    self.backup_snapshot.db_path.is_file(),
                    display_path(&self.backup_snapshot.db_path),
                );
                render_detection_row(
                    ui,
                    "snapshots dir",
                    self.backup_snapshot.snapshots_dir.is_dir(),
                    display_path(&self.backup_snapshot.snapshots_dir),
                );
            });

        ui.add_space(14.0);
        ui.label(RichText::new("Import Preview").strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("import_source")
                .selected_text(self.import_source.picker_label())
                .show_ui(ui, |ui| {
                    for source in ImportSource::ALL {
                        ui.selectable_value(&mut self.import_source, source, source.picker_label());
                    }
                });
            ui.add_sized(
                [420.0, 28.0],
                egui::TextEdit::singleline(&mut self.import_path_input)
                    .hint_text("Import file or folder"),
            );
            let trimmed_path = self.import_path_input.trim().to_string();
            let path = PathBuf::from(&trimmed_path);
            let can_preview =
                !self.import_action.is_running() && !trimmed_path.is_empty() && path.exists();
            if ui
                .add_enabled(can_preview, egui::Button::new("Preview import"))
                .on_hover_text(import_preview_help(self.import_source))
                .clicked()
            {
                self.start_import_preview();
            }
            if ui
                .add_enabled(!trimmed_path.is_empty(), egui::Button::new("Copy command"))
                .clicked()
            {
                ui.ctx().copy_text(import_preview_command(
                    self.import_source,
                    &path,
                    &self.backup_snapshot.data_dir,
                ));
            }
        });
        ui.add_space(6.0);
        ui.label(import_action_status(&self.import_action));
        if let Some(output) = import_action_output(&self.import_action) {
            ui.add_space(4.0);
            ScrollArea::vertical()
                .id_salt("import_preview_output")
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(output).monospace()).wrap());
                });
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.import_commit_confirmed,
                "Allow import into active profile",
            );
            let trimmed_path = self.import_path_input.trim().to_string();
            let path = PathBuf::from(&trimmed_path);
            let preview_matches =
                import_preview_matches(&self.import_action, self.import_source, &path);
            let can_import = health == DaemonHealth::Healthy
                && self.import_commit_confirmed
                && preview_matches
                && !self.import_commit.is_running()
                && !trimmed_path.is_empty()
                && path.exists();
            if ui
                .add_enabled(can_import, egui::Button::new("Import previewed documents"))
                .on_hover_text(import_commit_help(self.import_source))
                .clicked()
            {
                self.start_document_import();
            }
            if self.import_commit_confirmed && !preview_matches {
                ui.label(
                    RichText::new("Preview the current source and path first.")
                        .color(warning_color(ui.visuals().dark_mode)),
                );
            }
        });
        ui.add_space(4.0);
        ui.label(import_commit_status(&self.import_commit, health));
        if let Some(output) = import_commit_output(&self.import_commit) {
            ui.add_space(4.0);
            ScrollArea::vertical()
                .id_salt("import_commit_output")
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(output).monospace()).wrap());
                });
        }

        ui.add_space(14.0);
        ui.separator();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Documents").strong());
            let can_refresh = health == DaemonHealth::Healthy && !self.document_list.is_loading();
            if ui
                .add_enabled(can_refresh, egui::Button::new("Refresh"))
                .clicked()
            {
                self.start_document_list_refresh();
            }
        });
        ui.add_space(4.0);
        ui.label(document_list_status(&self.document_list));
        if let Some(documents) = document_list_items(&self.document_list) {
            ui.add_space(4.0);
            if documents.is_empty() {
                ui.label("No documents in this profile yet.");
            } else {
                ScrollArea::vertical()
                    .id_salt("document_list_results")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for (idx, document) in documents.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&document.status).strong());
                                ui.label(
                                    RichText::new(format!("{} chunk(s)", document.chunk_count))
                                        .color(muted_text_color(ui.visuals().dark_mode)),
                                );
                                ui.label(
                                    RichText::new(memory_timestamp_label(document.ingested_at_ms))
                                        .color(muted_text_color(ui.visuals().dark_mode)),
                                );
                                if ui.button("Copy id").clicked() {
                                    ui.ctx().copy_text(document.doc_id.clone());
                                }
                                if ui.button("Inspect").clicked() {
                                    inspect_document_id = Some(document.doc_id.clone());
                                }
                            });
                            ui.label(RichText::new(document_title_label(document)).strong());
                            ui.add(egui::Label::new(document_source_label(document)).wrap());
                        }
                    });
            }
        }
        ui.add_space(12.0);
        ui.label(RichText::new("Search documents").strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let search_width = (ui.available_width() - 88.0).max(180.0);
            ui.add_sized(
                [search_width, 28.0],
                egui::TextEdit::singleline(&mut self.document_search_query)
                    .hint_text("Search imported document chunks"),
            );
            let can_search = health == DaemonHealth::Healthy
                && !self.document_search.is_searching()
                && !self.document_search_query.trim().is_empty();
            if ui
                .add_enabled(can_search, egui::Button::new("Search"))
                .clicked()
            {
                self.start_document_search();
            }
        });
        ui.add_space(4.0);
        ui.label(document_search_status(&self.document_search, health));
        if let Some((query, hits)) = document_search_results(&self.document_search) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{} chunk result(s) for `{query}`", hits.len()))
                    .color(muted_text_color(ui.visuals().dark_mode)),
            );
            if hits.is_empty() {
                ui.label("No matching document chunks found.");
            } else {
                ScrollArea::vertical()
                    .id_salt("document_search_results")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for (idx, hit) in hits.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("#{}", hit.chunk_index)).strong());
                                ui.label(
                                    RichText::new(format!("distance {:.3}", hit.cos_distance))
                                        .color(muted_text_color(ui.visuals().dark_mode)),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "chars {}-{}",
                                        hit.start_offset, hit.end_offset
                                    ))
                                    .color(muted_text_color(ui.visuals().dark_mode)),
                                );
                                if ui.button("Inspect").clicked() {
                                    inspect_document_id = Some(hit.doc_id.clone());
                                }
                                if ui.button("Copy doc id").clicked() {
                                    ui.ctx().copy_text(hit.doc_id.clone());
                                }
                                if ui.button("Copy chunk id").clicked() {
                                    ui.ctx().copy_text(hit.chunk_id.clone());
                                }
                            });
                            ui.label(RichText::new(document_search_hit_title(hit)).strong());
                            ui.add(egui::Label::new(document_search_hit_source(hit)).wrap());
                            ui.add(egui::Label::new(&hit.content).wrap());
                        }
                    });
            }
        }

        if let Some(doc_id) = inspect_document_id {
            self.start_document_inspect(&doc_id);
        }

        ui.add_space(10.0);
        ui.label(RichText::new("Selected document").strong());
        ui.add_space(4.0);
        ui.label(document_detail_status(&self.document_detail));
        if !matches!(self.document_detail, DocumentDetailState::Loaded { .. })
            && !matches!(self.document_forget, DocumentForgetState::Idle)
        {
            ui.label(document_forget_status(&self.document_forget));
        }
        if let Some(detail) = document_detail_loaded(&self.document_detail).cloned() {
            ui.add_space(4.0);
            egui::Grid::new("document_detail_grid")
                .num_columns(2)
                .spacing([14.0, 5.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label(RichText::new("ID").strong());
                    ui.horizontal(|ui| {
                        ui.label(&detail.doc_id);
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(detail.doc_id.clone());
                        }
                    });
                    ui.end_row();

                    ui.label(RichText::new("Title").strong());
                    ui.label(document_detail_title_label(&detail));
                    ui.end_row();

                    ui.label(RichText::new("Source").strong());
                    ui.add(egui::Label::new(document_detail_source_label(&detail)).wrap());
                    ui.end_row();

                    ui.label(RichText::new("Status").strong());
                    ui.label(&detail.status);
                    ui.end_row();

                    ui.label(RichText::new("Size").strong());
                    ui.label(document_detail_size_label(&detail));
                    ui.end_row();

                    ui.label(RichText::new("Ingested").strong());
                    ui.label(memory_timestamp_label(detail.ingested_at_ms));
                    ui.end_row();

                    ui.label(RichText::new("Modified").strong());
                    ui.label(memory_timestamp_label(detail.modified_at_ms));
                    ui.end_row();

                    ui.label(RichText::new("Hash").strong());
                    ui.label(detail.content_hash.as_deref().unwrap_or("hash unavailable"));
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            ui.checkbox(
                &mut self.document_forget_confirmed,
                "Allow forget for this document",
            );
            ui.horizontal(|ui| {
                let can_forget = health == DaemonHealth::Healthy
                    && detail.status == "active"
                    && self.document_forget_confirmed
                    && !self.document_forget.is_forgetting();
                if ui
                    .add_enabled(can_forget, egui::Button::new("Forget document"))
                    .clicked()
                {
                    self.start_document_forget(&detail.doc_id);
                }
            });
            ui.add_space(4.0);
            ui.label(document_forget_status(&self.document_forget));
            ui.add_space(8.0);
            ui.label(RichText::new("Chunks").strong());
            ScrollArea::vertical()
                .id_salt("document_detail_chunks")
                .max_height(220.0)
                .show(ui, |ui| {
                    if detail.chunks.is_empty() {
                        ui.label("No chunks for this document.");
                    } else {
                        for (idx, chunk) in detail.chunks.iter().enumerate() {
                            if idx > 0 {
                                ui.separator();
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("#{}", chunk.chunk_index)).strong());
                                ui.label(format!("{} token(s)", chunk.token_count));
                                if ui.button("Copy chunk id").clicked() {
                                    ui.ctx().copy_text(chunk.chunk_id.clone());
                                }
                            });
                            ui.add(egui::Label::new(&chunk.content_preview).wrap());
                        }
                    }
                });
        }

        ui.add_space(12.0);
        let backup_url = backup_url_from_status_url(&self.state.settings.status_url);
        let backup_dir = self.backup_snapshot.data_dir.join("backups");
        let backup_running = self.backup_action.is_running();
        ui.horizontal(|ui| {
            if ui.button("Open data dir").clicked() {
                open_path_async(self.backup_snapshot.data_dir.clone(), "data dir");
            }
            if ui.button("Copy backup folder").clicked() {
                ui.ctx().copy_text(display_path(&backup_dir));
            }
            let can_run_backup = health == DaemonHealth::Healthy && !backup_running;
            let label = if backup_running {
                "Backup running"
            } else {
                "Run backup now"
            };
            if ui
                .add_enabled(can_run_backup, egui::Button::new(label))
                .clicked()
            {
                self.start_daemon_backup();
            }
        });

        ui.add_space(8.0);
        egui::Grid::new("backup_readonly_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Tray backup action").strong());
                ui.label(backup_action_status(&self.backup_action, health));
                ui.end_row();

                ui.label(RichText::new("Backup route").strong());
                ui.label(backup_url);
                ui.end_row();

                ui.label(RichText::new("Backup folder").strong());
                ui.label(display_path(&backup_dir));
                ui.end_row();

                ui.label(RichText::new("Restore path").strong());
                ui.label("CLI/admin flow only");
                ui.end_row();

                ui.label(RichText::new("Latest known backup").strong());
                match &self.backup_snapshot.latest_known_backup {
                    Some(backup) => {
                        let age = backup
                            .modified
                            .map(format_age)
                            .unwrap_or_else(|| "modified time unavailable".to_string());
                        ui.label(format!("{} ({age})", backup.path.display()));
                    }
                    None => {
                        ui.label("none found in data dir or data-dir/backups");
                    }
                }
                ui.end_row();
            });
    }
}

fn collect_setup_snapshot(settings_path: &Path) -> SetupSnapshot {
    let data_dir = tray::resolve_data_dir();
    let current_exe = std::env::current_exe().ok();
    let sibling_solo = current_exe
        .as_deref()
        .and_then(Path::parent)
        .map(|dir| dir.join(solo_command_name()))
        .unwrap_or_else(|| PathBuf::from(solo_command_name()));
    let sibling_solo_exists = sibling_solo.is_file();
    let solo_on_path_exists = command_exists_on_path(solo_command_name());
    let solo_command_available = sibling_solo_exists || solo_on_path_exists;
    let settings_exists = settings_path.is_file();
    let solo_config_exists = data_dir.join("solo.config.toml").is_file();
    let lockfile = collect_lockfile_snapshot(&data_dir);

    SetupSnapshot {
        data_dir,
        current_exe,
        sibling_solo,
        sibling_solo_exists,
        solo_on_path_exists,
        solo_command_available,
        settings_exists,
        solo_config_exists,
        lockfile,
    }
}

fn collect_lockfile_snapshot(data_dir: &Path) -> LockfileSnapshot {
    let path = data_dir.join("solo.lock");
    if !path.is_file() {
        return LockfileSnapshot {
            path,
            state: LockfileState::Free,
            detail: "No daemon lock is present.".to_string(),
        };
    }

    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) => {
            return LockfileSnapshot {
                path,
                state: LockfileState::Unreadable,
                detail: format!("Lock file exists but could not be read: {error}"),
            };
        }
    };
    let raw = body.trim();
    let Some(pid) = raw.parse::<u32>().ok() else {
        return LockfileSnapshot {
            path,
            state: LockfileState::Stale,
            detail: format!("Lock file has an invalid PID ({raw:?}) and can be cleared."),
        };
    };

    match process_identity_for_pid(pid) {
        Some(identity) if identity.is_solo_lock_owner() => LockfileSnapshot {
            path,
            state: LockfileState::Held,
            detail: format!(
                "Held by Solo process PID {} ({}).",
                identity.pid, identity.name
            ),
        },
        Some(identity) => LockfileSnapshot {
            path,
            state: LockfileState::Stale,
            detail: format!(
                "Lock PID {} belongs to {}, not solo; it can be cleared before starting.",
                identity.pid, identity.name
            ),
        },
        None => LockfileSnapshot {
            path,
            state: LockfileState::Stale,
            detail: format!("Lock PID {pid} is not running and can be cleared."),
        },
    }
}

#[derive(Debug, Clone)]
struct ProcessIdentity {
    pid: u32,
    name: String,
    exe: Option<PathBuf>,
}

impl ProcessIdentity {
    fn is_solo_lock_owner(&self) -> bool {
        path_or_name_stem_is_solo(&self.name)
            || self
                .exe
                .as_deref()
                .and_then(Path::file_stem)
                .and_then(|stem| stem.to_str())
                .is_some_and(path_or_name_stem_is_solo)
    }
}

fn process_identity_for_pid(pid: u32) -> Option<ProcessIdentity> {
    let refresh = RefreshKind::new()
        .with_processes(ProcessRefreshKind::new().with_exe(UpdateKind::OnlyIfNotSet));
    let sys = System::new_with_specifics(refresh);
    let process = sys.process(Pid::from_u32(pid))?;
    Some(ProcessIdentity {
        pid,
        name: process.name().to_string_lossy().to_string(),
        exe: process.exe().map(Path::to_path_buf),
    })
}

fn path_or_name_stem_is_solo(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    let trimmed = normalized.strip_suffix(".exe").unwrap_or(&normalized);
    trimmed == "solo"
}

fn clear_stale_lockfile(snapshot: &LockfileSnapshot) -> Result<(), String> {
    if snapshot.state != LockfileState::Stale {
        return Err("lock file is not classified as stale".to_string());
    }
    std::fs::remove_file(&snapshot.path)
        .map_err(|error| format!("remove stale lock {}: {error}", snapshot.path.display()))
}

const WORKSPACE_FILE_ROOTS_ENV: &str = "SOLO_WORKSPACE_FILE_ROOTS";

fn collect_workspace_file_access_snapshot(data_dir: &Path) -> WorkspaceFileAccessSnapshot {
    let config_path = data_dir.join("solo.config.toml");
    if !config_path.is_file() {
        return workspace_file_access_snapshot(
            config_path,
            WorkspaceFileAccessState::ConfigMissing,
            Vec::new(),
            "solo.config.toml not found; create Solo memory from Solo Controls first",
        );
    }

    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(error) => {
            return workspace_file_access_snapshot(
                config_path,
                WorkspaceFileAccessState::InvalidConfig,
                Vec::new(),
                format!("read failed: {error}"),
            );
        }
    };
    let value = match raw.parse::<toml::Value>() {
        Ok(value) => value,
        Err(error) => {
            return workspace_file_access_snapshot(
                config_path,
                WorkspaceFileAccessState::InvalidConfig,
                Vec::new(),
                format!("toml parse failed: {error}"),
            );
        }
    };

    let Some(table) = value.as_table() else {
        return workspace_file_access_snapshot(
            config_path,
            WorkspaceFileAccessState::InvalidConfig,
            Vec::new(),
            "solo.config.toml root is not a TOML table",
        );
    };
    let Some(section) = table.get("workspace_file_access") else {
        return workspace_file_access_snapshot(
            config_path,
            WorkspaceFileAccessState::Unrestricted,
            Vec::new(),
            "file imports are unrestricted",
        );
    };
    let Some(section) = section.as_table() else {
        return workspace_file_access_snapshot(
            config_path,
            WorkspaceFileAccessState::InvalidConfig,
            Vec::new(),
            "[workspace_file_access] is not a table",
        );
    };
    let Some(allowed_roots) = section.get("allowed_roots") else {
        return workspace_file_access_snapshot(
            config_path,
            WorkspaceFileAccessState::Unrestricted,
            Vec::new(),
            "file imports are unrestricted",
        );
    };
    let Some(items) = allowed_roots.as_array() else {
        return workspace_file_access_snapshot(
            config_path,
            WorkspaceFileAccessState::InvalidConfig,
            Vec::new(),
            "workspace_file_access.allowed_roots is not an array",
        );
    };
    let mut roots = Vec::with_capacity(items.len());
    for item in items {
        match item.as_str() {
            Some(root) => roots.push(root.to_string()),
            None => {
                return workspace_file_access_snapshot(
                    config_path,
                    WorkspaceFileAccessState::InvalidConfig,
                    Vec::new(),
                    "workspace_file_access.allowed_roots contains a non-string value",
                );
            }
        }
    }
    let state = if roots.is_empty() {
        WorkspaceFileAccessState::Disabled
    } else {
        WorkspaceFileAccessState::Restricted
    };
    let detail = if roots.is_empty() {
        "file imports are disabled until an allowed root is configured".to_string()
    } else {
        "file imports must stay under the allowed roots".to_string()
    };
    workspace_file_access_snapshot(config_path, state, roots, detail)
}

fn workspace_file_access_snapshot(
    config_path: PathBuf,
    state: WorkspaceFileAccessState,
    allowed_roots: Vec<String>,
    detail: impl Into<String>,
) -> WorkspaceFileAccessSnapshot {
    WorkspaceFileAccessSnapshot {
        config_path,
        state,
        allowed_roots,
        env_override: workspace_file_access_env_override(),
        detail: detail.into(),
    }
}

fn workspace_file_access_env_override() -> Option<String> {
    std::env::var_os(WORKSPACE_FILE_ROOTS_ENV)
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty())
}

fn set_workspace_file_access_allowed_roots(
    config_path: &Path,
    roots: Option<Vec<PathBuf>>,
) -> Result<PathBuf, String> {
    if !config_path.is_file() {
        return Err(format!(
            "solo.config.toml not found at {}",
            config_path.display()
        ));
    }
    let raw = std::fs::read_to_string(config_path)
        .map_err(|e| format!("read {}: {e}", config_path.display()))?;
    let mut value = raw
        .parse::<toml::Value>()
        .map_err(|e| format!("parse {}: {e}", config_path.display()))?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| "solo.config.toml root is not a TOML table".to_string())?;

    match roots {
        Some(roots) => {
            let mut allowed = Vec::with_capacity(roots.len());
            for root in roots {
                let canonical = std::fs::canonicalize(&root)
                    .map_err(|e| format!("project root {} is not readable: {e}", root.display()))?;
                if !canonical.is_dir() {
                    return Err(format!(
                        "project root {} is not a directory",
                        root.display()
                    ));
                }
                allowed.push(toml::Value::String(display_user_path(&canonical)));
            }
            let entry = table
                .entry("workspace_file_access".to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            let section = entry.as_table_mut().ok_or_else(|| {
                "[workspace_file_access] already exists but is not a table".to_string()
            })?;
            section.insert("allowed_roots".to_string(), toml::Value::Array(allowed));
        }
        None => {
            if let Some(section) = table.get_mut("workspace_file_access") {
                let section = section.as_table_mut().ok_or_else(|| {
                    "[workspace_file_access] already exists but is not a table".to_string()
                })?;
                section.remove("allowed_roots");
                if section.is_empty() {
                    table.remove("workspace_file_access");
                }
            }
        }
    }

    let body =
        toml::to_string_pretty(&value).map_err(|e| format!("serialize solo.config.toml: {e}"))?;
    replace_solo_config_with_backup(config_path, &body)
}

fn replace_solo_config_with_backup(config_path: &Path, body: &str) -> Result<PathBuf, String> {
    let parent = config_path
        .parent()
        .ok_or_else(|| format!("config path has no parent: {}", config_path.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let stem = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("solo.config.toml");
    let backup_path = parent.join(format!("{stem}.tray-backup-{stamp}"));
    let tmp_path = parent.join(format!("{stem}.tray-tmp-{stamp}"));

    std::fs::copy(config_path, &backup_path)
        .map_err(|e| format!("backup {}: {e}", config_path.display()))?;
    {
        let mut tmp_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| format!("open temp config {}: {e}", tmp_path.display()))?;
        std::io::Write::write_all(&mut tmp_file, body.as_bytes())
            .map_err(|e| format!("write temp config {}: {e}", tmp_path.display()))?;
        tmp_file
            .sync_all()
            .map_err(|e| format!("sync temp config {}: {e}", tmp_path.display()))?;
    }
    preserve_original_config_permissions(config_path, &tmp_path)?;

    match std::fs::rename(&tmp_path, config_path) {
        Ok(()) => Ok(backup_path),
        Err(first_error) => {
            if let Err(remove_error) = std::fs::remove_file(config_path) {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!(
                    "replace {} failed: {first_error}; remove existing file failed: {remove_error}",
                    config_path.display()
                ));
            }
            match std::fs::rename(&tmp_path, config_path) {
                Ok(()) => Ok(backup_path),
                Err(second_error) => match std::fs::copy(&backup_path, config_path) {
                    Ok(_) => Err(format!(
                        "replace {} failed: {second_error}; restored backup {}",
                        config_path.display(),
                        backup_path.display()
                    )),
                    Err(restore_error) => Err(format!(
                        "replace {} failed: {second_error}; restore from backup {} failed: {restore_error}",
                        config_path.display(),
                        backup_path.display()
                    )),
                },
            }
        }
    }
}

fn preserve_original_config_permissions(config_path: &Path, tmp_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(config_path)
            .map_err(|e| format!("read permissions {}: {e}", config_path.display()))?
            .permissions()
            .mode();
        let permissions = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(tmp_path, permissions)
            .map_err(|e| format!("set permissions {}: {e}", tmp_path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (config_path, tmp_path);
    }
    Ok(())
}

fn solo_command_name() -> &'static str {
    if cfg!(windows) { "solo.exe" } else { "solo" }
}

fn command_exists_on_path(command: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| command_exists_in_paths(command, std::env::split_paths(&paths)))
        .unwrap_or(false)
}

fn command_exists_in_paths<I>(command: &str, paths: I) -> bool
where
    I: IntoIterator<Item = PathBuf>,
{
    paths.into_iter().any(|dir| dir.join(command).is_file())
}

fn collect_tool_snapshot(
    last_statuses: &std::collections::BTreeMap<String, ConnectedToolLastStatus>,
    project_root: Option<&Path>,
    active_profile: &str,
) -> ToolSnapshot {
    let rows = SetupTarget::ALL
        .into_iter()
        .map(|target| {
            let mut row = inspect_tool_config(target, project_root);
            row.last_status = connected_tool_last_status(
                last_statuses,
                target,
                &row.profile_route,
                active_profile,
            );
            row
        })
        .collect();
    ToolSnapshot { rows }
}

fn inspect_tool_config(target: SetupTarget, project_root: Option<&Path>) -> ToolConfigRow {
    let detection = detect_tool_config_path(target, project_root);
    let Some(path) = detection.path else {
        return ToolConfigRow {
            target,
            path: None,
            state: ToolConfigState::Unknown,
            transport: ToolTransport::Unknown,
            profile_route: ToolProfileRoute::Unknown,
            detail: detection
                .note
                .unwrap_or_else(|| "config path unavailable".to_string()),
            last_status: None,
        };
    };

    let (state, transport, profile_route, detail) =
        if matches!(target, SetupTarget::CodexUser | SetupTarget::CodexProject) {
            inspect_toml_tool_config(&path)
        } else {
            inspect_json_tool_config(&path)
        };

    ToolConfigRow {
        target,
        path: Some(path),
        state,
        transport,
        profile_route,
        detail,
        last_status: None,
    }
}

fn connected_tool_status_key(target: SetupTarget, profile: &str) -> String {
    format!("{}@{}", target.key(), profile)
}

fn connected_tool_history_profile(route: &ToolProfileRoute, active_profile: &str) -> String {
    match route {
        ToolProfileRoute::DaemonDefault | ToolProfileRoute::Unknown => active_profile.to_string(),
        #[cfg(test)]
        ToolProfileRoute::Explicit(profile) if !profile.is_empty() => profile.clone(),
        #[cfg(test)]
        ToolProfileRoute::Explicit(_) => active_profile.to_string(),
    }
}

fn connected_tool_last_status(
    last_statuses: &std::collections::BTreeMap<String, ConnectedToolLastStatus>,
    target: SetupTarget,
    route: &ToolProfileRoute,
    active_profile: &str,
) -> Option<ConnectedToolLastStatus> {
    let profile = connected_tool_history_profile(route, active_profile);
    let scoped_key = connected_tool_status_key(target, &profile);
    if let Some(status) = last_statuses.get(&scoped_key) {
        return Some(status.clone());
    }
    let legacy = last_statuses.get(target.key())?;
    match legacy
        .resolved_profile
        .as_deref()
        .filter(|profile| !profile.is_empty())
    {
        Some(resolved) if resolved != profile => None,
        _ => Some(legacy.clone()),
    }
}

struct ToolPathDetection {
    path: Option<PathBuf>,
    note: Option<String>,
}

fn detect_tool_config_path(target: SetupTarget, project_root: Option<&Path>) -> ToolPathDetection {
    let env_var = |key: &str| std::env::var_os(key);
    detect_tool_config_path_for_os(target, std::env::consts::OS, &env_var, project_root)
}

fn detect_tool_config_path_for_os<F>(
    target: SetupTarget,
    os: &str,
    env_var: &F,
    project_root: Option<&Path>,
) -> ToolPathDetection
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    match target {
        SetupTarget::ClaudeDesktop => detect_claude_config_path_for_os(os, env_var),
        SetupTarget::Cursor => home_dir_for_os(os, env_var)
            .map(|home| ToolPathDetection {
                path: Some(home.join(".cursor").join("mcp.json")),
                note: None,
            })
            .unwrap_or_else(|| missing_home_detection_for_os(os)),
        SetupTarget::CodexUser => home_dir_for_os(os, env_var)
            .map(|home| ToolPathDetection {
                path: Some(home.join(".codex").join("config.toml")),
                note: None,
            })
            .unwrap_or_else(|| missing_home_detection_for_os(os)),
        SetupTarget::CodexProject => project_root
            .map(|root| ToolPathDetection {
                path: Some(root.join(".codex").join("config.toml")),
                note: None,
            })
            .unwrap_or_else(|| ToolPathDetection {
                path: None,
                note: Some("select a project root in Projects".to_string()),
            }),
    }
}

fn detect_claude_config_path_for_os<F>(os: &str, env_var: &F) -> ToolPathDetection
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    match os {
        "windows" => env_var("APPDATA")
            .map(PathBuf::from)
            .map(|base| ToolPathDetection {
                path: Some(base.join("Claude").join("claude_desktop_config.json")),
                note: None,
            })
            .unwrap_or_else(|| ToolPathDetection {
                path: None,
                note: Some("APPDATA is not set".to_string()),
            }),
        "macos" => home_dir_for_os(os, env_var)
            .map(|home| ToolPathDetection {
                path: Some(
                    home.join("Library")
                        .join("Application Support")
                        .join("Claude")
                        .join("claude_desktop_config.json"),
                ),
                note: None,
            })
            .unwrap_or_else(|| missing_home_detection_for_os(os)),
        _ => home_dir_for_os(os, env_var)
            .map(|home| ToolPathDetection {
                path: Some(
                    home.join(".config")
                        .join("Claude")
                        .join("claude_desktop_config.json"),
                ),
                note: None,
            })
            .unwrap_or_else(|| missing_home_detection_for_os(os)),
    }
}

fn home_dir_for_os<F>(os: &str, env_var: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    if os == "windows" {
        if let Some(profile) = env_var("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        let drive = env_var("HOMEDRIVE")?;
        let path = env_var("HOMEPATH")?;
        let mut joined = drive;
        joined.push(path);
        return Some(PathBuf::from(joined));
    }
    env_var("HOME").map(PathBuf::from)
}

fn missing_home_detection_for_os(os: &str) -> ToolPathDetection {
    ToolPathDetection {
        path: None,
        note: Some(if os == "windows" {
            "USERPROFILE is not set".to_string()
        } else {
            "HOME is not set".to_string()
        }),
    }
}

fn inspect_json_tool_config(
    path: &Path,
) -> (ToolConfigState, ToolTransport, ToolProfileRoute, String) {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (
                ToolConfigState::NeedsSetup,
                ToolTransport::None,
                ToolProfileRoute::Unknown,
                "config file does not exist".to_string(),
            );
        }
        Err(error) => {
            return (
                ToolConfigState::NeedsRepair,
                ToolTransport::Unknown,
                ToolProfileRoute::Unknown,
                format!("read config failed: {error}"),
            );
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(json) => json,
        Err(error) => {
            return (
                ToolConfigState::NeedsRepair,
                ToolTransport::Unknown,
                ToolProfileRoute::Unknown,
                format!("malformed JSON: {error}"),
            );
        }
    };
    inspect_json_tool_value(&json)
}

fn inspect_json_tool_value(
    json: &serde_json::Value,
) -> (ToolConfigState, ToolTransport, ToolProfileRoute, String) {
    let Some(root) = json.as_object() else {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "config root is not a JSON object".to_string(),
        );
    };
    let Some(mcp_servers) = root.get("mcpServers").and_then(|value| value.as_object()) else {
        return (
            ToolConfigState::NeedsSetup,
            ToolTransport::None,
            ToolProfileRoute::Unknown,
            "`mcpServers.solo` is not configured".to_string(),
        );
    };
    let Some(solo) = mcp_servers.get("solo") else {
        return (
            ToolConfigState::NeedsSetup,
            ToolTransport::None,
            ToolProfileRoute::Unknown,
            "`mcpServers.solo` is not configured".to_string(),
        );
    };
    let Some(server) = solo.as_object() else {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "`mcpServers.solo` is not a JSON object".to_string(),
        );
    };
    if json_contains_passphrase_reference(solo) {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "`mcpServers.solo` contains SOLO_PASSPHRASE".to_string(),
        );
    }
    if json_contains_bearer_authorization_reference(solo) {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "`mcpServers.solo` contains an Authorization bearer token".to_string(),
        );
    }
    let command = server
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let args = server
        .get("args")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if command.is_empty() || !server.get("args").is_some_and(|value| value.is_array()) {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "`mcpServers.solo.command` and `args` must be configured".to_string(),
        );
    }

    let transport = classify_json_tool_transport(command, &args);
    let profile_route = json_tool_profile_route(command, &args);
    let detail = tool_config_detail("`mcpServers.solo` is configured", &profile_route);
    (ToolConfigState::Verified, transport, profile_route, detail)
}

fn classify_json_tool_transport(command: &str, args: &[serde_json::Value]) -> ToolTransport {
    let arg_strings: Vec<&str> = args.iter().filter_map(|arg| arg.as_str()).collect();
    if command == "solo" && arg_strings.contains(&"mcp-stdio") {
        return ToolTransport::Stdio;
    }
    if command == "npx" && arg_strings.contains(&"mcp-remote") {
        return ToolTransport::HttpBridge;
    }
    ToolTransport::Unknown
}

fn json_tool_profile_route(command: &str, args: &[serde_json::Value]) -> ToolProfileRoute {
    let arg_strings: Vec<&str> = args.iter().filter_map(|arg| arg.as_str()).collect();
    if (command == "solo" && arg_strings.contains(&"mcp-stdio"))
        || (command == "npx" && arg_strings.contains(&"mcp-remote"))
    {
        return ToolProfileRoute::DaemonDefault;
    }
    ToolProfileRoute::Unknown
}

fn profile_from_args(args: &[&str]) -> ToolProfileRoute {
    let _ = args;
    ToolProfileRoute::DaemonDefault
}

fn tool_config_detail(prefix: &str, profile_route: &ToolProfileRoute) -> String {
    let _ = profile_route;
    prefix.to_string()
}

fn inspect_toml_tool_config(
    path: &Path,
) -> (ToolConfigState, ToolTransport, ToolProfileRoute, String) {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (
                ToolConfigState::NeedsSetup,
                ToolTransport::None,
                ToolProfileRoute::Unknown,
                "config file does not exist".to_string(),
            );
        }
        Err(error) => {
            return (
                ToolConfigState::NeedsRepair,
                ToolTransport::Unknown,
                ToolProfileRoute::Unknown,
                format!("read config failed: {error}"),
            );
        }
    };
    let toml: toml::Value = match toml::from_str(&raw) {
        Ok(toml) => toml,
        Err(error) => {
            return (
                ToolConfigState::NeedsRepair,
                ToolTransport::Unknown,
                ToolProfileRoute::Unknown,
                format!("malformed TOML: {error}"),
            );
        }
    };
    inspect_toml_tool_value(&toml)
}

fn inspect_toml_tool_value(
    toml: &toml::Value,
) -> (ToolConfigState, ToolTransport, ToolProfileRoute, String) {
    let Some(root) = toml.as_table() else {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "config root is not a TOML table".to_string(),
        );
    };
    let Some(mcp_servers) = root.get("mcp_servers").and_then(|value| value.as_table()) else {
        return (
            ToolConfigState::NeedsSetup,
            ToolTransport::None,
            ToolProfileRoute::Unknown,
            "`mcp_servers.solo` is not configured".to_string(),
        );
    };
    let Some(solo) = mcp_servers.get("solo") else {
        return (
            ToolConfigState::NeedsSetup,
            ToolTransport::None,
            ToolProfileRoute::Unknown,
            "`mcp_servers.solo` is not configured".to_string(),
        );
    };
    let Some(server) = solo.as_table() else {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "`mcp_servers.solo` is not a TOML table".to_string(),
        );
    };
    if toml_contains_passphrase_reference(solo) {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "`mcp_servers.solo` contains SOLO_PASSPHRASE".to_string(),
        );
    }
    if toml_contains_bearer_authorization_reference(solo) {
        return (
            ToolConfigState::NeedsRepair,
            ToolTransport::Unknown,
            ToolProfileRoute::Unknown,
            "`mcp_servers.solo` contains an Authorization bearer token".to_string(),
        );
    }
    let has_url = server
        .get("url")
        .and_then(|value| value.as_str())
        .is_some_and(|url| !url.is_empty());
    let command = server
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if has_url {
        let profile_route = toml_http_profile_route(server);
        return (
            ToolConfigState::Verified,
            ToolTransport::Http,
            profile_route.clone(),
            tool_config_detail("`mcp_servers.solo` uses HTTP", &profile_route),
        );
    }
    if command == "solo" {
        let profile_route = toml_stdio_profile_route(server);
        return (
            ToolConfigState::Verified,
            ToolTransport::Stdio,
            profile_route.clone(),
            tool_config_detail("`mcp_servers.solo` uses stdio", &profile_route),
        );
    }
    (
        ToolConfigState::NeedsRepair,
        ToolTransport::Unknown,
        ToolProfileRoute::Unknown,
        "`mcp_servers.solo.url` or `command` must be configured".to_string(),
    )
}

fn toml_http_profile_route(server: &toml::map::Map<String, toml::Value>) -> ToolProfileRoute {
    let _ = server;
    ToolProfileRoute::DaemonDefault
}

fn toml_stdio_profile_route(server: &toml::map::Map<String, toml::Value>) -> ToolProfileRoute {
    let args = server
        .get("args")
        .and_then(|value| value.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    profile_from_args(&args)
}

fn json_contains_passphrase_reference(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains("SOLO_PASSPHRASE"),
        serde_json::Value::Array(items) => items.iter().any(json_contains_passphrase_reference),
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("SOLO_PASSPHRASE") || json_contains_passphrase_reference(value)
        }),
        _ => false,
    }
}

fn json_contains_bearer_authorization_reference(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => is_authorization_bearer_header(text),
        serde_json::Value::Array(items) => items
            .iter()
            .any(json_contains_bearer_authorization_reference),
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            (key.eq_ignore_ascii_case("authorization") && json_value_contains_bearer_scheme(value))
                || json_contains_bearer_authorization_reference(value)
        }),
        _ => false,
    }
}

fn json_value_contains_bearer_scheme(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => starts_with_bearer_scheme(text),
        serde_json::Value::Array(items) => items.iter().any(json_value_contains_bearer_scheme),
        serde_json::Value::Object(map) => map.values().any(json_value_contains_bearer_scheme),
        _ => false,
    }
}

fn toml_contains_passphrase_reference(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(text) => text.contains("SOLO_PASSPHRASE"),
        toml::Value::Array(items) => items.iter().any(toml_contains_passphrase_reference),
        toml::Value::Table(map) => map.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("SOLO_PASSPHRASE") || toml_contains_passphrase_reference(value)
        }),
        _ => false,
    }
}

fn toml_contains_bearer_authorization_reference(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(text) => is_authorization_bearer_header(text),
        toml::Value::Array(items) => items
            .iter()
            .any(toml_contains_bearer_authorization_reference),
        toml::Value::Table(map) => map.iter().any(|(key, value)| {
            (key.eq_ignore_ascii_case("authorization") && toml_value_contains_bearer_scheme(value))
                || toml_contains_bearer_authorization_reference(value)
        }),
        _ => false,
    }
}

fn toml_value_contains_bearer_scheme(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(text) => starts_with_bearer_scheme(text),
        toml::Value::Array(items) => items.iter().any(toml_value_contains_bearer_scheme),
        toml::Value::Table(map) => map.values().any(toml_value_contains_bearer_scheme),
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

fn collect_backup_snapshot() -> BackupSnapshot {
    let data_dir = tray::resolve_data_dir();
    let db_path = data_dir.join("solo.db");
    let snapshots_dir = data_dir.join("snapshots");
    let latest_known_backup = latest_known_backup(&data_dir);

    BackupSnapshot {
        data_dir,
        db_path,
        snapshots_dir,
        latest_known_backup,
    }
}

fn collect_library_snapshot(data_dir: &Path) -> LibrarySnapshot {
    let db_path = data_dir.join("solo.db");
    match std::fs::metadata(&db_path) {
        Ok(metadata) => LibrarySnapshot {
            db_path,
            exists: metadata.is_file(),
            size_bytes: metadata.is_file().then_some(metadata.len()),
            last_error: None,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LibrarySnapshot {
            db_path,
            exists: false,
            size_bytes: None,
            last_error: None,
        },
        Err(error) => LibrarySnapshot {
            db_path: db_path.clone(),
            exists: false,
            size_bytes: None,
            last_error: Some(format!(
                "memory library check failed for {}: {error}",
                db_path.display()
            )),
        },
    }
}

fn collect_project_memory_snapshot(root: Option<&Path>) -> ProjectMemorySnapshot {
    let Some(root) = root else {
        return ProjectMemorySnapshot {
            root: None,
            config_path: None,
            state: ProjectMemoryState::NotSelected,
            config: None,
            detail: "choose a project root to inspect `.solo/project.toml`".to_string(),
        };
    };
    let root = root.to_path_buf();
    if !root.is_dir() {
        return ProjectMemorySnapshot {
            root: Some(root),
            config_path: None,
            state: ProjectMemoryState::MissingRoot,
            config: None,
            detail: "project root is not a directory".to_string(),
        };
    }
    let config_path = root.join(".solo").join("project.toml");
    if !config_path.is_file() {
        return ProjectMemorySnapshot {
            root: Some(root),
            config_path: Some(config_path),
            state: ProjectMemoryState::MissingConfig,
            config: None,
            detail: "run `solo project init` to create project memory config".to_string(),
        };
    }
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(error) => {
            return ProjectMemorySnapshot {
                root: Some(root),
                config_path: Some(config_path),
                state: ProjectMemoryState::InvalidConfig,
                config: None,
                detail: format!("read project config failed: {error}"),
            };
        }
    };
    let value: toml::Value = match toml::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            return ProjectMemorySnapshot {
                root: Some(root),
                config_path: Some(config_path),
                state: ProjectMemoryState::InvalidConfig,
                config: None,
                detail: format!("project config is invalid TOML: {error}"),
            };
        }
    };
    let Some(project) = value.get("project").and_then(|value| value.as_table()) else {
        return ProjectMemorySnapshot {
            root: Some(root),
            config_path: Some(config_path),
            state: ProjectMemoryState::InvalidConfig,
            config: None,
            detail: "project config is missing [project]".to_string(),
        };
    };
    let name = project
        .get("name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_project_name(&root));
    let project_id = project
        .get("id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| slugify_project_id(&name));
    let tags = project
        .get("tags")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    ProjectMemorySnapshot {
        root: Some(root),
        config_path: Some(config_path),
        state: ProjectMemoryState::Ready,
        detail: format!("project `{project_id}` is configured"),
        config: Some(ProjectMemoryConfig {
            name,
            project_id,
            tags,
        }),
    }
}

fn project_root_exists(snapshot: &ProjectMemorySnapshot) -> bool {
    snapshot.root.is_some() && snapshot.state != ProjectMemoryState::MissingRoot
}

fn collect_secret_snapshot(query_keychain: bool) -> SecretSnapshot {
    let backend = crate::secret_store::backend_label();
    if !query_keychain {
        return SecretSnapshot {
            backend,
            passphrase_stored: None,
            bearer_token_stored: None,
            last_error: None,
        };
    }

    let mut errors = Vec::new();
    let passphrase_stored = match crate::secret_store::has_daemon_passphrase() {
        Ok(stored) => Some(stored),
        Err(error) => {
            errors.push(format!("daemon passphrase: {error}"));
            None
        }
    };
    let bearer_token_stored = match crate::secret_store::has_bearer_token() {
        Ok(stored) => Some(stored),
        Err(error) => {
            errors.push(format!("bearer token: {error}"));
            None
        }
    };
    SecretSnapshot {
        backend,
        passphrase_stored,
        bearer_token_stored,
        last_error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

fn latest_known_backup(data_dir: &Path) -> Option<BackupFile> {
    let candidates = [data_dir.to_path_buf(), data_dir.join("backups")];
    let mut latest: Option<BackupFile> = None;
    for dir in candidates {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !looks_like_backup_file(&path) {
                continue;
            }
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
            let replace = match (&latest, modified) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(prev), Some(modified)) => prev.modified.map(|p| modified > p).unwrap_or(true),
            };
            if replace {
                latest = Some(BackupFile { path, modified });
            }
        }
    }
    latest
}

fn looks_like_backup_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let file_name = file_name.to_ascii_lowercase();
    file_name.contains("backup") && file_name.ends_with(".db")
}

fn setup_client_commands(status_url: &str, data_dir: &Path) -> Vec<(&'static str, String)> {
    let mcp_url = mcp_url_from_status_url(status_url);
    let data_dir = shell_arg(&display_path(data_dir));
    vec![
        ("List clients", "solo setup-client list".to_string()),
        (
            "Doctor",
            format!("solo setup-client doctor --url {}", shell_arg(&mcp_url)),
        ),
        (
            "Claude Code HTTP",
            format!(
                "claude mcp add --transport http --scope user solo {}",
                shell_arg(&mcp_url)
            ),
        ),
        (
            "Claude Desktop HTTP",
            format!(
                "solo setup-client claude-desktop --transport http --url {} --dry-run",
                shell_arg(&mcp_url)
            ),
        ),
        (
            "Cursor HTTP",
            format!(
                "solo setup-client cursor --transport http --url {} --dry-run",
                shell_arg(&mcp_url)
            ),
        ),
        (
            "Codex HTTP (user)",
            format!(
                "solo setup-client codex --scope user --transport http --url {} --dry-run",
                shell_arg(&mcp_url)
            ),
        ),
        (
            "Codex HTTP (project)",
            format!(
                "solo setup-client codex --scope project --transport http --url {} --dry-run",
                shell_arg(&mcp_url)
            ),
        ),
        (
            "Claude Desktop stdio preview",
            format!(
                "solo setup-client claude-desktop --transport stdio --data-dir {data_dir} --dry-run"
            ),
        ),
        (
            "Cursor stdio preview",
            format!("solo setup-client cursor --transport stdio --data-dir {data_dir} --dry-run"),
        ),
        (
            "Codex stdio preview",
            format!(
                "solo setup-client codex --scope user --transport stdio --data-dir {data_dir} --dry-run"
            ),
        ),
    ]
}

fn setup_client_command_block(status_url: &str, data_dir: &Path) -> String {
    setup_client_commands(status_url, data_dir)
        .into_iter()
        .map(|(_, command)| command)
        .collect::<Vec<_>>()
        .join("\n")
}

fn mcp_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/mcp"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/mcp".to_string())
}

fn backup_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/backup"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/backup".to_string())
}

fn memory_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/memory".to_string())
}

fn memory_search_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/search"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/memory/search".to_string())
}

fn memory_context_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/context"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/memory/context".to_string())
}

fn project_facts_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/v1/project/facts"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/v1/project/facts".to_string())
}

fn project_decision_add_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/v1/project/decisions"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/v1/project/decisions".to_string())
}

fn project_decision_search_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/v1/project/decisions/search"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/v1/project/decisions/search".to_string())
}

fn memory_inspect_url_from_status_url(status_url: &str, memory_id: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/{memory_id}"))
        .unwrap_or_else(|| format!("http://127.0.0.1:17821/memory/{memory_id}"))
}

fn memory_forget_url_from_status_url(status_url: &str, memory_id: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/{memory_id}?reason=solo_desktop"))
        .unwrap_or_else(|| format!("http://127.0.0.1:17821/memory/{memory_id}?reason=solo_desktop"))
}

fn memory_contradictions_url_from_status_url(status_url: &str, limit: usize) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/contradictions?limit={limit}"))
        .unwrap_or_else(|| format!("http://127.0.0.1:17821/memory/contradictions?limit={limit}"))
}

fn memory_contradiction_resolve_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/contradictions/resolve"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/memory/contradictions/resolve".to_string())
}

fn memory_documents_import_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/documents/import"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/memory/documents/import".to_string())
}

fn memory_documents_list_url_from_status_url(status_url: &str, limit: usize) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/documents?limit={limit}&offset=0"))
        .unwrap_or_else(|| {
            format!("http://127.0.0.1:17821/memory/documents?limit={limit}&offset=0")
        })
}

fn memory_documents_search_url_from_status_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/documents/search"))
        .unwrap_or_else(|| "http://127.0.0.1:17821/memory/documents/search".to_string())
}

fn memory_document_inspect_url_from_status_url(status_url: &str, doc_id: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/memory/documents/{doc_id}"))
        .unwrap_or_else(|| format!("http://127.0.0.1:17821/memory/documents/{doc_id}"))
}

fn memory_document_forget_url_from_status_url(status_url: &str, doc_id: &str) -> String {
    memory_document_inspect_url_from_status_url(status_url, doc_id)
}

fn memory_inbox_url_from_status_url(status_url: &str, limit: usize) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/v1/inbox?limit={limit}"))
        .unwrap_or_else(|| format!("http://127.0.0.1:17821/v1/inbox?limit={limit}"))
}

fn memory_inbox_review_url_from_status_url(status_url: &str, memory_id: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(|base| format!("{base}/v1/inbox/{memory_id}/review"))
        .unwrap_or_else(|| format!("http://127.0.0.1:17821/v1/inbox/{memory_id}/review"))
}

fn supervisor_state_text(state: &SupervisorState) -> String {
    match state {
        SupervisorState::Locked => "locked".to_string(),
        SupervisorState::Starting => "starting".to_string(),
        SupervisorState::Running => "running".to_string(),
        SupervisorState::Restarting => "restarting".to_string(),
        SupervisorState::Crashed(msg) => format!("crashed: {msg}"),
        SupervisorState::StartupFailed(msg) => format!("startup failed: {msg}"),
        SupervisorState::Stopped => "stopped".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateTone {
    Good,
    Warn,
    Bad,
}

fn daemon_lifecycle_label(
    state: Option<&SupervisorState>,
    health: DaemonHealth,
    dark_mode: bool,
) -> RichText {
    let (text, tone, _) = daemon_lifecycle_status(state, health);
    state_text(&text, tone, dark_mode).strong()
}

fn daemon_lifecycle_status(
    state: Option<&SupervisorState>,
    health: DaemonHealth,
) -> (String, StateTone, String) {
    match state {
        Some(SupervisorState::Locked) => (
            "Daemon locked".to_string(),
            StateTone::Warn,
            "Solo needs your passphrase to unlock local memory.".to_string(),
        ),
        Some(SupervisorState::StartupFailed(msg)) => (
            "Start failed".to_string(),
            StateTone::Bad,
            format!("Check passphrase or settings: {msg}"),
        ),
        Some(SupervisorState::Stopped) => (
            "Daemon stopped".to_string(),
            StateTone::Bad,
            "Start Solo when you want memory and MCP available.".to_string(),
        ),
        Some(SupervisorState::Starting) => (
            "Daemon starting".to_string(),
            StateTone::Warn,
            "Waiting for /v1/status.".to_string(),
        ),
        Some(SupervisorState::Restarting) => (
            "Daemon restarting".to_string(),
            StateTone::Warn,
            "Applying the selected profile or settings.".to_string(),
        ),
        Some(SupervisorState::Running) if health == DaemonHealth::Healthy => (
            "Daemon running".to_string(),
            StateTone::Good,
            daemon_ready_clients_description().to_string(),
        ),
        Some(SupervisorState::Running) => (
            "Daemon starting".to_string(),
            StateTone::Warn,
            "Process is alive; HTTP is warming up.".to_string(),
        ),
        Some(SupervisorState::Crashed(msg)) => (
            "Daemon crashed".to_string(),
            StateTone::Bad,
            format!("Supervisor will retry. Last exit: {msg}"),
        ),
        None => (
            "Daemon busy".to_string(),
            StateTone::Warn,
            "Supervisor state is refreshing.".to_string(),
        ),
    }
}

fn lockfile_status(snapshot: &LockfileSnapshot) -> (String, StateTone, String) {
    match snapshot.state {
        LockfileState::Free => (
            "free".to_string(),
            StateTone::Good,
            "No startup lock is blocking Solo.".to_string(),
        ),
        LockfileState::Stale => (
            "stale lock".to_string(),
            StateTone::Warn,
            snapshot.detail.clone(),
        ),
        LockfileState::Held => ("held".to_string(), StateTone::Warn, snapshot.detail.clone()),
        LockfileState::Unreadable => (
            "check needed".to_string(),
            StateTone::Bad,
            snapshot.detail.clone(),
        ),
    }
}

fn passphrase_status(
    state: Option<&SupervisorState>,
    secret: &SecretSnapshot,
    remember_passphrase_in_keychain: bool,
) -> (String, StateTone, String) {
    match state {
        Some(
            SupervisorState::Locked | SupervisorState::StartupFailed(_) | SupervisorState::Stopped,
        )
        | None => {
            let detail = if !remember_passphrase_in_keychain {
                "Keychain unlock is off; enter passphrase to start Solo.".to_string()
            } else {
                match secret.passphrase_stored {
                    Some(true) => "Stored keychain secret can start Solo.".to_string(),
                    Some(false) => "Enter passphrase to start Solo.".to_string(),
                    None => "Enter passphrase or refresh keychain state.".to_string(),
                }
            };
            ("Enter passphrase".to_string(), StateTone::Warn, detail)
        }
        Some(SupervisorState::Running) => (
            "Accepted".to_string(),
            StateTone::Good,
            "Daemon has the passphrase for this session.".to_string(),
        ),
        Some(SupervisorState::Starting | SupervisorState::Restarting) => (
            "Starting Solo".to_string(),
            StateTone::Warn,
            "Passphrase was sent to the daemon.".to_string(),
        ),
        Some(SupervisorState::Crashed(_)) => (
            "Retrying".to_string(),
            StateTone::Warn,
            "Supervisor is keeping the daemon alive with the current passphrase.".to_string(),
        ),
    }
}

fn library_status(snapshot: &LibrarySnapshot) -> (String, StateTone, String) {
    if let Some(error) = snapshot.last_error.as_ref() {
        return (
            "Library unavailable".to_string(),
            StateTone::Bad,
            error.to_string(),
        );
    }
    if !snapshot.exists {
        return (
            "Library not found".to_string(),
            StateTone::Warn,
            display_path(&snapshot.db_path),
        );
    }
    let detail = match snapshot.size_bytes {
        Some(size_bytes) => format!(
            "Community memory library ready at {} ({})",
            display_path(&snapshot.db_path),
            format_bytes(size_bytes)
        ),
        None => format!(
            "Community memory library ready at {}",
            display_path(&snapshot.db_path)
        ),
    };
    ("Library ready".to_string(), StateTone::Good, detail)
}

fn project_memory_state_label(state: ProjectMemoryState) -> &'static str {
    match state {
        ProjectMemoryState::NotSelected => "not selected",
        ProjectMemoryState::MissingRoot => "missing root",
        ProjectMemoryState::MissingConfig => "needs init",
        ProjectMemoryState::Ready => "configured",
        ProjectMemoryState::InvalidConfig => "needs repair",
    }
}

fn project_memory_state_tone(state: ProjectMemoryState) -> StateTone {
    match state {
        ProjectMemoryState::Ready => StateTone::Good,
        ProjectMemoryState::NotSelected | ProjectMemoryState::MissingConfig => StateTone::Warn,
        ProjectMemoryState::MissingRoot | ProjectMemoryState::InvalidConfig => StateTone::Bad,
    }
}

fn project_memory_config_label(snapshot: &ProjectMemorySnapshot) -> &'static str {
    match snapshot.state {
        ProjectMemoryState::Ready => "found",
        ProjectMemoryState::MissingConfig => "missing",
        ProjectMemoryState::InvalidConfig => "invalid",
        ProjectMemoryState::NotSelected => "not selected",
        ProjectMemoryState::MissingRoot => "unavailable",
    }
}

fn project_memory_summary(snapshot: &ProjectMemorySnapshot) -> String {
    if let Some(config) = snapshot.config.as_ref() {
        let tags = if config.tags.is_empty() {
            "no tags".to_string()
        } else {
            format!("tags: {}", config.tags.join(", "))
        };
        return format!("{} (`{}`); {tags}", config.name, config.project_id);
    }
    snapshot.detail.clone()
}

fn workspace_file_access_state_label(state: WorkspaceFileAccessState) -> &'static str {
    match state {
        WorkspaceFileAccessState::ConfigMissing => "config missing",
        WorkspaceFileAccessState::Unrestricted => "unrestricted",
        WorkspaceFileAccessState::Restricted => "restricted",
        WorkspaceFileAccessState::Disabled => "disabled",
        WorkspaceFileAccessState::InvalidConfig => "needs repair",
    }
}

fn workspace_file_access_state_tone(state: WorkspaceFileAccessState) -> StateTone {
    match state {
        WorkspaceFileAccessState::Restricted => StateTone::Good,
        WorkspaceFileAccessState::Unrestricted
        | WorkspaceFileAccessState::ConfigMissing
        | WorkspaceFileAccessState::Disabled => StateTone::Warn,
        WorkspaceFileAccessState::InvalidConfig => StateTone::Bad,
    }
}

fn workspace_file_access_roots_label(snapshot: &WorkspaceFileAccessSnapshot) -> String {
    match snapshot.state {
        WorkspaceFileAccessState::Restricted => format!("{} root(s)", snapshot.allowed_roots.len()),
        WorkspaceFileAccessState::Disabled => "none".to_string(),
        WorkspaceFileAccessState::Unrestricted => "all readable files".to_string(),
        WorkspaceFileAccessState::ConfigMissing => "unknown".to_string(),
        WorkspaceFileAccessState::InvalidConfig => "invalid".to_string(),
    }
}

fn workspace_file_access_roots_detail(snapshot: &WorkspaceFileAccessSnapshot) -> String {
    if snapshot.allowed_roots.is_empty() {
        return display_path(&snapshot.config_path);
    }
    snapshot.allowed_roots.join("; ")
}

fn workspace_file_access_project_status(
    access: &WorkspaceFileAccessSnapshot,
    project: &ProjectMemorySnapshot,
) -> (String, StateTone, String) {
    let Some(root) = project.root.as_ref() else {
        return (
            "Needs project root".to_string(),
            StateTone::Warn,
            "Select a project root before restricting file imports.".to_string(),
        );
    };
    let root_canonical = std::fs::canonicalize(root).ok();
    let root_display = root_canonical
        .as_deref()
        .map(display_user_path)
        .unwrap_or_else(|| display_user_path(root));
    match access.state {
        WorkspaceFileAccessState::Restricted => {
            if access.allowed_roots.iter().any(|allowed| {
                root_allowed_by_workspace_file_access(
                    root_canonical.as_deref(),
                    &root_display,
                    allowed,
                )
            }) {
                (
                    "Allowed".to_string(),
                    StateTone::Good,
                    "Selected project root is in workspace_file_access.allowed_roots.".to_string(),
                )
            } else {
                (
                    "Outside allowed roots".to_string(),
                    StateTone::Warn,
                    "Use Restrict imports to project root or clear the import restriction."
                        .to_string(),
                )
            }
        }
        WorkspaceFileAccessState::Unrestricted => (
            "Unrestricted".to_string(),
            StateTone::Warn,
            "Any readable local file can be imported by daemon HTTP/MCP requests.".to_string(),
        ),
        WorkspaceFileAccessState::Disabled => (
            "Blocked".to_string(),
            StateTone::Warn,
            "File imports are disabled because allowed_roots is empty.".to_string(),
        ),
        WorkspaceFileAccessState::ConfigMissing => (
            "Unavailable".to_string(),
            StateTone::Warn,
            "Create Solo memory before changing daemon file access.".to_string(),
        ),
        WorkspaceFileAccessState::InvalidConfig => (
            "Needs repair".to_string(),
            StateTone::Bad,
            access.detail.clone(),
        ),
    }
}

fn workspace_file_access_runtime_status(
    access: &WorkspaceFileAccessSnapshot,
    restart_required: bool,
) -> (String, StateTone, String) {
    if restart_required {
        return (
            "Restart required".to_string(),
            StateTone::Warn,
            "The config file changed; restart Solo before the daemon enforces this policy."
                .to_string(),
        );
    }
    if let Some(value) = access.env_override.as_ref() {
        return (
            "Env override".to_string(),
            StateTone::Warn,
            format!(
                "{WORKSPACE_FILE_ROOTS_ENV} is set for this process; daemon launches use it before solo.config.toml: {value}"
            ),
        );
    }
    match access.state {
        WorkspaceFileAccessState::Restricted => (
            "Startup policy".to_string(),
            StateTone::Good,
            "The daemon reads this allow-list when Solo starts.".to_string(),
        ),
        WorkspaceFileAccessState::Unrestricted => (
            "Startup policy".to_string(),
            StateTone::Warn,
            "The daemon starts with unrestricted file imports unless an environment override is set."
                .to_string(),
        ),
        WorkspaceFileAccessState::Disabled => (
            "Startup policy".to_string(),
            StateTone::Warn,
            "The daemon starts with daemon-side file imports disabled.".to_string(),
        ),
        WorkspaceFileAccessState::ConfigMissing => (
            "Unavailable".to_string(),
            StateTone::Warn,
            "Create Solo memory before daemon file access can be configured.".to_string(),
        ),
        WorkspaceFileAccessState::InvalidConfig => (
            "Needs repair".to_string(),
            StateTone::Bad,
            access.detail.clone(),
        ),
    }
}

fn root_allowed_by_workspace_file_access(
    root_canonical: Option<&Path>,
    root_display: &str,
    allowed: &str,
) -> bool {
    if let (Some(root), Ok(allowed_canonical)) = (
        root_canonical,
        std::fs::canonicalize(PathBuf::from(allowed)),
    ) {
        if root == allowed_canonical || root.starts_with(allowed_canonical) {
            return true;
        }
    }
    let allowed = display_user_path(Path::new(allowed));
    root_display == allowed || path_string_contains_root(root_display, &allowed)
}

fn path_string_contains_root(path: &str, root: &str) -> bool {
    let path = path.trim_end_matches(['/', '\\']);
    let root = root.trim_end_matches(['/', '\\']);
    path == root || path.starts_with(&format!("{root}/")) || path.starts_with(&format!("{root}\\"))
}

fn workspace_access_scope_allows_global(scope: WorkspaceAccessScope) -> bool {
    matches!(
        scope,
        WorkspaceAccessScope::GlobalOnly | WorkspaceAccessScope::GlobalAndProject
    )
}

fn workspace_access_scope_allows_project(scope: WorkspaceAccessScope) -> bool {
    matches!(
        scope,
        WorkspaceAccessScope::ProjectOnly | WorkspaceAccessScope::GlobalAndProject
    )
}

fn workspace_access_scope_allows_target(scope: WorkspaceAccessScope, target: SetupTarget) -> bool {
    match target {
        SetupTarget::CodexProject => workspace_access_scope_allows_project(scope),
        SetupTarget::CodexUser | SetupTarget::ClaudeDesktop | SetupTarget::Cursor => {
            workspace_access_scope_allows_global(scope)
        }
    }
}

fn workspace_access_target_ready(
    scope: WorkspaceAccessScope,
    target: SetupTarget,
    snapshot: &ProjectMemorySnapshot,
) -> bool {
    if !workspace_access_scope_allows_target(scope, target) {
        return false;
    }
    target != SetupTarget::CodexProject || project_root_exists(snapshot)
}

fn workspace_scope_global_status(scope: WorkspaceAccessScope) -> (String, StateTone, String) {
    if workspace_access_scope_allows_global(scope) {
        (
            "Allowed".to_string(),
            StateTone::Good,
            "User-level client setup targets the active profile.".to_string(),
        )
    } else {
        (
            "Blocked by scope".to_string(),
            StateTone::Warn,
            "Project-only mode avoids setting up user-level global memory clients.".to_string(),
        )
    }
}

fn workspace_scope_project_status(
    scope: WorkspaceAccessScope,
    snapshot: &ProjectMemorySnapshot,
) -> (String, StateTone, String) {
    if !workspace_access_scope_allows_project(scope) {
        return (
            "Blocked by scope".to_string(),
            StateTone::Warn,
            "Global-only mode avoids setting up project-scoped memory clients.".to_string(),
        );
    }
    if !project_root_exists(snapshot) {
        return (
            "Needs project root".to_string(),
            StateTone::Warn,
            "Select a project root before using project-scoped client setup.".to_string(),
        );
    }
    (
        project_memory_state_label(snapshot.state).to_string(),
        project_memory_state_tone(snapshot.state),
        project_memory_summary(snapshot),
    )
}

fn workspace_scope_target_status(
    scope: WorkspaceAccessScope,
    target: SetupTarget,
    snapshot: &ProjectMemorySnapshot,
) -> (String, StateTone, String) {
    match target {
        SetupTarget::CodexProject => workspace_scope_project_status(scope, snapshot),
        SetupTarget::CodexUser | SetupTarget::ClaudeDesktop | SetupTarget::Cursor => {
            workspace_scope_global_status(scope)
        }
    }
}

fn workspace_scope_action_detail(allowed: bool, access_detail: &str, fallback: &str) -> String {
    if allowed {
        fallback.to_string()
    } else {
        access_detail.to_string()
    }
}

fn can_offer_project_init(snapshot: &ProjectMemorySnapshot) -> bool {
    snapshot.root.is_some() && snapshot.state == ProjectMemoryState::MissingConfig
}

fn can_preview_project_docs(snapshot: &ProjectMemorySnapshot) -> bool {
    project_root_exists(snapshot) && snapshot.state != ProjectMemoryState::InvalidConfig
}

fn can_copy_project_policy(snapshot: &ProjectMemorySnapshot) -> bool {
    project_policy_context(snapshot).is_some()
}

fn can_run_project_action(
    kind: ProjectActionKind,
    snapshot: &ProjectMemorySnapshot,
    init_confirmed: bool,
) -> bool {
    match kind {
        ProjectActionKind::Init => can_offer_project_init(snapshot) && init_confirmed,
        ProjectActionKind::Preview => can_preview_project_docs(snapshot),
    }
}

fn project_action_unavailable_message(
    kind: ProjectActionKind,
    snapshot: &ProjectMemorySnapshot,
    init_confirmed: bool,
) -> String {
    if can_run_project_action(kind, snapshot, init_confirmed) {
        return match kind {
            ProjectActionKind::Init => {
                "write `.solo/project.toml` without overwriting an existing config".to_string()
            }
            ProjectActionKind::Preview => {
                "scan README/docs/ADR files without opening the database".to_string()
            }
        };
    }

    match kind {
        ProjectActionKind::Init => match snapshot.state {
            ProjectMemoryState::NotSelected => "select a project root first".to_string(),
            ProjectMemoryState::MissingRoot => "project root is missing".to_string(),
            ProjectMemoryState::MissingConfig if !init_confirmed => {
                "confirm the init write before creating `.solo/project.toml`".to_string()
            }
            ProjectMemoryState::MissingConfig => "ready to create project config".to_string(),
            ProjectMemoryState::Ready => "project config already exists".to_string(),
            ProjectMemoryState::InvalidConfig => {
                "repair or delete `.solo/project.toml` before creating a new config".to_string()
            }
        },
        ProjectActionKind::Preview => match snapshot.state {
            ProjectMemoryState::NotSelected => "select a project root first".to_string(),
            ProjectMemoryState::MissingRoot => "project root is missing".to_string(),
            ProjectMemoryState::MissingConfig => {
                "project docs can be previewed with the default scan rules".to_string()
            }
            ProjectMemoryState::Ready => "ready to preview project docs".to_string(),
            ProjectMemoryState::InvalidConfig => {
                "repair or delete `.solo/project.toml` before running project actions".to_string()
            }
        },
    }
}

fn project_policy_status(snapshot: &ProjectMemorySnapshot) -> String {
    match snapshot.state {
        ProjectMemoryState::Ready => {
            "Copy repo-scoped memory rules for a coding agent.".to_string()
        }
        ProjectMemoryState::MissingConfig => {
            "Copy a policy with a default project id, or create `.solo/project.toml` first."
                .to_string()
        }
        ProjectMemoryState::NotSelected => {
            "select a project root before copying policy".to_string()
        }
        ProjectMemoryState::MissingRoot => "project root is missing".to_string(),
        ProjectMemoryState::InvalidConfig => {
            "repair `.solo/project.toml` before copying project policy".to_string()
        }
    }
}

fn project_action_status(action: &ProjectActionState) -> String {
    match action {
        ProjectActionState::Idle => {
            "Preview docs scans the selected root without opening the encrypted database."
                .to_string()
        }
        ProjectActionState::Running {
            kind,
            root,
            started_at,
        } => format!(
            "{} running for {} ({}s)",
            kind.label(),
            display_path(root),
            started_at.elapsed().as_secs()
        ),
        ProjectActionState::Succeeded {
            kind,
            message,
            completed_at,
            ..
        } => format!(
            "{} succeeded: {message} ({})",
            kind.label(),
            format_age(*completed_at)
        ),
        ProjectActionState::Failed {
            kind,
            message,
            completed_at,
        } => format!(
            "{} failed: {message} ({})",
            kind.label(),
            format_age(*completed_at)
        ),
    }
}

fn project_action_output(action: &ProjectActionState) -> Option<&str> {
    match action {
        ProjectActionState::Succeeded {
            kind: ProjectActionKind::Preview,
            output,
            ..
        } if !output.is_empty() => Some(output),
        _ => None,
    }
}

fn project_action_kind(action: &ProjectActionState) -> Option<ProjectActionKind> {
    match action {
        ProjectActionState::Running { kind, .. }
        | ProjectActionState::Succeeded { kind, .. }
        | ProjectActionState::Failed { kind, .. } => Some(*kind),
        ProjectActionState::Idle => None,
    }
}

fn parse_project_docs_preview(body: &str) -> Result<ProjectDocsPreview, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode project preview JSON: {e}"))?;
    let project = json.get("project");
    let root = json
        .get("root")
        .or_else(|| project.and_then(|value| value.get("root")))
        .and_then(|value| value.as_str())
        .ok_or_else(|| "project preview missing root".to_string())?
        .to_string();
    let candidates_value = json
        .get("candidates")
        .or_else(|| json.get("candidate_paths"))
        .and_then(|value| value.as_array())
        .ok_or_else(|| "project preview missing candidates array".to_string())?;
    let candidates = candidates_value
        .iter()
        .map(|candidate| parse_project_doc_candidate(&root, candidate))
        .collect::<Result<Vec<_>, _>>()?;
    let project_name = json
        .get("project_name")
        .or_else(|| project.and_then(|value| value.get("name")))
        .and_then(|value| value.as_str())
        .unwrap_or("project")
        .to_string();
    let project_id = json
        .get("project_id")
        .or_else(|| project.and_then(|value| value.get("project_id")))
        .or_else(|| project.and_then(|value| value.get("id")))
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok(ProjectDocsPreview {
        root,
        project_name,
        project_id,
        files_scanned: json
            .get("files_scanned")
            .or_else(|| json.pointer("/counts/files_scanned"))
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        candidates_found: json
            .get("candidates_found")
            .or_else(|| json.pointer("/counts/candidate_files"))
            .or_else(|| json.get("candidate_count"))
            .and_then(|value| value.as_u64())
            .unwrap_or(candidates.len() as u64) as usize,
        truncated: json
            .get("truncated")
            .or_else(|| json.pointer("/counts/truncated"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        candidates,
    })
}

fn parse_project_doc_candidate(
    root: &str,
    value: &serde_json::Value,
) -> Result<ProjectDocCandidate, String> {
    if let Some(path) = value.as_str() {
        return Ok(ProjectDocCandidate {
            path: project_candidate_path_from_preview(root, path),
            label: path.to_string(),
        });
    }
    let path = value
        .get("path")
        .or_else(|| value.get("absolute_path"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| "project doc candidate missing path".to_string())?;
    let label = value
        .get("relative_path")
        .or_else(|| value.get("display_path"))
        .and_then(|value| value.as_str())
        .unwrap_or(path);
    Ok(ProjectDocCandidate {
        path: project_candidate_path_from_preview(root, path),
        label: label.to_string(),
    })
}

fn project_candidate_path_from_preview(root: &str, path: &str) -> String {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        display_path(&path)
    } else {
        display_path(&PathBuf::from(root).join(path))
    }
}

fn format_project_docs_preview(preview: &ProjectDocsPreview) -> String {
    let mut lines = vec![
        format!("project: {} ({})", preview.project_name, preview.project_id),
        format!("root: {}", preview.root),
        format!("files scanned: {}", preview.files_scanned),
        format!("candidates: {}", preview.candidates_found),
        format!("truncated: {}", preview.truncated),
    ];
    for candidate in preview.candidates.iter().take(30) {
        lines.push(format!("candidate: {}", candidate.label));
    }
    if preview.candidates.len() > 30 {
        lines.push(format!(
            "... {} more candidate(s)",
            preview.candidates.len().saturating_sub(30)
        ));
    }
    lines.join("\n")
}

fn can_import_project_docs(
    preview: Option<&ProjectDocsPreview>,
    health: DaemonHealth,
    running: bool,
    confirmed: bool,
) -> bool {
    health == DaemonHealth::Healthy
        && !running
        && confirmed
        && preview
            .map(|preview| !preview.candidates.is_empty())
            .unwrap_or(false)
}

fn project_docs_import_unavailable_message(
    preview: Option<&ProjectDocsPreview>,
    health: DaemonHealth,
    confirmed: bool,
) -> String {
    let Some(preview) = preview else {
        return "preview project docs first".to_string();
    };
    if preview.candidates.is_empty() {
        return "preview found no project docs to import".to_string();
    }
    if health != DaemonHealth::Healthy {
        return "start Solo before importing project docs".to_string();
    }
    if !confirmed {
        return "confirm the import before writing documents into the active profile".to_string();
    }
    "import exactly the previewed project docs into the active profile".to_string()
}

fn project_docs_import_status(
    state: &ProjectDocsImportState,
    health: DaemonHealth,
    preview: Option<&ProjectDocsPreview>,
) -> String {
    match state {
        ProjectDocsImportState::Idle => match preview {
            Some(preview) if preview.candidates.is_empty() => {
                "No project docs found in the last preview.".to_string()
            }
            Some(_) if health == DaemonHealth::Healthy => {
                "Confirm to import the previewed docs into the active profile.".to_string()
            }
            Some(_) => "Start Solo to import the previewed docs.".to_string(),
            None => "Preview docs before importing project docs.".to_string(),
        },
        ProjectDocsImportState::Running { count, started_at } => format!(
            "importing {count} project doc(s) ({}s)",
            started_at.elapsed().as_secs()
        ),
        ProjectDocsImportState::Succeeded {
            report,
            completed_at,
        } => format!(
            "project docs imported; {} new, {} deduped, {} failed, {} chunk(s) ({})",
            report.imported,
            report.deduped,
            report.failed,
            report.chunks_persisted,
            format_age(*completed_at)
        ),
        ProjectDocsImportState::Failed {
            message,
            completed_at,
        } => format!(
            "project docs import failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn project_docs_import_output(state: &ProjectDocsImportState) -> Option<String> {
    let ProjectDocsImportState::Succeeded { report, .. } = state else {
        return None;
    };
    Some(format_native_import_report(report))
}

fn project_decision_context(
    snapshot: &ProjectMemorySnapshot,
) -> Option<(&Path, &ProjectMemoryConfig)> {
    let root = snapshot.root.as_deref()?;
    let config = snapshot.config.as_ref()?;
    (snapshot.state == ProjectMemoryState::Ready).then_some((root, config))
}

fn can_use_project_decisions(
    snapshot: &ProjectMemorySnapshot,
    health: DaemonHealth,
    action_running: bool,
) -> bool {
    health == DaemonHealth::Healthy
        && !action_running
        && project_decision_context(snapshot).is_some()
}

fn project_decision_unavailable_message(snapshot: &ProjectMemorySnapshot) -> String {
    match snapshot.state {
        ProjectMemoryState::Ready => {
            "save or search decisions through the running daemon".to_string()
        }
        ProjectMemoryState::NotSelected => "select a project root first".to_string(),
        ProjectMemoryState::MissingRoot => "project root is missing".to_string(),
        ProjectMemoryState::MissingConfig => {
            "create `.solo/project.toml` before saving project decisions".to_string()
        }
        ProjectMemoryState::InvalidConfig => {
            "repair `.solo/project.toml` before saving project decisions".to_string()
        }
    }
}

fn project_descriptor_json(config: &ProjectMemoryConfig, root: &Path) -> serde_json::Value {
    serde_json::to_value(project_descriptor(config, root))
        .expect("ProjectMemoryDescriptor serializes")
}

fn project_descriptor(config: &ProjectMemoryConfig, root: &Path) -> ProjectMemoryDescriptor {
    ProjectMemoryDescriptor {
        name: config.name.clone(),
        id: config.project_id.clone(),
        root: display_path(root),
        tags: config.tags.clone(),
    }
}

fn project_decision_hit_matches(hit: &MemorySearchHit, project_id: &str) -> bool {
    hit.source_type == "project_decision" && hit.content.contains(&format!("(id: {project_id},"))
}

fn project_decision_status(
    action: &ProjectDecisionActionState,
    snapshot: &ProjectMemorySnapshot,
    health: DaemonHealth,
) -> String {
    match action {
        ProjectDecisionActionState::Idle if health != DaemonHealth::Healthy => {
            "Start Solo to save or search project decisions.".to_string()
        }
        ProjectDecisionActionState::Idle if project_decision_context(snapshot).is_none() => {
            project_decision_unavailable_message(snapshot)
        }
        ProjectDecisionActionState::Idle => {
            "Project decisions are saved into the active profile.".to_string()
        }
        ProjectDecisionActionState::Adding { started_at } => {
            format!(
                "saving project decision ({}s)",
                started_at.elapsed().as_secs()
            )
        }
        ProjectDecisionActionState::Added {
            memory_id,
            completed_at,
        } => format!(
            "saved project decision {memory_id} ({})",
            format_age(*completed_at)
        ),
        ProjectDecisionActionState::Searching { query, started_at } => {
            format!(
                "searching project decisions for `{query}` ({}s)",
                started_at.elapsed().as_secs()
            )
        }
        ProjectDecisionActionState::SearchSucceeded {
            hits, completed_at, ..
        } => format!(
            "project decision search returned {} hit(s) ({})",
            hits.len(),
            format_age(*completed_at)
        ),
        ProjectDecisionActionState::Failed {
            verb,
            message,
            completed_at,
        } => format!(
            "{} failed: {message} ({})",
            verb.label(),
            format_age(*completed_at)
        ),
    }
}

fn project_decision_verb(action: &ProjectDecisionActionState) -> Option<ProjectDecisionVerb> {
    match action {
        ProjectDecisionActionState::Adding { .. } | ProjectDecisionActionState::Added { .. } => {
            Some(ProjectDecisionVerb::Add)
        }
        ProjectDecisionActionState::Searching { .. }
        | ProjectDecisionActionState::SearchSucceeded { .. } => Some(ProjectDecisionVerb::Search),
        ProjectDecisionActionState::Failed { verb, .. } => Some(*verb),
        ProjectDecisionActionState::Idle => None,
    }
}

fn project_decision_results(
    action: &ProjectDecisionActionState,
) -> Option<(&str, &[MemorySearchHit])> {
    match action {
        ProjectDecisionActionState::SearchSucceeded { query, hits, .. } => {
            Some((query.as_str(), hits.as_slice()))
        }
        _ => None,
    }
}

fn project_facts_subject(config: &ProjectMemoryConfig, input: &str) -> String {
    let subject = input.trim();
    if subject.is_empty() {
        config.name.clone()
    } else {
        subject.to_string()
    }
}

fn project_facts_subject_label(snapshot: &ProjectMemorySnapshot, input: &str) -> String {
    let subject = input.trim();
    if subject.is_empty() {
        snapshot
            .config
            .as_ref()
            .map(|config| config.name.clone())
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        subject.to_string()
    }
}

fn can_load_project_facts(
    snapshot: &ProjectMemorySnapshot,
    health: DaemonHealth,
    loading: bool,
) -> bool {
    health == DaemonHealth::Healthy && !loading && project_decision_context(snapshot).is_some()
}

fn project_facts_unavailable_message(snapshot: &ProjectMemorySnapshot) -> String {
    match snapshot.state {
        ProjectMemoryState::Ready => {
            "load facts for the project name or a custom subject".to_string()
        }
        ProjectMemoryState::NotSelected => "select a project root first".to_string(),
        ProjectMemoryState::MissingRoot => "project root is missing".to_string(),
        ProjectMemoryState::MissingConfig => {
            "create `.solo/project.toml` before loading project facts".to_string()
        }
        ProjectMemoryState::InvalidConfig => {
            "repair `.solo/project.toml` before loading project facts".to_string()
        }
    }
}

fn project_facts_status(
    state: &ProjectFactsState,
    snapshot: &ProjectMemorySnapshot,
    health: DaemonHealth,
) -> String {
    match state {
        ProjectFactsState::Idle if health != DaemonHealth::Healthy => {
            "Start Solo to load project facts.".to_string()
        }
        ProjectFactsState::Idle if project_decision_context(snapshot).is_none() => {
            project_facts_unavailable_message(snapshot)
        }
        ProjectFactsState::Idle => "Load facts from the active profile.".to_string(),
        ProjectFactsState::Loading {
            subject,
            started_at,
        } => format!(
            "loading facts for `{subject}` ({}s)",
            started_at.elapsed().as_secs()
        ),
        ProjectFactsState::Loaded {
            subject,
            facts,
            completed_at,
        } => format!(
            "loaded {} fact(s) for `{subject}` ({})",
            facts.len(),
            format_age(*completed_at)
        ),
        ProjectFactsState::Failed {
            subject,
            message,
            completed_at,
        } => format!(
            "facts for `{subject}` failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn project_facts_results(state: &ProjectFactsState) -> Option<(&str, &[ProjectFactHit])> {
    match state {
        ProjectFactsState::Loaded { subject, facts, .. } => Some((subject.as_str(), facts)),
        _ => None,
    }
}

fn project_facts_state_subject(state: &ProjectFactsState) -> String {
    match state {
        ProjectFactsState::Loading { subject, .. }
        | ProjectFactsState::Loaded { subject, .. }
        | ProjectFactsState::Failed { subject, .. } => subject.clone(),
        ProjectFactsState::Idle => "unknown".to_string(),
    }
}

fn project_fact_label(fact: &ProjectFactHit) -> String {
    format!(
        "{} --{}--> {} ({})",
        fact.subject_id, fact.predicate, fact.object_id, fact.object_kind
    )
}

fn memory_context_graph_fact_label(fact: &MemoryContextGraphFact) -> String {
    let mut label = format!(
        "{} --{}--> {} ({}, {:.2})",
        fact.subject_id, fact.predicate, fact.object_id, fact.object_kind, fact.confidence
    );
    if let Some(preview) = fact.evidence_preview.as_deref().filter(|s| !s.is_empty()) {
        label.push_str(": ");
        label.push_str(preview);
    }
    label
}

fn memory_context_graph_warning_label(warning: &MemoryContextGraphReviewWarning) -> String {
    format!(
        "{}: {} --{}--> {}",
        warning.reason_code, warning.subject_id, warning.predicate, warning.object_id
    )
}

fn project_fact_validity_label(fact: &ProjectFactHit) -> String {
    let valid_from = memory_timestamp_label(Some(fact.valid_from_ms));
    match fact.valid_to_ms {
        Some(valid_to) => format!(
            "valid from {valid_from} until {}; cluster {}",
            memory_timestamp_label(Some(valid_to)),
            fact.cluster_id.as_deref().unwrap_or("none")
        ),
        None => format!(
            "valid from {valid_from}; cluster {}",
            fact.cluster_id.as_deref().unwrap_or("none")
        ),
    }
}

fn import_preview_help(source: ImportSource) -> &'static str {
    match source {
        ImportSource::Markdown => "scan Markdown files without ingesting them",
        ImportSource::Text => "scan text files without ingesting them",
        ImportSource::Json => "scan JSON files without ingesting them",
        ImportSource::ChatGpt => "parse ChatGPT exports without ingesting them",
        ImportSource::Claude => "parse Claude exports without ingesting them",
        ImportSource::Bookmarks => "parse bookmark exports without crawling pages",
    }
}

fn import_commit_help(source: ImportSource) -> &'static str {
    match source {
        ImportSource::Markdown | ImportSource::Text | ImportSource::Json => {
            "ingest supported documents into the active Solo profile"
        }
        ImportSource::ChatGpt | ImportSource::Claude | ImportSource::Bookmarks => {
            "materialize previewed records and ingest them into the active Solo profile"
        }
    }
}

fn import_action_status(action: &ImportActionState) -> String {
    match action {
        ImportActionState::Idle => {
            "Preview import scans the selected source without opening the encrypted database."
                .to_string()
        }
        ImportActionState::Running {
            source,
            path,
            started_at,
        } => format!(
            "{} preview running for {} ({}s)",
            source.label(),
            display_path(path),
            started_at.elapsed().as_secs()
        ),
        ImportActionState::Succeeded {
            source,
            path,
            message,
            completed_at,
            ..
        } => format!(
            "{} preview succeeded for {}: {message} ({})",
            source.label(),
            display_path(path),
            format_age(*completed_at)
        ),
        ImportActionState::Failed {
            source,
            message,
            completed_at,
        } => format!(
            "{} preview failed: {message} ({})",
            source.label(),
            format_age(*completed_at)
        ),
    }
}

fn import_action_output(action: &ImportActionState) -> Option<&str> {
    match action {
        ImportActionState::Succeeded { output, .. } if !output.is_empty() => Some(output),
        _ => None,
    }
}

fn import_action_source(action: &ImportActionState) -> Option<ImportSource> {
    match action {
        ImportActionState::Running { source, .. }
        | ImportActionState::Succeeded { source, .. }
        | ImportActionState::Failed { source, .. } => Some(*source),
        ImportActionState::Idle => None,
    }
}

fn import_preview_matches(action: &ImportActionState, source: ImportSource, path: &Path) -> bool {
    matches!(
        action,
        ImportActionState::Succeeded {
            source: preview_source,
            path: preview_path,
            ..
        } if *preview_source == source && preview_path == path
    )
}

fn import_commit_status(action: &ImportCommitState, health: DaemonHealth) -> String {
    match action {
        ImportCommitState::Idle if health != DaemonHealth::Healthy => {
            "Start Solo to import the previewed source into the active profile.".to_string()
        }
        ImportCommitState::Idle => {
            "Import writes the previewed source into the active profile after confirmation."
                .to_string()
        }
        ImportCommitState::Running { path, started_at } => format!(
            "importing {} ({}s)",
            display_path(path),
            started_at.elapsed().as_secs()
        ),
        ImportCommitState::Succeeded {
            report,
            completed_at,
        } => format!(
            "import finished for {}; {} new, {} deduped, {} failed, {} chunk(s) ({})",
            report.path,
            report.imported,
            report.deduped,
            report.failed,
            report.chunks_persisted,
            format_age(*completed_at)
        ),
        ImportCommitState::Failed {
            message,
            completed_at,
        } => format!("import failed: {message} ({})", format_age(*completed_at)),
    }
}

fn import_commit_output(action: &ImportCommitState) -> Option<String> {
    let ImportCommitState::Succeeded { report, .. } = action else {
        return None;
    };
    Some(format_native_import_report(report))
}

fn format_native_import_report(report: &NativeImportReport) -> String {
    let mut lines = vec![
        format!("path: {}", report.path),
        format!("dry_run: {}", report.dry_run),
        format!("recursive: {}", report.recursive),
        format!("truncated: {}", report.truncated),
        format!("total_files: {}", report.total_files),
        format!("total_bytes: {}", report.total_bytes),
        format!("store_original_file: {}", report.store_original_file),
        format!("imported: {}", report.imported),
        format!("deduped: {}", report.deduped),
        format!("failed: {}", report.failed),
        format!("chunks_persisted: {}", report.chunks_persisted),
        format!("assets_retained: {}", report.assets_retained),
        format!("assets_deduped: {}", report.assets_deduped),
        format!("asset_links: {}", report.asset_links),
        format!("asset_failed: {}", report.asset_failed),
    ];
    for result in report.results.iter().take(20) {
        let state = if let Some(error) = result.error.as_deref() {
            format!("failed: {error}")
        } else if let Some(error) = result.asset_error.as_deref() {
            format!("asset failed: {error}")
        } else if result.deduped {
            "deduped".to_string()
        } else {
            "imported".to_string()
        };
        let doc = result
            .doc_id
            .as_deref()
            .map(|doc_id| format!(" doc_id={doc_id}"))
            .unwrap_or_default();
        let asset = result
            .asset_id
            .as_deref()
            .map(|asset_id| format!(" asset_id={asset_id}"))
            .unwrap_or_default();
        lines.push(format!(
            "- {} bytes={} ingested={} chunks={} {}{}{}",
            result.path,
            result.bytes,
            result.bytes_ingested,
            result.chunks_persisted,
            state,
            doc,
            asset
        ));
    }
    if report.results.len() > 20 {
        lines.push(format!(
            "... {} more result(s)",
            report.results.len().saturating_sub(20)
        ));
    }
    lines.join("\n")
}

fn document_list_status(state: &DocumentListState) -> String {
    match state {
        DocumentListState::Idle => "Refresh to list documents in the active profile.".to_string(),
        DocumentListState::Loading { started_at } => {
            format!("loading documents ({}s)", started_at.elapsed().as_secs())
        }
        DocumentListState::Loaded {
            documents,
            completed_at,
        } => format!(
            "loaded {} document(s) ({})",
            documents.len(),
            format_age(*completed_at)
        ),
        DocumentListState::Failed {
            message,
            completed_at,
        } => format!(
            "documents failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn document_list_items(state: &DocumentListState) -> Option<&[DocumentSummary]> {
    match state {
        DocumentListState::Loaded { documents, .. } => Some(documents.as_slice()),
        _ => None,
    }
}

fn document_search_status(state: &DocumentSearchState, health: DaemonHealth) -> String {
    match state {
        DocumentSearchState::Idle if health != DaemonHealth::Healthy => {
            "Start Solo to search imported documents.".to_string()
        }
        DocumentSearchState::Idle => {
            "Search imported document chunks in the active profile.".to_string()
        }
        DocumentSearchState::Searching { query, started_at } => {
            format!("searching `{query}` ({}s)", started_at.elapsed().as_secs())
        }
        DocumentSearchState::Succeeded {
            hits, completed_at, ..
        } => format!(
            "search returned {} chunk hit(s) ({})",
            hits.len(),
            format_age(*completed_at)
        ),
        DocumentSearchState::Failed {
            query,
            message,
            completed_at,
        } => format!(
            "search `{query}` failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn document_search_results(state: &DocumentSearchState) -> Option<(&str, &[DocumentSearchHit])> {
    match state {
        DocumentSearchState::Succeeded { query, hits, .. } => Some((query.as_str(), hits)),
        _ => None,
    }
}

fn document_search_query(state: &DocumentSearchState) -> String {
    match state {
        DocumentSearchState::Searching { query, .. }
        | DocumentSearchState::Succeeded { query, .. }
        | DocumentSearchState::Failed { query, .. } => query.clone(),
        DocumentSearchState::Idle => "unknown".to_string(),
    }
}

fn document_search_hit_title(hit: &DocumentSearchHit) -> String {
    hit.doc_title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&hit.doc_id)
        .to_string()
}

fn document_search_hit_source(hit: &DocumentSearchHit) -> String {
    let source = hit
        .doc_source
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("source unavailable");
    let mime = hit
        .doc_mime_type
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("mime unavailable");
    format!("{source} / {mime}")
}

fn document_title_label(document: &DocumentSummary) -> String {
    document
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&document.doc_id)
        .to_string()
}

fn document_source_label(document: &DocumentSummary) -> String {
    let source = document
        .source
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("source unavailable");
    let mime = document
        .mime_type
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("mime unavailable");
    format!("{source} / {mime}")
}

fn document_detail_status(state: &DocumentDetailState) -> String {
    match state {
        DocumentDetailState::Idle => "Inspect a document to view chunks and metadata.".to_string(),
        DocumentDetailState::Loading { doc_id, started_at } => {
            format!("inspecting {doc_id} ({}s)", started_at.elapsed().as_secs())
        }
        DocumentDetailState::Loaded {
            detail,
            completed_at,
        } => format!(
            "loaded {}; {} chunk(s) ({})",
            detail.doc_id,
            detail.chunks.len(),
            format_age(*completed_at)
        ),
        DocumentDetailState::Failed {
            doc_id,
            message,
            completed_at,
        } => format!(
            "inspect {doc_id} failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn document_detail_loaded(state: &DocumentDetailState) -> Option<&DocumentDetail> {
    match state {
        DocumentDetailState::Loaded { detail, .. } => Some(detail),
        _ => None,
    }
}

fn document_detail_id(state: &DocumentDetailState) -> String {
    match state {
        DocumentDetailState::Loading { doc_id, .. }
        | DocumentDetailState::Failed { doc_id, .. } => doc_id.clone(),
        DocumentDetailState::Loaded { detail, .. } => detail.doc_id.clone(),
        DocumentDetailState::Idle => "unknown".to_string(),
    }
}

fn document_detail_title_label(detail: &DocumentDetail) -> String {
    detail
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("title unavailable")
        .to_string()
}

fn document_detail_source_label(detail: &DocumentDetail) -> String {
    let source = detail
        .source
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("source unavailable");
    let mime = detail
        .mime_type
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("mime unavailable");
    format!("{source} / {mime}")
}

fn document_detail_size_label(detail: &DocumentDetail) -> String {
    let bytes = detail
        .byte_size
        .map(|bytes| format!("{bytes} bytes"))
        .unwrap_or_else(|| "size unavailable".to_string());
    format!("{bytes}; {} chunk(s)", detail.chunk_count)
}

fn document_forget_status(state: &DocumentForgetState) -> String {
    match state {
        DocumentForgetState::Idle => {
            "Forget hides the selected active document from search and lists.".to_string()
        }
        DocumentForgetState::Forgetting { doc_id, started_at } => {
            format!("forgetting {doc_id} ({}s)", started_at.elapsed().as_secs())
        }
        DocumentForgetState::Forgotten {
            report,
            completed_at,
        } => format!(
            "forgot {}; {} chunk(s) tombstoned ({})",
            report.doc_id,
            report.chunks_tombstoned,
            format_age(*completed_at)
        ),
        DocumentForgetState::Failed {
            doc_id,
            message,
            completed_at,
        } => format!(
            "forget {doc_id} failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn document_forget_id(state: &DocumentForgetState) -> String {
    match state {
        DocumentForgetState::Forgetting { doc_id, .. }
        | DocumentForgetState::Failed { doc_id, .. } => doc_id.clone(),
        DocumentForgetState::Forgotten { report, .. } => report.doc_id.clone(),
        DocumentForgetState::Idle => "unknown".to_string(),
    }
}

fn memory_action_status(action: &MemoryActionState) -> String {
    match action {
        MemoryActionState::Idle => {
            "Save a memory or search the active profile through the running daemon.".to_string()
        }
        MemoryActionState::Remembering { started_at } => {
            format!("saving memory ({}s)", started_at.elapsed().as_secs())
        }
        MemoryActionState::Remembered {
            memory_id,
            completed_at,
        } => format!("remembered {memory_id} ({})", format_age(*completed_at)),
        MemoryActionState::Searching { query, started_at } => {
            format!("searching `{query}` ({}s)", started_at.elapsed().as_secs())
        }
        MemoryActionState::SearchSucceeded {
            hits, completed_at, ..
        } => format!(
            "search returned {} hit(s) ({})",
            hits.len(),
            format_age(*completed_at)
        ),
        MemoryActionState::Failed {
            verb,
            message,
            completed_at,
        } => format!(
            "{} failed: {message} ({})",
            verb.label(),
            format_age(*completed_at)
        ),
    }
}

fn memory_context_status(state: &MemoryContextState, health: DaemonHealth) -> String {
    match state {
        MemoryContextState::Idle if health != DaemonHealth::Healthy => {
            "Start Solo to preview agent memory context.".to_string()
        }
        MemoryContextState::Idle => {
            "Build the same context bundle agents can request from Solo.".to_string()
        }
        MemoryContextState::Loading { query, started_at } => {
            format!(
                "building context for `{query}` ({}s)",
                started_at.elapsed().as_secs()
            )
        }
        MemoryContextState::Loaded {
            summary,
            completed_at,
        } => format!(
            "context ready for `{}`: {} recall, {} fact(s), {} theme(s) ({})",
            summary.query,
            summary.recall_hits.len(),
            summary.facts.len(),
            summary.themes.len(),
            format_age(*completed_at)
        ),
        MemoryContextState::Failed {
            query,
            message,
            completed_at,
        } => format!(
            "context `{query}` failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn memory_context_query(state: &MemoryContextState) -> String {
    match state {
        MemoryContextState::Loading { query, .. } | MemoryContextState::Failed { query, .. } => {
            query.clone()
        }
        MemoryContextState::Loaded { summary, .. } => summary.query.clone(),
        MemoryContextState::Idle => "unknown".to_string(),
    }
}

fn memory_context_summary(state: &MemoryContextState) -> Option<&MemoryContextSummary> {
    match state {
        MemoryContextState::Loaded { summary, .. } => Some(summary),
        _ => None,
    }
}

fn memory_context_subject_label(summary: &MemoryContextSummary) -> String {
    match (&summary.subject, &summary.resolved_subject) {
        (Some(subject), Some(resolved)) if subject != resolved => {
            format!("{subject} -> {resolved}")
        }
        (Some(subject), _) => subject.clone(),
        (None, Some(resolved)) => resolved.clone(),
        (None, None) => "none".to_string(),
    }
}

fn memory_action_verb(action: &MemoryActionState) -> Option<MemoryActionVerb> {
    match action {
        MemoryActionState::Remembering { .. } | MemoryActionState::Remembered { .. } => {
            Some(MemoryActionVerb::Remember)
        }
        MemoryActionState::Searching { .. } | MemoryActionState::SearchSucceeded { .. } => {
            Some(MemoryActionVerb::Search)
        }
        MemoryActionState::Failed { verb, .. } => Some(*verb),
        MemoryActionState::Idle => None,
    }
}

fn memory_search_results(
    action: &MemoryActionState,
) -> Option<(&str, &[MemorySearchHit], usize, usize)> {
    match action {
        MemoryActionState::SearchSucceeded {
            query,
            hits,
            index_len,
            candidates_considered,
            ..
        } => Some((
            query.as_str(),
            hits.as_slice(),
            *index_len,
            *candidates_considered,
        )),
        _ => None,
    }
}

fn memory_recent_status(state: &MemoryRecentState) -> String {
    match state {
        MemoryRecentState::Idle => {
            "Refresh to load the newest memories in this profile.".to_string()
        }
        MemoryRecentState::Loading { started_at } => {
            format!(
                "loading recent memories ({}s)",
                started_at.elapsed().as_secs()
            )
        }
        MemoryRecentState::Loaded {
            memories,
            completed_at,
        } => format!(
            "{} recent memory item(s) loaded ({})",
            memories.len(),
            format_age(*completed_at)
        ),
        MemoryRecentState::Failed {
            message,
            completed_at,
        } => format!(
            "recent memories failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn memory_recent_items(state: &MemoryRecentState) -> Option<&[RecentMemory]> {
    match state {
        MemoryRecentState::Loaded { memories, .. } => Some(memories.as_slice()),
        _ => None,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MemoryReviewCounts {
    total: usize,
    needs_review: usize,
    approved: usize,
    dismissed: usize,
}

impl MemoryReviewCounts {
    fn reviewed(self) -> usize {
        self.approved + self.dismissed
    }
}

fn set_memory_review_state_cached(
    settings: &mut Settings,
    memory_id: &str,
    state: Option<&str>,
    reviewed_at_ms: i64,
) -> bool {
    let memory_id = memory_id.trim();
    if memory_id.is_empty() {
        return false;
    }

    let key = memory_id.to_string();
    match state {
        Some(state @ ("approved" | "dismissed")) => {
            settings.memory_reviews.insert(
                key,
                MemoryReviewStatus {
                    state: state.to_string(),
                    reviewed_at_ms: Some(reviewed_at_ms),
                    note: None,
                },
            );
            true
        }
        Some(_) => false,
        None => settings.memory_reviews.remove(&key).is_some(),
    }
}

fn memory_review_status<'a>(
    settings: &'a Settings,
    memory_id: &str,
) -> Option<&'a MemoryReviewStatus> {
    settings.memory_reviews.get(memory_id)
}

fn memory_effective_review_status(
    settings: &Settings,
    memory: &RecentMemory,
) -> Option<MemoryReviewStatus> {
    match memory.review_state.as_deref() {
        Some("approved" | "dismissed") => Some(MemoryReviewStatus {
            state: memory.review_state.clone().unwrap_or_default(),
            reviewed_at_ms: memory.reviewed_at_ms,
            note: memory.review_note.clone(),
        }),
        _ => memory_review_status(settings, &memory.memory_id).cloned(),
    }
}

fn memory_review_label(review: Option<&MemoryReviewStatus>) -> &'static str {
    match review.map(|status| status.state.as_str()) {
        Some("approved") => "Approved",
        Some("dismissed") => "Dismissed",
        _ => "Needs review",
    }
}

fn memory_review_tone(review: Option<&MemoryReviewStatus>) -> StateTone {
    match review.map(|status| status.state.as_str()) {
        Some("approved") => StateTone::Good,
        Some("dismissed") => StateTone::Warn,
        _ => StateTone::Warn,
    }
}

fn memory_review_detail(review: Option<&MemoryReviewStatus>) -> String {
    let Some(review) = review else {
        return "No inbox decision yet. Approve or dismiss without changing the memory content."
            .to_string();
    };
    let when = review
        .reviewed_at_ms
        .map(|ts_ms| memory_age_label(Some(ts_ms)))
        .unwrap_or_else(|| "time unknown".to_string());
    let mut detail = format!(
        "Inbox state: {}; reviewed {when}. This does not change the memory content.",
        memory_review_label(Some(review))
    );
    if let Some(note) = review.note.as_deref().and_then(memory_review_note_summary) {
        detail.push_str(" Note: ");
        detail.push_str(&note);
    }
    detail
}

fn memory_review_note_summary(note: &str) -> Option<String> {
    const LIMIT: usize = 140;
    let trimmed = note.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= LIMIT {
        return Some(trimmed);
    }
    let mut summary: String = trimmed.chars().take(LIMIT.saturating_sub(3)).collect();
    summary.push_str("...");
    Some(summary)
}

fn memory_review_matches_filter(
    review: Option<&MemoryReviewStatus>,
    filter: MemoryReviewFilter,
) -> bool {
    match filter {
        MemoryReviewFilter::NeedsReview => !matches!(
            review.map(|status| status.state.as_str()),
            Some("approved" | "dismissed")
        ),
        MemoryReviewFilter::Approved => {
            matches!(review.map(|status| status.state.as_str()), Some("approved"))
        }
        MemoryReviewFilter::Dismissed => {
            matches!(
                review.map(|status| status.state.as_str()),
                Some("dismissed")
            )
        }
        MemoryReviewFilter::All => true,
    }
}

fn memory_review_counts(memories: &[RecentMemory], settings: &Settings) -> MemoryReviewCounts {
    let mut counts = MemoryReviewCounts::default();
    for memory in memories {
        counts.total += 1;
        match memory_effective_review_status(settings, memory)
            .as_ref()
            .map(|status| status.state.as_str())
        {
            Some("approved") => counts.approved += 1,
            Some("dismissed") => counts.dismissed += 1,
            _ => counts.needs_review += 1,
        }
    }
    counts
}

fn memory_review_counts_label(counts: &MemoryReviewCounts) -> String {
    format!(
        "{} loaded; {} need review; {} approved; {} dismissed",
        counts.total, counts.needs_review, counts.approved, counts.dismissed
    )
}

fn memory_review_status_label(counts: &MemoryReviewCounts) -> String {
    if counts.total == 0 {
        "No memories loaded".to_string()
    } else if counts.needs_review == 0 {
        "Review clear".to_string()
    } else {
        format!("{} to review", counts.needs_review)
    }
}

fn memory_review_status_tone(counts: &MemoryReviewCounts) -> StateTone {
    if counts.total > 0 && counts.needs_review == 0 {
        StateTone::Good
    } else {
        StateTone::Warn
    }
}

fn memory_review_visible_label(
    visible_count: usize,
    total_count: usize,
    review_filter: MemoryReviewFilter,
    source_filter: MemorySourceFilter,
) -> String {
    format!(
        "showing {visible_count}/{total_count}; filters: {}, {}",
        review_filter.label(),
        source_filter.label()
    )
}

fn memory_review_scope_detail(counts: &MemoryReviewCounts) -> String {
    format!(
        "Inbox review state is stored in the Solo daemon with a local compatibility cache. {} reviewed; memory content is unchanged.",
        counts.reviewed()
    )
}

fn memory_review_clipboard_summary(
    counts: &MemoryReviewCounts,
    visible_count: usize,
    review_filter: MemoryReviewFilter,
    source_filter: MemorySourceFilter,
) -> String {
    format!(
        "Solo Memory Inbox\nloaded: {}\nvisible: {visible_count}\nneeds_review: {}\napproved: {}\ndismissed: {}\nreview_filter: {}\nsource_filter: {}",
        counts.total,
        counts.needs_review,
        counts.approved,
        counts.dismissed,
        review_filter.label(),
        source_filter.label()
    )
}

const HIGH_SALIENCE_THRESHOLD: f64 = 0.75;

fn memory_source_matches_filter(memory: &RecentMemory, filter: MemorySourceFilter) -> bool {
    match filter {
        MemorySourceFilter::All => true,
        MemorySourceFilter::HighSalience => memory
            .salience
            .is_some_and(|salience| salience >= HIGH_SALIENCE_THRESHOLD),
        MemorySourceFilter::UserCreated => memory_source_is_user_created(memory),
        MemorySourceFilter::AgentCreated => memory_source_is_agent_created(memory),
        MemorySourceFilter::ToolOutput => memory_source_contains(memory, "tool"),
        MemorySourceFilter::DocumentDerived => {
            memory_source_contains(memory, "document")
                || memory_source_contains(memory, "doc")
                || memory_source_contains(memory, "import")
        }
        MemorySourceFilter::SoloDesktop => memory_source_contains(memory, "solo_desktop"),
    }
}

fn memory_source_is_user_created(memory: &RecentMemory) -> bool {
    memory_source_contains(memory, "user")
        || memory_source_contains(memory, "manual")
        || memory_source_contains(memory, "solo_desktop")
}

fn memory_source_is_agent_created(memory: &RecentMemory) -> bool {
    memory_source_contains(memory, "agent")
        || memory_source_contains(memory, "mcp")
        || memory_source_contains(memory, "codex")
        || memory_source_contains(memory, "claude")
        || memory_source_contains(memory, "cursor")
}

fn memory_source_contains(memory: &RecentMemory, needle: &str) -> bool {
    memory
        .source_type
        .as_deref()
        .is_some_and(|source| source.to_ascii_lowercase().contains(needle))
}

fn memory_source_summary(memory: &RecentMemory) -> String {
    let source = memory.source_type.as_deref().unwrap_or("source unknown");
    let salience = memory
        .salience
        .map(|salience| format!("salience {salience:.2}"))
        .unwrap_or_else(|| "salience unknown".to_string());
    let status = memory.status.as_deref().unwrap_or("status unknown");
    format!("{source}; {salience}; {status}")
}

fn memory_age_label(ts_ms: Option<i64>) -> String {
    let Some(ts_ms) = ts_ms else {
        return "unknown time".to_string();
    };
    let Ok(ms) = u64::try_from(ts_ms) else {
        return "unknown time".to_string();
    };
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
    format_age(at)
}

fn memory_timestamp_label(ts_ms: Option<i64>) -> String {
    memory_age_label(ts_ms)
}

fn memory_detail_status(state: &MemoryDetailState) -> String {
    match state {
        MemoryDetailState::Idle => {
            "Inspect a recent or recalled memory to view full details.".to_string()
        }
        MemoryDetailState::Loading {
            memory_id,
            started_at,
        } => format!(
            "inspecting {memory_id} ({}s)",
            started_at.elapsed().as_secs()
        ),
        MemoryDetailState::Loaded {
            detail,
            completed_at,
        } => format!(
            "loaded {} ({})",
            detail.memory_id,
            format_age(*completed_at)
        ),
        MemoryDetailState::Failed {
            memory_id,
            message,
            completed_at,
        } => format!(
            "inspect {memory_id} failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn memory_detail_loaded(state: &MemoryDetailState) -> Option<&MemoryDetail> {
    match state {
        MemoryDetailState::Loaded { detail, .. } => Some(detail),
        _ => None,
    }
}

fn memory_detail_id(state: &MemoryDetailState) -> String {
    match state {
        MemoryDetailState::Loading { memory_id, .. }
        | MemoryDetailState::Failed { memory_id, .. } => memory_id.clone(),
        MemoryDetailState::Loaded { detail, .. } => detail.memory_id.clone(),
        MemoryDetailState::Idle => "unknown".to_string(),
    }
}

fn memory_detail_source_label(detail: &MemoryDetail) -> String {
    match detail.source_id.as_deref() {
        Some(source_id) if !source_id.is_empty() => {
            format!("{} / {}", detail.source_type, source_id)
        }
        _ => detail.source_type.clone(),
    }
}

fn memory_update_status(state: &MemoryUpdateState) -> String {
    match state {
        MemoryUpdateState::Idle => {
            "Update rewrites the selected active memory and refreshes its embedding.".to_string()
        }
        MemoryUpdateState::Updating {
            memory_id,
            started_at,
        } => format!("updating {memory_id} ({}s)", started_at.elapsed().as_secs()),
        MemoryUpdateState::Updated {
            memory_id,
            updated_at_ms,
            completed_at,
        } => format!(
            "updated {memory_id}; updated {} ({})",
            memory_timestamp_label(*updated_at_ms),
            format_age(*completed_at)
        ),
        MemoryUpdateState::Failed {
            memory_id,
            message,
            completed_at,
        } => format!(
            "update {memory_id} failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn memory_update_id(state: &MemoryUpdateState) -> String {
    match state {
        MemoryUpdateState::Updating { memory_id, .. }
        | MemoryUpdateState::Updated { memory_id, .. }
        | MemoryUpdateState::Failed { memory_id, .. } => memory_id.clone(),
        MemoryUpdateState::Idle => "unknown".to_string(),
    }
}

fn memory_forget_status(state: &MemoryForgetState) -> String {
    match state {
        MemoryForgetState::Idle => {
            "Forget hides the selected active memory from future recall.".to_string()
        }
        MemoryForgetState::Forgetting {
            memory_id,
            started_at,
        } => format!(
            "forgetting {memory_id} ({}s)",
            started_at.elapsed().as_secs()
        ),
        MemoryForgetState::Forgotten {
            memory_id,
            completed_at,
        } => format!("forgot {memory_id} ({})", format_age(*completed_at)),
        MemoryForgetState::Failed {
            memory_id,
            message,
            completed_at,
        } => format!(
            "forget {memory_id} failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn memory_forget_id(state: &MemoryForgetState) -> String {
    match state {
        MemoryForgetState::Forgetting { memory_id, .. }
        | MemoryForgetState::Forgotten { memory_id, .. }
        | MemoryForgetState::Failed { memory_id, .. } => memory_id.clone(),
        MemoryForgetState::Idle => "unknown".to_string(),
    }
}

fn memory_contradiction_status(state: &MemoryContradictionState) -> String {
    match state {
        MemoryContradictionState::Idle => {
            "Refresh to show Steward-flagged memory conflicts.".to_string()
        }
        MemoryContradictionState::Loading { started_at } => {
            format!(
                "loading contradictions ({}s)",
                started_at.elapsed().as_secs()
            )
        }
        MemoryContradictionState::Loaded {
            contradictions,
            completed_at,
        } => {
            let open = contradictions
                .iter()
                .filter(|item| item.status != "resolved")
                .count();
            format!(
                "loaded {} contradiction(s); {} open ({})",
                contradictions.len(),
                open,
                format_age(*completed_at)
            )
        }
        MemoryContradictionState::Failed {
            message,
            completed_at,
        } => format!(
            "contradictions failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn memory_contradiction_items(state: &MemoryContradictionState) -> Option<&[MemoryContradiction]> {
    match state {
        MemoryContradictionState::Loaded { contradictions, .. } => Some(contradictions.as_slice()),
        _ => None,
    }
}

fn contradiction_side_label(
    label: &str,
    triple: Option<&MemoryContradictionTriple>,
    fallback_id: &str,
) -> String {
    let Some(triple) = triple else {
        return format!("{label}: {fallback_id}");
    };
    let validity = match triple.valid_to_ms {
        Some(valid_to_ms) => format!(
            "; valid {} to {}",
            memory_timestamp_label(triple.valid_from_ms),
            memory_timestamp_label(Some(valid_to_ms))
        ),
        None => format!(
            "; valid from {}",
            memory_timestamp_label(triple.valid_from_ms)
        ),
    };
    format!(
        "{label}: {}: {} --{}--> {} ({}){}",
        triple.triple_id,
        triple.subject_id,
        triple.predicate,
        triple.object_id,
        triple.object_kind,
        validity
    )
}

fn memory_contradiction_resolve_status(state: &MemoryContradictionResolveState) -> String {
    match state {
        MemoryContradictionResolveState::Idle => {
            "Use A current, B current, or Reopen after reviewing the conflict.".to_string()
        }
        MemoryContradictionResolveState::Resolving { label, started_at } => {
            format!("updating {label} ({}s)", started_at.elapsed().as_secs())
        }
        MemoryContradictionResolveState::Resolved {
            resolution,
            completed_at,
        } => {
            let winner = resolution
                .winning_triple_id
                .as_deref()
                .map(|winner| format!("; winner {winner}"))
                .unwrap_or_default();
            let resolved_at = resolution
                .resolved_at_ms
                .map(|ms| format!("; resolved {}", memory_timestamp_label(Some(ms))))
                .unwrap_or_default();
            let note = resolution
                .resolution_note
                .as_deref()
                .map(|note| format!("; {note}"))
                .unwrap_or_default();
            format!(
                "{} {} / {} ({}){}{}{}",
                resolution.status,
                resolution.a_id,
                resolution.b_id,
                format_age(*completed_at),
                winner,
                resolved_at,
                note
            )
        }
        MemoryContradictionResolveState::Failed {
            label,
            message,
            completed_at,
        } => format!(
            "resolve {label} failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn memory_contradiction_resolve_label(state: &MemoryContradictionResolveState) -> String {
    match state {
        MemoryContradictionResolveState::Resolving { label, .. }
        | MemoryContradictionResolveState::Failed { label, .. } => label.clone(),
        MemoryContradictionResolveState::Resolved { resolution, .. } => {
            format!(
                "{} / {} ({})",
                resolution.a_id, resolution.b_id, resolution.kind
            )
        }
        MemoryContradictionResolveState::Idle => "unknown contradiction".to_string(),
    }
}

fn contradiction_resolution_note(status: &str, winning_triple_id: Option<&str>) -> Option<String> {
    match (status, winning_triple_id) {
        ("resolved", Some(winner)) => Some(format!("Resolved in Solo: {winner} is current.")),
        ("reopened", _) => Some("Reopened in Solo.".to_string()),
        _ => None,
    }
}

fn setup_wizard_daemon_ready(state: Option<&SupervisorState>, health: DaemonHealth) -> bool {
    matches!(state, Some(SupervisorState::Running)) && health == DaemonHealth::Healthy
}

fn setup_wizard_library_ready(snapshot: &LibrarySnapshot) -> bool {
    snapshot.last_error.is_none() && snapshot.exists
}

fn setup_wizard_mcp_ready(
    health: DaemonHealth,
    probe: &McpProbeState,
    active_profile: &str,
) -> bool {
    health == DaemonHealth::Healthy && mcp_probe_ready_for(probe, active_profile)
}

fn setup_wizard_verified_tool_count(snapshot: &ToolSnapshot) -> usize {
    snapshot
        .rows
        .iter()
        .filter(|row| row.state == ToolConfigState::Verified)
        .count()
}

fn setup_wizard_import_ready(
    import_commit: &ImportCommitState,
    document_list: &DocumentListState,
    project_docs_import: &ProjectDocsImportState,
) -> bool {
    native_import_report_has_documents(import_commit_report(import_commit))
        || document_list_items(document_list).is_some_and(|documents| !documents.is_empty())
        || native_import_report_has_documents(project_docs_import_report(project_docs_import))
}

fn setup_wizard_import_detail(
    import_commit: &ImportCommitState,
    document_list: &DocumentListState,
    project_docs_import: &ProjectDocsImportState,
) -> String {
    if let Some(report) = import_commit_report(import_commit)
        && native_import_report_has_documents(Some(report))
    {
        return format!(
            "Imported {} document(s), deduped {}, failed {}.",
            report.imported, report.deduped, report.failed
        );
    }
    if let Some(report) = project_docs_import_report(project_docs_import)
        && native_import_report_has_documents(Some(report))
    {
        return format!(
            "Imported {} project document(s), deduped {}, failed {}.",
            report.imported, report.deduped, report.failed
        );
    }
    if let Some(documents) = document_list_items(document_list) {
        if !documents.is_empty() {
            return format!("{} imported document(s) are visible.", documents.len());
        }
    }
    "Import a file/folder from Data, or import project docs from Projects.".to_string()
}

fn setup_wizard_review_ready(settings: &Settings, memory_recent: &MemoryRecentState) -> bool {
    if setup_wizard_visible_review_count(settings, memory_recent) > 0 {
        return true;
    }
    !settings.memory_reviews.is_empty()
}

fn setup_wizard_review_detail(settings: &Settings, memory_recent: &MemoryRecentState) -> String {
    let visible_count = setup_wizard_visible_review_count(settings, memory_recent);
    if visible_count > 0 {
        return format!("{visible_count} reviewed memory decision(s) visible.");
    }
    let count = settings.memory_reviews.len();
    if count > 0 {
        format!("{count} cached inbox review decision(s) recorded.")
    } else {
        "Review at least one recent memory from the Memory inbox.".to_string()
    }
}

fn setup_wizard_visible_review_count(
    settings: &Settings,
    memory_recent: &MemoryRecentState,
) -> usize {
    let Some(memories) = memory_recent_items(memory_recent) else {
        return 0;
    };
    memories
        .iter()
        .filter(|memory| memory_effective_review_status(settings, memory).is_some())
        .count()
}

fn import_commit_report(state: &ImportCommitState) -> Option<&NativeImportReport> {
    match state {
        ImportCommitState::Succeeded { report, .. } => Some(report),
        _ => None,
    }
}

fn project_docs_import_report(state: &ProjectDocsImportState) -> Option<&NativeImportReport> {
    match state {
        ProjectDocsImportState::Succeeded { report, .. } => Some(report),
        _ => None,
    }
}

fn native_import_report_has_documents(report: Option<&NativeImportReport>) -> bool {
    report.is_some_and(|report| {
        report.imported > 0
            || report.chunks_persisted > 0
            || report
                .results
                .iter()
                .any(|result| result.doc_id.is_some() && result.error.is_none())
    })
}

fn setup_wizard_is_complete(
    state: Option<&SupervisorState>,
    health: DaemonHealth,
    library_snapshot: &LibrarySnapshot,
    active_profile: &str,
    tool_snapshot: &ToolSnapshot,
    mcp_probe: &McpProbeState,
    import_ready: bool,
    review_ready: bool,
) -> bool {
    setup_wizard_daemon_ready(state, health)
        && setup_wizard_library_ready(library_snapshot)
        && setup_wizard_mcp_ready(health, mcp_probe, active_profile)
        && setup_wizard_verified_tool_count(tool_snapshot) > 0
        && import_ready
        && review_ready
}

fn setup_wizard_step_state(done: bool, prior_done: bool) -> SetupWizardStepState {
    if done {
        SetupWizardStepState::Complete
    } else if prior_done {
        SetupWizardStepState::Active
    } else {
        SetupWizardStepState::Waiting
    }
}

fn mcp_status(
    health: DaemonHealth,
    payload: Option<&serde_json::Value>,
    status_url: &str,
) -> (String, StateTone, String) {
    let mcp_url = mcp_url_from_status_url(status_url);
    if health != DaemonHealth::Healthy {
        return ("MCP not ready".to_string(), StateTone::Warn, mcp_url);
    }
    let sessions = payload
        .and_then(|json| json.pointer("/mcp/sessions"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    (
        "MCP ready".to_string(),
        StateTone::Good,
        format!("{mcp_url} ({sessions} active session(s))"),
    )
}

fn mcp_runtime_rows(
    payload: Option<&serde_json::Value>,
    status_url: &str,
    secret_snapshot: &SecretSnapshot,
) -> Vec<(&'static str, String)> {
    vec![
        ("MCP URL", mcp_url_from_status_url(status_url)),
        (
            "Active sessions",
            status_payload_string(payload, "/mcp/sessions"),
        ),
        ("Memory library", "Community Memory Library".to_string()),
        (
            "Library ready",
            status_payload_string(payload, "/library/ready"),
        ),
        ("Bearer auth", bearer_secret_status(secret_snapshot)),
    ]
}

fn bearer_secret_status(snapshot: &SecretSnapshot) -> String {
    match (snapshot.bearer_token_stored, snapshot.last_error.as_deref()) {
        (Some(true), _) => "stored in OS keychain".to_string(),
        (Some(false), _) => "not stored".to_string(),
        (None, Some(error)) => format!("unknown: {error}"),
        (None, None) => "unknown".to_string(),
    }
}

fn mcp_doctor_endpoint_status(state: &SetupDoctorState) -> (String, StateTone, String) {
    match state {
        SetupDoctorState::Idle => (
            "Not run".to_string(),
            StateTone::Warn,
            "Run Doctor from Connected Tools when endpoint and client state disagree.".to_string(),
        ),
        SetupDoctorState::Running { target, started_at } => (
            "Checking".to_string(),
            StateTone::Warn,
            format!(
                "{} Doctor started {}s ago.",
                target.label(),
                started_at.elapsed().as_secs()
            ),
        ),
        SetupDoctorState::Succeeded {
            report,
            completed_at,
            ..
        } => (
            report.endpoint.status.replace('_', " "),
            setup_doctor_endpoint_tone(&report.endpoint.status),
            format!("{} ({})", report.endpoint.detail, format_age(*completed_at)),
        ),
        SetupDoctorState::Failed {
            target,
            message,
            completed_at,
        } => (
            "Doctor failed".to_string(),
            StateTone::Bad,
            format!(
                "{} Doctor failed: {message} ({})",
                target.label(),
                format_age(*completed_at)
            ),
        ),
    }
}

fn mcp_probe_ready_for(probe: &McpProbeState, active_profile: &str) -> bool {
    matches!(
        probe,
        McpProbeState::Succeeded { summary, .. } if summary.profile == active_profile
    )
}

fn mcp_probe_status(probe: &McpProbeState) -> (String, StateTone, String) {
    match probe {
        McpProbeState::Idle => (
            "Not probed".to_string(),
            StateTone::Warn,
            "Run Probe MCP to confirm initialize and tools/list.".to_string(),
        ),
        McpProbeState::Running {
            profile,
            started_at,
        } => (
            "Probing MCP".to_string(),
            StateTone::Warn,
            format!(
                "profile `{profile}`; started {}s ago",
                started_at.elapsed().as_secs()
            ),
        ),
        McpProbeState::Succeeded {
            summary,
            completed_at,
        } => (
            "MCP verified".to_string(),
            StateTone::Good,
            format!(
                "profile `{}`: {} {} via protocol {}; {} tool(s); {}; session {} closed ({})",
                summary.profile,
                summary.server_name,
                summary.server_version,
                summary.protocol_version,
                summary.tool_count,
                if summary.used_bearer_token {
                    "keychain bearer auth"
                } else {
                    "no bearer auth"
                },
                short_session_id(&summary.session_id),
                format_age(*completed_at)
            ),
        ),
        McpProbeState::Failed {
            profile,
            message,
            completed_at,
        } => (
            "Probe failed".to_string(),
            StateTone::Bad,
            format!(
                "profile `{profile}`: {message} ({})",
                format_age(*completed_at)
            ),
        ),
    }
}

fn mcp_probe_action_status(probe: &McpProbeState) -> String {
    let (text, _, detail) = mcp_probe_status(probe);
    format!("{text}: {detail}")
}

fn mcp_probe_profile_label(probe: &McpProbeState) -> String {
    match probe {
        McpProbeState::Running { profile, .. } | McpProbeState::Failed { profile, .. } => {
            profile.clone()
        }
        McpProbeState::Succeeded { summary, .. } => summary.profile.clone(),
        McpProbeState::Idle => "default".to_string(),
    }
}

fn tool_config_status(row: &ToolConfigRow) -> (String, StateTone, String) {
    let path = row
        .path
        .as_deref()
        .map(display_path)
        .unwrap_or_else(|| "config path unavailable".to_string());
    (
        row.state.label().to_string(),
        row.state.tone(),
        format!(
            "{}; transport {}; {}; {}",
            row.detail,
            row.transport.label(),
            row.profile_route.label(),
            path
        ),
    )
}

fn draw_tool_verification_details(
    ui: &mut egui::Ui,
    row: &ToolConfigRow,
    daemon_default_profile: &str,
) {
    ui.label(RichText::new(format!("{} details", row.target.label())).strong());
    ui.add_space(4.0);
    egui::Grid::new("tool_verification_details_grid")
        .num_columns(2)
        .spacing([18.0, 6.0])
        .striped(true)
        .show(ui, |ui| {
            for (label, value) in tool_verification_detail_rows(row, daemon_default_profile) {
                ui.label(RichText::new(label).strong());
                ui.add(egui::Label::new(value).wrap());
                ui.end_row();
            }
        });
}

fn tool_verification_detail_rows(
    row: &ToolConfigRow,
    daemon_default_profile: &str,
) -> Vec<(&'static str, String)> {
    let config_path = row
        .path
        .as_deref()
        .map(display_path)
        .unwrap_or_else(|| "config path unavailable".to_string());
    let mut rows = vec![
        ("Config", format!("{}: {}", row.state.label(), row.detail)),
        ("Transport", row.transport.label().to_string()),
        (
            "Profile route",
            tool_profile_route_label(&row.profile_route, daemon_default_profile),
        ),
        (
            "Route detail",
            tool_profile_route_detail(&row.profile_route, daemon_default_profile),
        ),
        ("Config file", config_path),
        (
            "Last action",
            tool_last_status_label(row.last_status.as_ref()),
        ),
        (
            "Last action detail",
            tool_last_status_detail(row.last_status.as_ref()),
        ),
    ];
    if let Some(profile) = row
        .last_status
        .as_ref()
        .and_then(|status| status.resolved_profile.as_deref())
        .filter(|profile| !profile.is_empty())
    {
        rows.push(("Resolved profile", profile.to_string()));
    }
    rows
}

fn tool_daemon_mcp_status(
    row: &ToolConfigRow,
    health: DaemonHealth,
    probe: &McpProbeState,
    daemon_default_profile: &str,
) -> (String, StateTone, String) {
    if row.state != ToolConfigState::Verified {
        return (
            "Waiting for config".to_string(),
            row.state.tone(),
            format!(
                "{} config is {}; fix setup before probing MCP.",
                row.target.label(),
                row.state.label()
            ),
        );
    }

    let Some(profile) = probe_profile_for_route(&row.profile_route, daemon_default_profile) else {
        return (
            "Route unknown".to_string(),
            StateTone::Bad,
            "Solo cannot resolve which profile this client would use.".to_string(),
        );
    };

    if health != DaemonHealth::Healthy {
        return (
            "Daemon not ready".to_string(),
            StateTone::Warn,
            format!("Start Solo before probing profile `{profile}`."),
        );
    }

    match probe {
        McpProbeState::Idle => (
            "Not probed".to_string(),
            StateTone::Warn,
            format!("Run Probe MCP for profile `{profile}` to check initialize and tools/list."),
        ),
        McpProbeState::Running {
            profile: running, ..
        } if running == &profile => (
            "Probing".to_string(),
            StateTone::Warn,
            format!("Checking Solo MCP for profile `{profile}`."),
        ),
        McpProbeState::Running {
            profile: running, ..
        } => (
            "Needs probe".to_string(),
            StateTone::Warn,
            format!("Currently probing `{running}`; this client resolves to `{profile}`."),
        ),
        McpProbeState::Succeeded { summary, .. } if summary.profile == profile => {
            let auth_note = if summary.used_bearer_token {
                "Tray probe used OS keychain bearer auth; this does not prove the app client has auth configured."
            } else {
                "Tray probe did not require bearer auth."
            };
            (
                "Tray probe OK".to_string(),
                StateTone::Good,
                format!(
                    "Solo MCP responded to initialize and tools/list for profile `{}` with {} tool(s). {auth_note}",
                    summary.profile, summary.tool_count
                ),
            )
        }
        McpProbeState::Succeeded { summary, .. } => (
            "Needs probe".to_string(),
            StateTone::Warn,
            format!(
                "Last tray probe checked `{}`; this client resolves to `{profile}`.",
                summary.profile
            ),
        ),
        McpProbeState::Failed {
            profile: failed,
            message,
            ..
        } if failed == &profile => (
            "Probe failed".to_string(),
            StateTone::Bad,
            format!("Solo MCP probe failed for profile `{profile}`: {message}"),
        ),
        McpProbeState::Failed {
            profile: failed, ..
        } => (
            "Needs probe".to_string(),
            StateTone::Warn,
            format!("Last failed probe checked `{failed}`; this client resolves to `{profile}`."),
        ),
    }
}

fn tool_client_load_status(
    row: &ToolConfigRow,
    client_check: &ClientCheckState,
) -> (String, StateTone, String) {
    if row.state != ToolConfigState::Verified {
        return (
            "Waiting for config".to_string(),
            row.state.tone(),
            "Client load cannot be checked until the Solo MCP config is valid.".to_string(),
        );
    }

    match client_check {
        ClientCheckState::Running { target, started_at } if *target == row.target => {
            return (
                "Checking".to_string(),
                StateTone::Warn,
                format!(
                    "{} client check started {}s ago.",
                    row.target.label(),
                    started_at.elapsed().as_secs()
                ),
            );
        }
        ClientCheckState::Succeeded {
            target,
            summary,
            completed_at,
        } if *target == row.target => {
            return (
                "Client loaded".to_string(),
                StateTone::Good,
                format!(
                    "{} loaded Solo: {summary} ({})",
                    row.target.label(),
                    format_age(*completed_at)
                ),
            );
        }
        ClientCheckState::Failed {
            target,
            message,
            completed_at,
        } if *target == row.target => {
            return (
                "Check failed".to_string(),
                StateTone::Bad,
                format!(
                    "{} client check failed: {message} ({})",
                    row.target.label(),
                    format_age(*completed_at)
                ),
            );
        }
        _ => {}
    }

    match row.target {
        SetupTarget::CodexUser | SetupTarget::CodexProject => (
            "Manual smoke".to_string(),
            StateTone::Warn,
            "Solo verified the config file and can probe the daemon separately. Run the Codex check if the Codex CLI is available, or confirm `solo` inside Codex with `codex mcp list`.".to_string(),
        ),
        SetupTarget::ClaudeDesktop => (
            "Restart needed".to_string(),
            StateTone::Warn,
            "Restart Claude Desktop, then confirm `solo` appears in the client's tool list. Solo cannot prove that from the tray yet.".to_string(),
        ),
        SetupTarget::Cursor => (
            "Manual smoke".to_string(),
            StateTone::Warn,
            "Confirm Cursor loaded `solo` in its MCP tools. Solo cannot prove that from the tray yet.".to_string(),
        ),
    }
}

fn client_check_status(state: &ClientCheckState) -> String {
    match state {
        ClientCheckState::Idle => {
            "Client check idle; Codex rows can run `codex mcp list` when the Codex CLI is available on PATH.".to_string()
        }
        ClientCheckState::Running { target, started_at } => format!(
            "{} client check running ({}s)",
            target.label(),
            started_at.elapsed().as_secs()
        ),
        ClientCheckState::Succeeded {
            target,
            summary,
            completed_at,
        } => format!(
            "{} client check succeeded: {summary} ({})",
            target.label(),
            format_age(*completed_at)
        ),
        ClientCheckState::Failed {
            target,
            message,
            completed_at,
        } => format!(
            "{} client check failed: {message} ({})",
            target.label(),
            format_age(*completed_at)
        ),
    }
}

fn setup_doctor_status(state: &SetupDoctorState) -> String {
    match state {
        SetupDoctorState::Idle => {
            "Doctor idle; run it when config, daemon, and client status disagree.".to_string()
        }
        SetupDoctorState::Running { target, started_at } => format!(
            "{} Doctor running ({}s)",
            target.label(),
            started_at.elapsed().as_secs()
        ),
        SetupDoctorState::Succeeded {
            target,
            report,
            completed_at,
        } => format!(
            "{} Doctor finished: MCP {}; {} ({})",
            target.label(),
            report.endpoint.status.replace('_', " "),
            setup_doctor_client_summary(report),
            format_age(*completed_at)
        ),
        SetupDoctorState::Failed {
            target,
            message,
            completed_at,
        } => format!(
            "{} Doctor failed: {message} ({})",
            target.label(),
            format_age(*completed_at)
        ),
    }
}

fn setup_doctor_client_summary(report: &SetupDoctorReport) -> String {
    let Some(client) = report.clients.first() else {
        return "no client rows".to_string();
    };
    format!(
        "{} config {}, Solo {}",
        client.display_name,
        client.config_status.replace('_', " "),
        client.solo_entry.replace('_', " ")
    )
}

fn setup_doctor_endpoint_tone(status: &str) -> StateTone {
    match status {
        "reachable" | "auth_required" => StateTone::Good,
        "wrong_path" | "unsupported" => StateTone::Warn,
        _ => StateTone::Bad,
    }
}

fn setup_doctor_client_tone(client: &SetupDoctorClient) -> StateTone {
    match (client.config_status.as_str(), client.solo_entry.as_str()) {
        ("ok", "installed") => StateTone::Good,
        ("invalid", _) | (_, "invalid") => StateTone::Bad,
        _ => StateTone::Warn,
    }
}

fn setup_doctor_tools_status(tools: &SetupDoctorTools) -> (String, StateTone, String) {
    if tools.missing_required_tools.is_empty() {
        (
            format!("{} listed", tools.tool_count),
            StateTone::Good,
            "Critical memory tools are available.".to_string(),
        )
    } else {
        (
            format!("{} listed", tools.tool_count),
            StateTone::Warn,
            format!(
                "Missing critical tool(s): {}",
                tools.missing_required_tools.join(", ")
            ),
        )
    }
}

fn draw_setup_doctor_report(ui: &mut egui::Ui, state: &SetupDoctorState) {
    match state {
        SetupDoctorState::Succeeded {
            target,
            report,
            completed_at,
        } => {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Doctor result: {}", target.label())).strong());
            ui.add_space(4.0);
            egui::Grid::new("setup_doctor_result_grid")
                .num_columns(3)
                .spacing([16.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label(RichText::new("Check").strong());
                    ui.label(RichText::new("Status").strong());
                    ui.label(RichText::new("Detail").strong());
                    ui.end_row();

                    if let Some(profile_route) = report.profile_route.as_deref() {
                        ui.label("Profile route");
                        ui.label(state_text(
                            "checked",
                            StateTone::Good,
                            ui.visuals().dark_mode,
                        ));
                        ui.add(egui::Label::new(profile_route).wrap());
                        ui.end_row();
                    }

                    ui.label("MCP endpoint");
                    ui.label(state_text(
                        &report.endpoint.status.replace('_', " "),
                        setup_doctor_endpoint_tone(&report.endpoint.status),
                        ui.visuals().dark_mode,
                    ));
                    let http = report
                        .endpoint
                        .http_status
                        .map(|status| format!(" HTTP {status};"))
                        .unwrap_or_default();
                    ui.add(
                        egui::Label::new(format!(
                            "{};{} {}",
                            report.endpoint.url, http, report.endpoint.detail
                        ))
                        .wrap(),
                    );
                    ui.end_row();

                    if let Some(tools) = report.endpoint.tools.as_ref() {
                        let (tools_text, tools_tone, tools_detail) =
                            setup_doctor_tools_status(tools);
                        ui.label("MCP tools");
                        ui.label(state_text(&tools_text, tools_tone, ui.visuals().dark_mode));
                        ui.add(egui::Label::new(tools_detail).wrap());
                        ui.end_row();
                    }

                    for client in &report.clients {
                        ui.label(&client.display_name);
                        ui.label(state_text(
                            &format!(
                                "{} / {}",
                                client.config_status.replace('_', " "),
                                client.solo_entry.replace('_', " ")
                            ),
                            setup_doctor_client_tone(client),
                            ui.visuals().dark_mode,
                        ));
                        let path = client
                            .config_path
                            .as_deref()
                            .unwrap_or("config path unavailable");
                        ui.add(egui::Label::new(format!("{path}; {}", client.detail)).wrap());
                        ui.end_row();
                    }
                });
            ui.label(RichText::new(format_age(*completed_at)).weak());
        }
        SetupDoctorState::Failed {
            target,
            message,
            completed_at,
        } => {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "{} Doctor failed: {message} ({})",
                    target.label(),
                    format_age(*completed_at)
                ))
                .color(error_color(ui.visuals().dark_mode)),
            );
        }
        SetupDoctorState::Running { .. } | SetupDoctorState::Idle => {}
    }
}

fn tool_profile_route_label(route: &ToolProfileRoute, daemon_default_profile: &str) -> String {
    let _ = (route, daemon_default_profile);
    "Community library".to_string()
}

fn tool_profile_route_detail(route: &ToolProfileRoute, daemon_default_profile: &str) -> String {
    let _ = (route, daemon_default_profile);
    "Solo Community uses one local memory library.".to_string()
}

fn short_session_id(session_id: &str) -> String {
    if session_id.len() <= 8 {
        session_id.to_string()
    } else {
        format!("{}...", &session_id[..8])
    }
}

fn should_show_start_controls(state: Option<&SupervisorState>) -> bool {
    matches!(
        state,
        None | Some(
            SupervisorState::Locked | SupervisorState::StartupFailed(_) | SupervisorState::Stopped
        )
    )
}

fn render_state_row(ui: &mut egui::Ui, label: &str, state: &str, tone: StateTone, detail: &str) {
    let dark_mode = ui.visuals().dark_mode;
    ui.label(label);
    ui.label(state_text(state, tone, dark_mode));
    ui.add(egui::Label::new(detail).wrap());
    ui.end_row();
}

fn render_wizard_step(
    ui: &mut egui::Ui,
    label: &str,
    step_state: SetupWizardStepState,
    state: &str,
    detail: &str,
) {
    let dark_mode = ui.visuals().dark_mode;
    ui.label(label);
    ui.label(state_text(step_state.label(), step_state.tone(), dark_mode));
    ui.label(state);
    ui.add(egui::Label::new(detail).wrap());
    ui.end_row();
}

fn state_text(text: &str, tone: StateTone, dark_mode: bool) -> RichText {
    RichText::new(text).color(match tone {
        StateTone::Good => success_color(dark_mode),
        StateTone::Warn => warning_color(dark_mode),
        StateTone::Bad => error_color(dark_mode),
    })
}

fn render_command_row(ui: &mut egui::Ui, label: &str, command: &str) {
    ui.label(label);
    ui.label(RichText::new(command).text_style(TextStyle::Monospace));
    if ui.button("Copy").clicked() {
        ui.ctx().copy_text(command.to_string());
    }
    ui.end_row();
}

fn render_detection_row(ui: &mut egui::Ui, label: &str, ok: bool, value: String) {
    ui.label(label);
    ui.label(if ok {
        RichText::new("found").color(egui::Color32::from_rgb(40, 180, 80))
    } else {
        RichText::new("missing").color(egui::Color32::from_rgb(220, 180, 60))
    });
    ui.label(value);
    ui.end_row();
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn display_user_path(path: &Path) -> String {
    let raw = display_path(path);
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw
    }
}

fn default_project_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_string()
}

fn slugify_project_id(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

fn shell_arg(value: &str) -> String {
    let is_simple = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '\\' | ':' | '.' | '-' | '_' | '=' | '@')
        });
    if is_simple {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
}

fn project_init_command(root: &Path) -> String {
    format!("solo project init {}", shell_arg(&display_path(root)))
}

fn project_ingest_dry_run_command(root: &Path) -> String {
    format!(
        "solo project ingest {} --dry-run --json",
        shell_arg(&display_path(root))
    )
}

fn project_facts_json_command(root: &Path, subject: &str, data_dir: &Path) -> String {
    let mut command = format!(
        "solo project facts {} --limit 12 --json --data-dir {}",
        shell_arg(&display_path(root)),
        shell_arg(&display_path(data_dir)),
    );
    let subject = subject.trim();
    if !subject.is_empty() {
        command.push_str(" --subject ");
        command.push_str(&shell_arg(subject));
    }
    command
}

fn project_decision_add_json_command(root: &Path, decision: &str, data_dir: &Path) -> String {
    format!(
        "solo project decisions {} --add {} --json --data-dir {}",
        shell_arg(&display_path(root)),
        shell_arg(decision.trim()),
        shell_arg(&display_path(data_dir)),
    )
}

fn project_decision_search_json_command(root: &Path, query: &str, data_dir: &Path) -> String {
    format!(
        "solo project decisions {} --query {} --limit 25 --json --data-dir {}",
        shell_arg(&display_path(root)),
        shell_arg(query.trim()),
        shell_arg(&display_path(data_dir)),
    )
}

fn project_codex_setup_command(root: &Path, mcp_url: &str) -> String {
    format!(
        "solo setup-client codex --scope project --project-dir {} --transport http --url {} --dry-run",
        shell_arg(&display_path(root)),
        shell_arg(mcp_url)
    )
}

fn project_agent_policy_command(root: &Path, client: ProjectPolicyClient) -> String {
    format!(
        "solo project policy {} --client {}",
        shell_arg(&display_path(root)),
        client.as_str()
    )
}

fn project_policy_context(
    snapshot: &ProjectMemorySnapshot,
) -> Option<(&Path, ProjectMemoryConfig)> {
    let root = snapshot.root.as_deref()?;
    if matches!(
        snapshot.state,
        ProjectMemoryState::NotSelected
            | ProjectMemoryState::MissingRoot
            | ProjectMemoryState::InvalidConfig
    ) {
        return None;
    }
    let config = snapshot.config.clone().unwrap_or_else(|| {
        let name = default_project_name(root);
        ProjectMemoryConfig {
            project_id: slugify_project_id(&name),
            name,
            tags: Vec::new(),
        }
    });
    Some((root, config))
}

fn project_agent_policy(
    snapshot: &ProjectMemorySnapshot,
    client: ProjectPolicyClient,
) -> Option<String> {
    let (root, config) = project_policy_context(snapshot)?;
    Some(render_project_agent_policy(root, &config, client))
}

fn render_project_agent_policy(
    root: &Path,
    config: &ProjectMemoryConfig,
    client: ProjectPolicyClient,
) -> String {
    render_project_policy(client, &project_descriptor(config, root))
}

fn project_action_args(kind: ProjectActionKind, root: &Path) -> Vec<std::ffi::OsString> {
    let mut args = os_args(["project"]);
    match kind {
        ProjectActionKind::Init => {
            args.push("init".into());
            args.push(root.into());
        }
        ProjectActionKind::Preview => {
            args.push("ingest".into());
            args.push(root.into());
            args.push("--dry-run".into());
            args.push("--json".into());
        }
    }
    args
}

fn import_preview_args(
    source: ImportSource,
    path: &Path,
    data_dir: &Path,
) -> Vec<std::ffi::OsString> {
    os_args(["import", source.command()])
        .into_iter()
        .chain([
            path.as_os_str().into(),
            "--dry-run".into(),
            "--json".into(),
            "--data-dir".into(),
            data_dir.as_os_str().into(),
        ])
        .collect()
}

fn import_preview_command(source: ImportSource, path: &Path, data_dir: &Path) -> String {
    format!(
        "solo import {} {} --dry-run --data-dir {}",
        source.command(),
        shell_arg(&display_path(path)),
        shell_arg(&display_path(data_dir))
    )
}

fn os_args<const N: usize>(args: [&str; N]) -> Vec<std::ffi::OsString> {
    args.into_iter().map(std::ffi::OsString::from).collect()
}

fn setup_action_status(action: &SetupActionState) -> String {
    match action {
        SetupActionState::Idle => {
            "Choose Apply to write a backed-up client config, or Verify to check it.".to_string()
        }
        SetupActionState::Running {
            target,
            verb,
            started_at,
        } => format!(
            "{} running for {} ({}s)",
            verb.label(),
            target.label(),
            started_at.elapsed().as_secs()
        ),
        SetupActionState::Succeeded {
            target,
            verb,
            message,
            completed_at,
        } => format!(
            "{} {} succeeded: {} ({})",
            target.label(),
            verb.label(),
            message,
            format_age(*completed_at)
        ),
        SetupActionState::Failed {
            target,
            verb,
            message,
            completed_at,
        } => format!(
            "{} {} failed: {} ({})",
            target.label(),
            verb.label(),
            message,
            format_age(*completed_at)
        ),
    }
}

fn tool_last_status_label(last_status: Option<&ConnectedToolLastStatus>) -> String {
    let Some(last_status) = last_status else {
        return "not checked".to_string();
    };
    let age = last_status
        .updated_at_ms
        .and_then(system_time_from_unix_ms)
        .map(format_age)
        .unwrap_or_else(|| "time unknown".to_string());
    let status = last_status.status.replace('_', " ");
    match last_status
        .resolved_profile
        .as_deref()
        .or(last_status.profile_route.as_deref())
    {
        Some(route) if !route.is_empty() => format!("{status}: {route} ({age})"),
        _ => format!("{status} ({age})"),
    }
}

fn tool_last_status_detail(last_status: Option<&ConnectedToolLastStatus>) -> String {
    let Some(last_status) = last_status else {
        return "No setup or verify action has run from this tray yet.".to_string();
    };
    let mut parts = Vec::new();
    if let Some(state) = last_status
        .config_state
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("Config: {state}"));
    }
    if let Some(transport) = last_status
        .transport
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("Transport: {transport}"));
    }
    if let Some(route) = last_status
        .profile_route
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("Route: {route}"));
    }
    if let Some(profile) = last_status
        .resolved_profile
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("Resolved profile: {profile}"));
    }
    if let Some(path) = last_status
        .config_path
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("Path: {path}"));
    }
    if !last_status.detail.is_empty() {
        parts.push(last_status.detail.clone());
    }
    if parts.is_empty() {
        "No detail recorded.".to_string()
    } else {
        parts.join("\n")
    }
}

fn secret_snapshot_status(snapshot: &SecretSnapshot) -> String {
    if let Some(error) = &snapshot.last_error {
        return format!("unavailable: {error}");
    }
    format!(
        "passphrase {}; token {}",
        secret_item_status(snapshot.passphrase_stored),
        secret_item_status(snapshot.bearer_token_stored),
    )
}

fn secret_item_status(stored: Option<bool>) -> &'static str {
    match stored {
        Some(true) => "stored",
        Some(false) => "not stored",
        None => "not checked",
    }
}

fn keychain_passphrase_status(
    snapshot: &SecretSnapshot,
    remember_enabled: bool,
    save_pending: bool,
) -> (String, StateTone, String) {
    if let Some(error) = &snapshot.last_error {
        return (
            "Unavailable".to_string(),
            StateTone::Bad,
            format!("{} could not be checked: {error}", snapshot.backend),
        );
    }
    if !remember_enabled {
        return (
            "Off".to_string(),
            StateTone::Warn,
            "Enable remember to store the daemon passphrase in the OS keychain.".to_string(),
        );
    }
    if save_pending {
        return (
            "Saving".to_string(),
            StateTone::Warn,
            "Passphrase will be saved to the OS keychain after Solo starts.".to_string(),
        );
    }
    match snapshot.passphrase_stored {
        Some(true) => (
            "Stored".to_string(),
            StateTone::Good,
            format!(
                "{} has a daemon passphrase for keychain start.",
                snapshot.backend
            ),
        ),
        Some(false) => (
            "Not stored".to_string(),
            StateTone::Warn,
            "Enter the passphrase once and Start Solo; it will be saved immediately.".to_string(),
        ),
        None => (
            "Not checked".to_string(),
            StateTone::Warn,
            "Refresh keychain state if this does not update.".to_string(),
        ),
    }
}

fn embedder_runtime_status(
    payload: Option<&serde_json::Value>,
    health: DaemonHealth,
    last_error: Option<&str>,
) -> (String, StateTone, String) {
    let Some(json) = payload else {
        let detail = last_error
            .map(|error| format!("No /v1/status payload yet: {error}"))
            .unwrap_or_else(|| "Waiting for /v1/status to report embedder runtime.".to_string());
        return match health {
            DaemonHealth::Down => ("Offline".to_string(), StateTone::Bad, detail),
            DaemonHealth::Healthy | DaemonHealth::Starting => {
                ("Checking".to_string(), StateTone::Warn, detail)
            }
        };
    };

    let summary = status_embedder_summary(Some(json));
    let running = status_payload_bool(Some(json), "/embedder/runtime/running");
    let probe_status = status_payload_opt_string(Some(json), "/embedder/runtime/status")
        .unwrap_or_else(|| "not_reported".to_string());
    let probe_detail =
        status_payload_opt_string(Some(json), "/embedder/runtime/detail").unwrap_or_default();
    let detail = if probe_detail.is_empty() {
        summary.clone()
    } else {
        format!("{summary}; {probe_detail}")
    };

    match running {
        Some(true) => ("Ready".to_string(), StateTone::Good, detail),
        Some(false) => {
            let label = match probe_status.as_str() {
                "timeout" => "Timeout",
                "error" => "Offline",
                _ => "Unavailable",
            };
            (label.to_string(), StateTone::Bad, detail)
        }
        None => (
            "Configured".to_string(),
            StateTone::Warn,
            format!("{summary}; runtime probe not reported by this daemon."),
        ),
    }
}

fn steward_runtime_status(
    payload: Option<&serde_json::Value>,
    health: DaemonHealth,
) -> (String, StateTone, String) {
    let Some(json) = payload else {
        let detail = match health {
            DaemonHealth::Down => "Start Solo before checking Steward runtime.",
            DaemonHealth::Healthy | DaemonHealth::Starting => {
                "Waiting for /v1/status Steward fields."
            }
        };
        return (
            "Not reported".to_string(),
            StateTone::Warn,
            detail.to_string(),
        );
    };

    let running = status_payload_bool(Some(json), "/steward/running")
        .or_else(|| status_payload_bool(Some(json), "/steward/runtime_wired"))
        .unwrap_or(false);
    let status = status_payload_opt_string(Some(json), "/steward/status")
        .unwrap_or_else(|| if running { "running" } else { "not_reported" }.to_string());
    let mut detail = status_payload_opt_string(Some(json), "/steward/note")
        .or_else(|| status_payload_opt_string(Some(json), "/steward/runtime_llm"))
        .unwrap_or_else(|| "Steward runtime was not described by /v1/status.".to_string());
    if let Some(pending) = status_payload_opt_string(Some(json), "/steward/pending_clusters")
        && pending != "0"
    {
        detail = format!("{detail}; {pending} pending cluster(s)");
    }

    match status.as_str() {
        "ready" => ("Ready".to_string(), StateTone::Good, detail),
        "disabled" => ("Disabled".to_string(), StateTone::Warn, detail),
        "not_wired" => ("Not wired".to_string(), StateTone::Warn, detail),
        "no_llm" => ("No LLM".to_string(), StateTone::Warn, detail),
        "pending" => ("Pending".to_string(), StateTone::Warn, detail),
        _ if running => ("Running".to_string(), StateTone::Good, detail),
        _ => ("Not running".to_string(), StateTone::Warn, detail),
    }
}

fn secret_action_status(action: &SecretActionState) -> String {
    match action {
        SecretActionState::Idle => "keychain idle".to_string(),
        SecretActionState::Succeeded {
            message,
            completed_at,
        } => format!("{message} ({})", format_age(*completed_at)),
        SecretActionState::Failed {
            message,
            completed_at,
        } => format!("{message} ({})", format_age(*completed_at)),
    }
}

fn first_run_init_status(state: &FirstRunInitState) -> String {
    match state {
        FirstRunInitState::Idle => "first-run setup idle".to_string(),
        FirstRunInitState::Running { started_at } => {
            format!(
                "creating encrypted memory ({}s)",
                started_at.elapsed().as_secs()
            )
        }
        FirstRunInitState::Succeeded {
            message,
            completed_at,
        } => format!("{message} ({})", format_age(*completed_at)),
        FirstRunInitState::Failed {
            message,
            completed_at,
        } => format!(
            "first-run setup failed: {message} ({})",
            format_age(*completed_at)
        ),
    }
}

fn probe_profile_for_route(route: &ToolProfileRoute, active_profile: &str) -> Option<String> {
    match route {
        ToolProfileRoute::DaemonDefault => Some(active_profile.to_string()),
        #[cfg(test)]
        ToolProfileRoute::Explicit(_) => None,
        ToolProfileRoute::Unknown => None,
    }
}

async fn run_first_run_init(
    data_dir: PathBuf,
    passphrase: Zeroizing<String>,
    first_name: String,
) -> Result<FirstRunInitSuccess, String> {
    let daemon_passphrase = Zeroizing::new(passphrase.as_str().to_string());
    let embedder = probe_embedder_config_from_env()
        .await
        .map_err(|e| format!("probe embedder config: {e}"))?;
    let params = InitParams {
        data_dir,
        passphrase,
        force: false,
        embedder,
    };
    let outcome = tokio::task::spawn_blocking(move || solo_storage::init(params))
        .await
        .map_err(|e| format!("first-run init worker failed: {e}"))?
        .map_err(|e| format!("solo init failed: {e}"))?;
    let user_alias_set = apply_first_run_user_alias(&outcome.config_path, &first_name)?;

    Ok(FirstRunInitSuccess {
        passphrase: daemon_passphrase,
        data_dir: outcome.data_dir,
        config_path: outcome.config_path,
        schema_version: outcome.schema_version,
        user_alias_set,
    })
}

fn apply_first_run_user_alias(config_path: &Path, first_name: &str) -> Result<bool, String> {
    let trimmed = first_name.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }

    let mut cfg = SoloConfig::read(config_path)
        .map_err(|e| format!("read config back from {}: {e}", config_path.display()))?;
    if !cfg.identity.user_aliases.is_empty() {
        return Ok(false);
    }

    cfg.identity.user_aliases = vec![trimmed.to_lowercase()];
    std::fs::remove_file(config_path).map_err(|e| {
        format!(
            "remove old config before alias rewrite at {}: {e}",
            config_path.display()
        )
    })?;
    cfg.write(config_path).map_err(|e| {
        format!(
            "rewrite config with user alias at {}: {e}",
            config_path.display()
        )
    })?;

    Ok(true)
}

fn run_setup_client_action(
    solo_bin: PathBuf,
    args: Vec<std::ffi::OsString>,
    target: SetupTarget,
    verb: SetupActionVerb,
    expected_profile_route: ExpectedToolProfileRoute,
    project_root: Option<PathBuf>,
) -> Result<SetupActionSuccess, String> {
    let primary_summary = run_solo_command(&solo_bin, args)?;
    let command_summary = if verb == SetupActionVerb::Apply {
        let verify_summary =
            run_solo_command(&solo_bin, target.verify_args(project_root.as_deref()))
                .map_err(|e| format!("setup applied, but post-setup verify failed: {e}"))?;
        format!("apply: {primary_summary}; verify: {verify_summary}")
    } else {
        primary_summary
    };
    let live_row = inspect_tool_config(target, project_root.as_deref());
    let verification = tool_verification_from_row(&live_row);
    validate_tool_verification(target, &expected_profile_route, &verification)?;
    Ok(SetupActionSuccess {
        message: setup_action_success_message(verb, &verification, &command_summary),
        verification,
    })
}

fn tool_verification_from_row(row: &ToolConfigRow) -> ToolVerification {
    ToolVerification {
        state: row.state,
        transport: row.transport,
        profile_route: row.profile_route.clone(),
        detail: row.detail.clone(),
        config_path: row.path.as_deref().map(display_path),
    }
}

fn validate_tool_verification(
    target: SetupTarget,
    expected_profile_route: &ExpectedToolProfileRoute,
    verification: &ToolVerification,
) -> Result<(), String> {
    if verification.state != ToolConfigState::Verified {
        return Err(format!(
            "{} command completed, but live config is {}: {}",
            target.label(),
            verification.state.label(),
            verification.detail
        ));
    }
    if !expected_profile_route.matches_route(&verification.profile_route) {
        return Err(format!(
            "{} config route mismatch: expected {}, found {}. {}",
            target.label(),
            expected_profile_route.label(),
            verification.profile_route.label(),
            verification.detail
        ));
    }
    Ok(())
}

fn setup_action_success_message(
    verb: SetupActionVerb,
    verification: &ToolVerification,
    command_summary: &str,
) -> String {
    let action = match verb {
        SetupActionVerb::Apply => "setup applied and verified",
        SetupActionVerb::Verify => "verified",
    };
    let path = verification
        .config_path
        .as_deref()
        .unwrap_or("config path unavailable");
    summarize_http_body(&format!(
        "{action}; {}; {}; {}; {path}; command: {command_summary}",
        verification.state.label(),
        verification.transport.label(),
        verification.profile_route.label(),
    ))
}

fn run_setup_client_doctor(
    solo_bin: PathBuf,
    args: Vec<std::ffi::OsString>,
) -> Result<SetupDoctorReport, String> {
    let output = run_solo_command_capture(&solo_bin, args)?;
    parse_setup_doctor_report(&output.stdout)
}

fn parse_setup_doctor_report(body: &str) -> Result<SetupDoctorReport, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode setup-client doctor JSON: {e}"))?;
    let endpoint_json = json
        .get("mcp_endpoint")
        .ok_or_else(|| "doctor JSON is missing `mcp_endpoint`".to_string())?;
    let profile_route = json
        .get("profile_route")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let endpoint = SetupDoctorEndpoint {
        url: required_json_string(endpoint_json, "url")?.to_string(),
        status: required_json_string(endpoint_json, "status")?.to_string(),
        detail: required_json_string(endpoint_json, "detail")?.to_string(),
        http_status: endpoint_json
            .get("http_status")
            .and_then(|value| value.as_u64())
            .and_then(|value| u16::try_from(value).ok()),
        tools: parse_setup_doctor_tools(endpoint_json.get("tools"))?,
    };
    let clients_json = json
        .get("clients")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "doctor JSON is missing `clients`".to_string())?;
    let clients = clients_json
        .iter()
        .map(parse_setup_doctor_client)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SetupDoctorReport {
        profile_route,
        endpoint,
        clients,
    })
}

fn parse_setup_doctor_tools(
    value: Option<&serde_json::Value>,
) -> Result<Option<SetupDoctorTools>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let tool_count = value
        .get("tool_count")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "doctor endpoint tools missing tool_count".to_string())?;
    let missing_required_tools = value
        .get("missing_required_tools")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "doctor endpoint tools missing missing_required_tools".to_string())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "doctor endpoint tools include a non-string item".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(SetupDoctorTools {
        tool_count,
        missing_required_tools,
    }))
}

fn parse_setup_doctor_client(value: &serde_json::Value) -> Result<SetupDoctorClient, String> {
    Ok(SetupDoctorClient {
        client: required_json_string(value, "client")?.to_string(),
        display_name: required_json_string(value, "display_name")?.to_string(),
        config_path: value
            .get("config_path")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        config_status: required_json_string(value, "config_status")?.to_string(),
        solo_entry: required_json_string(value, "solo_entry")?.to_string(),
        detail: required_json_string(value, "detail")?.to_string(),
    })
}

fn run_project_action(
    solo_bin: PathBuf,
    args: Vec<std::ffi::OsString>,
    kind: ProjectActionKind,
) -> Result<ProjectActionSuccess, String> {
    let output = run_solo_command_capture(&solo_bin, args)?;
    Ok(ProjectActionSuccess {
        kind,
        message: output.summary,
        output: output.text,
    })
}

fn run_import_preview(
    solo_bin: PathBuf,
    args: Vec<std::ffi::OsString>,
    source: ImportSource,
    path: PathBuf,
) -> Result<ImportActionSuccess, String> {
    let output = run_solo_command_capture(&solo_bin, args)?;
    let preview = parse_import_preview_response(source, &output.text)?;
    Ok(ImportActionSuccess {
        source,
        path,
        message: preview.message,
        output: preview.output,
    })
}

struct ImportPreviewDisplay {
    message: String,
    output: String,
}

fn parse_import_preview_response(
    source: ImportSource,
    body: &str,
) -> Result<ImportPreviewDisplay, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode import preview JSON: {e}"))?;
    let path = required_json_string(&json, "path")?;
    let estimated = json_u64(&json, "estimated_chunk_candidates");
    if matches!(
        source,
        ImportSource::ChatGpt | ImportSource::Claude | ImportSource::Bookmarks
    ) {
        let candidate_records = json_u64(&json, "candidate_records");
        let filtered_records = json_u64(&json, "filtered_records");
        let skipped_records = json_u64(&json, "skipped_records");
        let records_scanned = json_u64(&json, "records_scanned");
        let materialized_format = json
            .get("materialized_format")
            .and_then(|value| value.as_str())
            .unwrap_or("markdown");
        let source_name = json
            .get("source")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| source.picker_label());
        return Ok(ImportPreviewDisplay {
            message: format!(
                "{candidate_records} candidate record(s), {filtered_records} filtered, {skipped_records} skipped, {estimated} estimated chunk(s)"
            ),
            output: format!(
                "import {} --dry-run\npath: {path}\nsource: {source_name}\nrecords scanned: {records_scanned}\ncandidate records: {candidate_records}\nfiltered records: {filtered_records}\nskipped records: {skipped_records}\nestimated chunk candidates: {estimated}\nmaterialized format: {materialized_format}",
                source.command()
            ),
        });
    }

    let files_scanned = json_u64(&json, "files_scanned");
    let candidate_files = json_u64(&json, "candidate_files");
    let skipped_files = json_u64(&json, "skipped_files");
    let source_name = json
        .get("source")
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| source.command())
        .to_string();
    let enabled_extensions = json
        .get("enabled_extensions")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "(none)".to_string());
    Ok(ImportPreviewDisplay {
        message: format!(
            "{candidate_files} candidate file(s), {skipped_files} skipped, {estimated} estimated chunk(s)"
        ),
        output: format!(
            "import {} --dry-run\npath: {path}\nsource: {source_name}\nfiles scanned: {files_scanned}\ncandidate files: {candidate_files}\nskipped files: {skipped_files}\nestimated chunk candidates: {estimated}\nenabled extensions: {enabled_extensions}",
            source.command()
        ),
    })
}

fn json_u64(json: &serde_json::Value, field: &str) -> u64 {
    json.get(field)
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

async fn run_daemon_document_import(
    url: String,
    profile: String,
    source: String,
    path: String,
    recursive: bool,
    max_files: u32,
) -> Result<NativeImportReport, String> {
    let body = serde_json::json!({
        "path": path,
        "source": source,
        "dry_run": false,
        "recursive": recursive,
        "max_files": max_files,
    });
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_native_import_response(&text)
}

async fn run_daemon_project_docs_import(
    url: String,
    profile: String,
    preview: ProjectDocsPreview,
) -> Result<NativeImportReport, String> {
    let mut aggregate = NativeImportReport {
        path: preview.root.clone(),
        dry_run: false,
        recursive: false,
        truncated: false,
        total_files: 0,
        total_bytes: 0,
        store_original_file: false,
        imported: 0,
        deduped: 0,
        failed: 0,
        chunks_persisted: 0,
        assets_retained: 0,
        assets_deduped: 0,
        asset_links: 0,
        asset_failed: 0,
        results: Vec::new(),
    };

    for candidate in preview.candidates {
        let report = run_daemon_document_import(
            url.clone(),
            profile.clone(),
            "native".to_string(),
            candidate.path,
            false,
            1,
        )
        .await?;
        aggregate.truncated |= report.truncated;
        aggregate.total_files += report.total_files;
        aggregate.total_bytes += report.total_bytes;
        aggregate.store_original_file |= report.store_original_file;
        aggregate.imported += report.imported;
        aggregate.deduped += report.deduped;
        aggregate.failed += report.failed;
        aggregate.chunks_persisted += report.chunks_persisted;
        aggregate.assets_retained += report.assets_retained;
        aggregate.assets_deduped += report.assets_deduped;
        aggregate.asset_links += report.asset_links;
        aggregate.asset_failed += report.asset_failed;
        aggregate.results.extend(report.results);
    }

    Ok(aggregate)
}

async fn run_daemon_document_list(
    url: String,
    profile: String,
) -> Result<Vec<DocumentSummary>, String> {
    let text = get_daemon_json_with_keychain_fallback(&url, &profile).await?;
    parse_document_list_response(&text)
}

async fn run_daemon_document_search(
    url: String,
    profile: String,
    query: String,
) -> Result<DocumentSearchSuccess, String> {
    let body = serde_json::json!({
        "query": query.clone(),
        "limit": 8,
    });
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_document_search_response(&query, &text)
}

async fn run_daemon_document_inspect(
    url: String,
    profile: String,
) -> Result<DocumentDetail, String> {
    let text = get_daemon_json_with_keychain_fallback(&url, &profile).await?;
    parse_document_detail_response(&text)
}

async fn run_daemon_document_forget(
    url: String,
    profile: String,
) -> Result<DocumentForgetReport, String> {
    let text = delete_daemon_json_with_keychain_fallback(&url, &profile).await?;
    parse_document_forget_response(&text)
}

struct ClientCheckCommand {
    bin: PathBuf,
    args: Vec<std::ffi::OsString>,
    cwd: Option<PathBuf>,
}

fn client_check_command(
    target: SetupTarget,
    project_root: Option<PathBuf>,
) -> Result<ClientCheckCommand, String> {
    match target {
        SetupTarget::CodexUser => Ok(ClientCheckCommand {
            bin: PathBuf::from("codex"),
            args: os_args(["mcp", "list"]),
            cwd: None,
        }),
        SetupTarget::CodexProject => {
            let Some(root) = project_root else {
                return Err(
                    "select a project root in Projects before running the Codex project check"
                        .to_string(),
                );
            };
            Ok(ClientCheckCommand {
                bin: PathBuf::from("codex"),
                args: os_args(["mcp", "list"]),
                cwd: Some(root),
            })
        }
        SetupTarget::ClaudeDesktop | SetupTarget::Cursor => {
            Err("this client check is manual in Solo for now".to_string())
        }
    }
}

fn run_client_check(
    target: SetupTarget,
    project_root: Option<PathBuf>,
) -> Result<ClientCheckSuccess, String> {
    let check = client_check_command(target, project_root)?;
    let mut command = std::process::Command::new(&check.bin);
    command
        .args(&check.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(cwd) = &check.cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let output = run_command_with_timeout(command, &check.bin, CLIENT_CHECK_TIMEOUT)
        .map_err(|message| client_check_command_error(target, &message))?;
    let summary = summarize_process_output(&output.stdout, &output.stderr);
    let text = display_process_output(&output.stdout, &output.stderr);
    if !output.status.success() {
        return Err(format!(
            "{} exited with status {}: {}",
            client_check_instruction_label(target),
            output.status,
            summary
        ));
    }
    if !codex_mcp_list_contains_solo(&text) {
        return Err(format!(
            "{} did not list `solo`: {}",
            client_check_instruction_label(target),
            summary
        ));
    }
    Ok(ClientCheckSuccess {
        summary: summarize_http_body(&format!(
            "{} listed `solo`: {summary}",
            client_check_instruction_label(target)
        )),
    })
}

fn client_check_instruction_label(target: SetupTarget) -> &'static str {
    match target {
        SetupTarget::CodexUser | SetupTarget::CodexProject => "codex mcp list",
        SetupTarget::ClaudeDesktop => "Claude Desktop MCP tools",
        SetupTarget::Cursor => "Cursor MCP tools",
    }
}

fn client_check_command_error(target: SetupTarget, message: &str) -> String {
    if matches!(target, SetupTarget::CodexUser | SetupTarget::CodexProject) {
        let lower = message.to_ascii_lowercase();
        if lower.contains("access is denied") || lower.contains("permission denied") {
            return format!(
                "Codex CLI is not runnable from PATH: {message}. Use Copy check inside Codex, or install/repair the Codex CLI executable."
            );
        }
        if lower.contains("not found")
            || lower.contains("no such file")
            || lower.contains("the system cannot find")
        {
            return format!(
                "Codex CLI was not found on PATH: {message}. Use Copy check inside Codex, or install the Codex CLI."
            );
        }
    }
    message.to_string()
}

fn codex_mcp_list_contains_solo(output: &str) -> bool {
    output
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
        .any(|token| token.eq_ignore_ascii_case("solo"))
}

fn run_solo_command(solo_bin: &Path, args: Vec<std::ffi::OsString>) -> Result<String, String> {
    run_solo_command_capture(solo_bin, args).map(|output| output.summary)
}

const SOLO_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
const OLLAMA_MIGRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
const CLIENT_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
struct SoloCommandOutput {
    summary: String,
    text: String,
    stdout: String,
}

struct CapturedCommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_solo_command_capture(
    solo_bin: &Path,
    args: Vec<std::ffi::OsString>,
) -> Result<SoloCommandOutput, String> {
    run_solo_command_capture_with_timeout(solo_bin, args, SOLO_COMMAND_TIMEOUT)
}

async fn run_ollama_embedder_migration(
    daemon_handle: Arc<Mutex<DaemonHandle>>,
    solo_bin: PathBuf,
    args: Vec<std::ffi::OsString>,
    passphrase: Zeroizing<String>,
) -> Result<OllamaMigrationSuccess, String> {
    stop_daemon_for_migration(daemon_handle).await?;
    let output = tokio::task::spawn_blocking(move || {
        run_solo_command_capture_with_secret_env_timeout(
            &solo_bin,
            args,
            "SOLO_PASSPHRASE",
            passphrase,
            OLLAMA_MIGRATION_TIMEOUT,
        )
    })
    .await
    .map_err(|e| format!("join migration worker: {e}"))??;
    Ok(OllamaMigrationSuccess {
        summary: output.summary,
    })
}

async fn stop_daemon_for_migration(handle: Arc<Mutex<DaemonHandle>>) -> Result<(), String> {
    let needs_stop = {
        let mut daemon = handle.lock().await;
        if daemon.supervisor_exited
            || matches!(
                daemon.state,
                SupervisorState::Locked
                    | SupervisorState::StartupFailed(_)
                    | SupervisorState::Stopped
            )
        {
            false
        } else {
            daemon.request_quit();
            true
        }
    };
    if !needs_stop {
        return Ok(());
    }

    for _ in 0..120 {
        {
            let daemon = handle.lock().await;
            if daemon.supervisor_exited && !daemon.running {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Err("timed out waiting for Solo daemon to stop before embedder migration".to_string())
}

fn run_solo_command_capture_with_timeout(
    solo_bin: &Path,
    args: Vec<std::ffi::OsString>,
    timeout: std::time::Duration,
) -> Result<SoloCommandOutput, String> {
    let mut command = std::process::Command::new(solo_bin);
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let output = run_command_with_timeout(command, solo_bin, timeout)?;
    let summary = summarize_process_output(&output.stdout, &output.stderr);
    let text = display_process_output(&output.stdout, &output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        Ok(SoloCommandOutput {
            summary,
            text,
            stdout,
        })
    } else {
        Err(format!(
            "{} exited with status {}: {}",
            solo_bin.display(),
            output.status,
            summary
        ))
    }
}

fn run_solo_command_capture_with_secret_env_timeout(
    solo_bin: &Path,
    args: Vec<std::ffi::OsString>,
    env_key: &str,
    env_value: Zeroizing<String>,
    timeout: std::time::Duration,
) -> Result<SoloCommandOutput, String> {
    let mut command = std::process::Command::new(solo_bin);
    command
        .args(&args)
        .env(env_key, env_value.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let output = run_command_with_timeout(command, solo_bin, timeout)?;
    let summary = summarize_process_output(&output.stdout, &output.stderr);
    let text = display_process_output(&output.stdout, &output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        Ok(SoloCommandOutput {
            summary,
            text,
            stdout,
        })
    } else {
        Err(format!(
            "{} exited with status {}: {}",
            solo_bin.display(),
            output.status,
            summary
        ))
    }
}

fn run_command_with_timeout(
    mut command: std::process::Command,
    solo_bin: &Path,
    timeout: std::time::Duration,
) -> Result<CapturedCommandOutput, String> {
    let mut child = command
        .spawn()
        .map_err(|e| format!("run {}: {e}", solo_bin.display()))?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("capture stdout for {}", solo_bin.display()));
    };
    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("capture stderr for {}", solo_bin.display()));
    };
    let stdout_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(format!(
                    "{} timed out after {}s",
                    solo_bin.display(),
                    timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(format!("wait for {}: {e}", solo_bin.display()));
            }
        }
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| format!("capture stdout for {} panicked", solo_bin.display()))?
        .map_err(|e| format!("read stdout for {}: {e}", solo_bin.display()))?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| format!("capture stderr for {} panicked", solo_bin.display()))?
        .map_err(|e| format!("read stderr for {}: {e}", solo_bin.display()))?;

    Ok(CapturedCommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn display_process_output(stdout: &[u8], stderr: &[u8]) -> String {
    const LIMIT: usize = 4_000;
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut text = String::new();
    if !stdout.trim().is_empty() {
        text.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push_str("\n\nstderr:\n");
        }
        text.push_str(stderr.trim());
    }
    if text.chars().count() <= LIMIT {
        return text;
    }
    let mut summary: String = text.chars().take(LIMIT).collect();
    summary.push_str("\n...");
    summary
}

fn summarize_process_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut text = String::new();
    if !stdout.trim().is_empty() {
        text.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push_str(" | ");
        }
        text.push_str(stderr.trim());
    }
    if text.is_empty() {
        "completed with no output".to_string()
    } else {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        summarize_http_body(&text)
    }
}

fn open_path_async(path: PathBuf, label: &'static str) {
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let result = tray::open_in_file_manager(&path);
        match result {
            Ok(()) => tracing::info!(
                target = label,
                path = %path.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "opened path"
            ),
            Err(e) => tracing::warn!(
                target = label,
                error = %e,
                path = %path.display(),
                "failed to open path"
            ),
        }
    });
}

fn default_backup_file_name() -> String {
    let millis = now_unix_ms();
    format!("solo-backup-{millis}.db")
}

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn backup_action_status(action: &BackupActionState, health: DaemonHealth) -> String {
    match action {
        BackupActionState::Idle if health == DaemonHealth::Healthy => {
            "ready; uses daemon HTTP backup".to_string()
        }
        BackupActionState::Idle => "waiting for healthy daemon".to_string(),
        BackupActionState::Running { dest, started_at } => format!(
            "running to {} ({}s)",
            dest.display(),
            started_at.elapsed().as_secs()
        ),
        BackupActionState::Succeeded {
            path,
            elapsed_ms,
            completed_at,
        } => format!(
            "completed {} in {:.2}s ({})",
            path.display(),
            *elapsed_ms as f64 / 1000.0,
            format_age(*completed_at)
        ),
        BackupActionState::Failed {
            message,
            completed_at,
        } => format!("failed: {message} ({})", format_age(*completed_at)),
    }
}

fn ollama_migration_status(action: &OllamaMigrationState) -> String {
    match action {
        OllamaMigrationState::Idle => {
            "ready; stops Solo, migrates embeddings, then restarts".to_string()
        }
        OllamaMigrationState::Running { model, started_at } => {
            format!("migrating to {model} ({}s)", started_at.elapsed().as_secs())
        }
        OllamaMigrationState::Succeeded {
            model,
            summary,
            completed_at,
        } => format!(
            "completed {model}: {} ({})",
            summarize_http_body(summary),
            format_age(*completed_at)
        ),
        OllamaMigrationState::Failed {
            model,
            message,
            completed_at,
        } => format!("failed {model}: {message} ({})", format_age(*completed_at)),
    }
}

fn ollama_migration_command(model: &str, dim: &str, base_url: &str, data_dir: &Path) -> String {
    let mut command = format!(
        "solo migrate-embedder ollama --model {} --base-url {} --data-dir {}",
        shell_arg(model.trim()),
        shell_arg(base_url.trim()),
        shell_arg(&display_path(data_dir))
    );
    let dim = dim.trim();
    if !dim.is_empty() {
        command.push_str(" --dim ");
        command.push_str(&shell_arg(dim));
    }
    command
}

const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
#[cfg(test)]
const SOLO_TENANT_HEADER: &str = "x-solo-tenant";

async fn run_mcp_probe(url: String, profile: String) -> Result<McpProbeSuccess, String> {
    match run_mcp_probe_with_token(&url, &profile, None).await {
        Ok(summary) => Ok(summary),
        Err(McpProbeError::Unauthorized(message)) => {
            let Some(token) = load_bearer_token_from_keychain().await? else {
                return Err(format!("{message}; no bearer token stored in OS keychain"));
            };
            run_mcp_probe_with_token(&url, &profile, Some(token.as_str()))
                .await
                .map_err(|error| match error {
                    McpProbeError::Unauthorized(message) => {
                        format!("keychain bearer token was rejected: {message}")
                    }
                    McpProbeError::Other(message) => message,
                })
        }
        Err(McpProbeError::Other(message)) => Err(message),
    }
}

#[derive(Debug)]
enum McpProbeError {
    Unauthorized(String),
    Other(String),
}

async fn load_bearer_token_from_keychain() -> Result<Option<Zeroizing<String>>, String> {
    tokio::task::spawn_blocking(crate::secret_store::load_bearer_token)
        .await
        .map_err(|error| format!("read bearer token from OS keychain: {error}"))?
}

async fn run_daemon_memory_remember(
    url: String,
    profile: String,
    content: String,
) -> Result<MemoryActionSuccess, String> {
    let body = serde_json::json!({
        "content": content,
        "source_type": "solo_desktop.inbox",
        "source_id": "solo-tray-memory-inbox",
    });
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    let memory_id = parse_memory_remember_response(&text)?;
    Ok(MemoryActionSuccess::Remembered { memory_id })
}

async fn run_daemon_memory_search(
    url: String,
    profile: String,
    query: String,
) -> Result<MemoryActionSuccess, String> {
    let body = serde_json::json!({
        "query": query,
        "limit": 5,
    });
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_memory_search_response(&query, &text)
}

async fn run_daemon_memory_context(
    url: String,
    profile: String,
    query: String,
    subject: Option<String>,
) -> Result<MemoryContextSummary, String> {
    let mut body = serde_json::json!({
        "query": query,
        "limit": 5,
    });
    if let Some(subject) = subject
        && let Some(object) = body.as_object_mut()
    {
        object.insert("subject".to_string(), serde_json::Value::String(subject));
    }
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_memory_context_response(&text)
}

async fn run_daemon_project_decision_add(
    url: String,
    profile: String,
    project: serde_json::Value,
    decision: String,
) -> Result<ProjectDecisionSuccess, String> {
    let body = serde_json::json!({
        "project": project,
        "decision": decision,
    });
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_project_decision_add_response(&text)
}

async fn run_daemon_project_decision_search(
    url: String,
    profile: String,
    project: serde_json::Value,
    display_query: String,
    project_id: String,
) -> Result<ProjectDecisionSuccess, String> {
    let body = serde_json::json!({
        "project": project,
        "query": display_query.as_str(),
        "limit": 25,
    });
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_project_decision_search_response(&display_query, &project_id, &text)
}

async fn run_daemon_project_facts(
    url: String,
    profile: String,
    project: serde_json::Value,
    subject: String,
) -> Result<ProjectFactsSuccess, String> {
    let body = serde_json::json!({
        "project": project,
        "subject": subject.as_str(),
        "limit": 12,
    });
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_project_facts_response(&subject, &text)
}

async fn run_daemon_recent_memories(
    url: String,
    profile: String,
) -> Result<Vec<RecentMemory>, String> {
    let text = get_daemon_json_with_keychain_fallback(&url, &profile).await?;
    parse_recent_memories_response(&text)
}

async fn run_daemon_memory_review(
    url: String,
    profile: String,
    state: Option<String>,
) -> Result<(), String> {
    let body = serde_json::json!({
        "state": state.unwrap_or_else(|| "needs_review".to_string()),
    });
    post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    Ok(())
}

async fn run_daemon_memory_inspect(url: String, profile: String) -> Result<MemoryDetail, String> {
    let text = get_daemon_json_with_keychain_fallback(&url, &profile).await?;
    parse_memory_detail_response(&text)
}

async fn run_daemon_memory_update(
    url: String,
    profile: String,
    content: String,
) -> Result<MemoryUpdateSuccess, String> {
    let body = serde_json::json!({ "content": content });
    let text = patch_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_memory_update_response(&text)
}

async fn run_daemon_memory_forget(
    url: String,
    profile: String,
    memory_id: String,
) -> Result<String, String> {
    delete_daemon_json_with_keychain_fallback(&url, &profile).await?;
    Ok(memory_id)
}

async fn run_daemon_memory_contradictions(
    url: String,
    profile: String,
) -> Result<Vec<MemoryContradiction>, String> {
    let text = get_daemon_json_with_keychain_fallback(&url, &profile).await?;
    parse_memory_contradictions_response(&text)
}

async fn run_daemon_memory_contradiction_resolve(
    url: String,
    profile: String,
    request: ContradictionResolveRequest,
) -> Result<MemoryContradictionResolution, String> {
    let body = serde_json::json!({
        "a_id": request.a_id,
        "b_id": request.b_id,
        "kind": request.kind,
        "status": request.status,
        "resolution_note": request.resolution_note,
        "winning_triple_id": request.winning_triple_id,
    });
    let text = post_daemon_json_with_keychain_fallback(&url, &profile, &body).await?;
    parse_memory_contradiction_resolution_response(&text)
}

struct DaemonJsonResponse {
    status: reqwest::StatusCode,
    text: String,
}

async fn post_daemon_json_with_keychain_fallback(
    url: &str,
    profile: &str,
    body: &serde_json::Value,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let response = post_daemon_json(&client, url, profile, body, None).await?;
    let response = if response.status == reqwest::StatusCode::UNAUTHORIZED {
        let Some(token) = load_bearer_token_from_keychain().await? else {
            return Err("daemon requires bearer auth; no token stored in OS keychain".to_string());
        };
        let retried = post_daemon_json(&client, url, profile, body, Some(token.as_str())).await?;
        if retried.status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(format!(
                "keychain bearer token was rejected: {}",
                summarize_http_body(&retried.text)
            ));
        }
        retried
    } else {
        response
    };

    if !response.status.is_success() {
        return Err(format!(
            "POST {url} returned {}: {}",
            response.status,
            summarize_http_body(&response.text)
        ));
    }
    Ok(response.text)
}

async fn get_daemon_json_with_keychain_fallback(
    url: &str,
    profile: &str,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let response = get_daemon_json(&client, url, profile, None).await?;
    let response = if response.status == reqwest::StatusCode::UNAUTHORIZED {
        let Some(token) = load_bearer_token_from_keychain().await? else {
            return Err("daemon requires bearer auth; no token stored in OS keychain".to_string());
        };
        let retried = get_daemon_json(&client, url, profile, Some(token.as_str())).await?;
        if retried.status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(format!(
                "keychain bearer token was rejected: {}",
                summarize_http_body(&retried.text)
            ));
        }
        retried
    } else {
        response
    };

    if !response.status.is_success() {
        return Err(format!(
            "GET {url} returned {}: {}",
            response.status,
            summarize_http_body(&response.text)
        ));
    }
    Ok(response.text)
}

async fn patch_daemon_json_with_keychain_fallback(
    url: &str,
    profile: &str,
    body: &serde_json::Value,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let response = patch_daemon_json(&client, url, profile, body, None).await?;
    let response = if response.status == reqwest::StatusCode::UNAUTHORIZED {
        let Some(token) = load_bearer_token_from_keychain().await? else {
            return Err("daemon requires bearer auth; no token stored in OS keychain".to_string());
        };
        let retried = patch_daemon_json(&client, url, profile, body, Some(token.as_str())).await?;
        if retried.status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(format!(
                "keychain bearer token was rejected: {}",
                summarize_http_body(&retried.text)
            ));
        }
        retried
    } else {
        response
    };

    if !response.status.is_success() {
        return Err(format!(
            "PATCH {url} returned {}: {}",
            response.status,
            summarize_http_body(&response.text)
        ));
    }
    Ok(response.text)
}

async fn delete_daemon_json_with_keychain_fallback(
    url: &str,
    profile: &str,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let response = delete_daemon_json(&client, url, profile, None).await?;
    let response = if response.status == reqwest::StatusCode::UNAUTHORIZED {
        let Some(token) = load_bearer_token_from_keychain().await? else {
            return Err("daemon requires bearer auth; no token stored in OS keychain".to_string());
        };
        let retried = delete_daemon_json(&client, url, profile, Some(token.as_str())).await?;
        if retried.status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(format!(
                "keychain bearer token was rejected: {}",
                summarize_http_body(&retried.text)
            ));
        }
        retried
    } else {
        response
    };

    if !response.status.is_success() {
        return Err(format!(
            "DELETE {url} returned {}: {}",
            response.status,
            summarize_http_body(&response.text)
        ));
    }
    Ok(response.text)
}

async fn post_daemon_json(
    client: &reqwest::Client,
    url: &str,
    profile: &str,
    body: &serde_json::Value,
    bearer_token: Option<&str>,
) -> Result<DaemonJsonResponse, String> {
    let response = with_profile_and_auth_headers(client.post(url), profile, bearer_token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("read daemon response: {e}"))?;
    Ok(DaemonJsonResponse { status, text })
}

async fn patch_daemon_json(
    client: &reqwest::Client,
    url: &str,
    profile: &str,
    body: &serde_json::Value,
    bearer_token: Option<&str>,
) -> Result<DaemonJsonResponse, String> {
    let response = with_profile_and_auth_headers(client.patch(url), profile, bearer_token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("PATCH {url}: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("read daemon response: {e}"))?;
    Ok(DaemonJsonResponse { status, text })
}

async fn get_daemon_json(
    client: &reqwest::Client,
    url: &str,
    profile: &str,
    bearer_token: Option<&str>,
) -> Result<DaemonJsonResponse, String> {
    let response = with_profile_and_auth_headers(client.get(url), profile, bearer_token)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("read daemon response: {e}"))?;
    Ok(DaemonJsonResponse { status, text })
}

async fn delete_daemon_json(
    client: &reqwest::Client,
    url: &str,
    profile: &str,
    bearer_token: Option<&str>,
) -> Result<DaemonJsonResponse, String> {
    let response = with_profile_and_auth_headers(client.delete(url), profile, bearer_token)
        .send()
        .await
        .map_err(|e| format!("DELETE {url}: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("read daemon response: {e}"))?;
    Ok(DaemonJsonResponse { status, text })
}

fn parse_memory_remember_response(body: &str) -> Result<String, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode remember response JSON: {e}"))?;
    json.get("memory_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "remember response missing memory_id".to_string())
}

fn parse_memory_search_response(query: &str, body: &str) -> Result<MemoryActionSuccess, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode search response JSON: {e}"))?;
    parse_memory_search_response_json(query, &json)
}

fn parse_memory_search_response_json(
    query: &str,
    json: &serde_json::Value,
) -> Result<MemoryActionSuccess, String> {
    let index_len = json
        .get("index_len")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as usize;
    let candidates_considered = json
        .get("candidates_considered")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as usize;
    let hits = json
        .get("hits")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "search response missing hits array".to_string())?
        .iter()
        .map(parse_memory_search_hit)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MemoryActionSuccess::Search {
        query: query.to_string(),
        hits,
        index_len,
        candidates_considered,
    })
}

fn parse_project_decision_add_response(body: &str) -> Result<ProjectDecisionSuccess, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode project decision JSON: {e}"))?;
    let memory_id = json
        .get("memory_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "project decision response missing memory_id".to_string())?;
    Ok(ProjectDecisionSuccess::Added { memory_id })
}

fn parse_project_decision_search_response(
    query: &str,
    project_id: &str,
    body: &str,
) -> Result<ProjectDecisionSuccess, String> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("decode project decision search JSON: {e}"))?;
    let query = json
        .get("query")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(query);
    let project_envelope = project_decision_json_action(&json) == Some("query");
    let hits = if project_envelope {
        json.get("hits")
            .and_then(|value| value.as_array())
            .ok_or_else(|| "project decision JSON missing hits array".to_string())?
            .iter()
            .map(parse_memory_search_hit)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let MemoryActionSuccess::Search { hits, .. } =
            parse_memory_search_response_json(query, &json)?
        else {
            return Err("project decision search returned unexpected remember result".to_string());
        };
        hits
    };
    let hits = if project_envelope {
        hits
    } else {
        hits.into_iter()
            .filter(|hit| project_decision_hit_matches(hit, project_id))
            .collect()
    };
    Ok(ProjectDecisionSuccess::Search {
        query: query.to_string(),
        hits,
    })
}

fn project_decision_json_action(json: &serde_json::Value) -> Option<&str> {
    let command = json.get("command").and_then(|value| value.as_str());
    let action = json.get("action").and_then(|value| value.as_str());
    (command == Some("project decisions"))
        .then_some(action)
        .flatten()
}

fn parse_project_facts_response(subject: &str, body: &str) -> Result<ProjectFactsSuccess, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode project facts JSON: {e}"))?;
    let subject = json
        .get("subject")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(subject);
    let items = match json.as_array() {
        Some(items) => items,
        None => json
            .get("facts")
            .and_then(|value| value.as_array())
            .ok_or_else(|| "project facts response missing facts array".to_string())?,
    };
    let facts = items
        .iter()
        .map(parse_project_fact_hit)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ProjectFactsSuccess {
        subject: subject.to_string(),
        facts,
    })
}

fn parse_project_fact_hit(value: &serde_json::Value) -> Result<ProjectFactHit, String> {
    Ok(ProjectFactHit {
        triple_id: required_json_string(value, "triple_id")?.to_string(),
        subject_id: required_json_string(value, "subject_id")?.to_string(),
        predicate: required_json_string(value, "predicate")?.to_string(),
        object_id: required_json_string(value, "object_id")?.to_string(),
        object_kind: required_json_string(value, "object_kind")?.to_string(),
        valid_from_ms: value
            .get("valid_from_ms")
            .and_then(|field| field.as_i64())
            .ok_or_else(|| "project fact missing valid_from_ms".to_string())?,
        valid_to_ms: value.get("valid_to_ms").and_then(|field| field.as_i64()),
        confidence: value
            .get("confidence")
            .and_then(|field| field.as_f64())
            .ok_or_else(|| "project fact missing confidence".to_string())?
            as f32,
        cluster_id: value
            .get("cluster_id")
            .and_then(|field| field.as_str())
            .map(str::to_string),
    })
}

fn parse_memory_search_hit(hit: &serde_json::Value) -> Result<MemorySearchHit, String> {
    let memory_id = hit
        .get("memory_id")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let content = hit
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let source_type = hit
        .get("source_type")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let tier = hit
        .get("tier")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let fused_score = hit
        .get("fused_score")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as f32;
    let cos_distance = hit
        .get("cos_distance")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as f32;
    Ok(MemorySearchHit {
        memory_id,
        content,
        source_type,
        tier,
        fused_score,
        cos_distance,
    })
}

fn parse_memory_context_response(body: &str) -> Result<MemoryContextSummary, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode memory context JSON: {e}"))?;
    let query = required_json_string(&json, "query")?.to_string();
    let subject = json
        .get("subject")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let resolved_subject = json
        .get("resolved_subject")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let sections = parse_memory_context_sections(
        json.get("sections")
            .ok_or_else(|| "memory context missing sections".to_string())?,
    )?;
    let recall_hits = json
        .pointer("/recall/hits")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "memory context missing recall.hits array".to_string())?
        .iter()
        .map(parse_memory_search_hit)
        .collect::<Result<Vec<_>, _>>()?;
    let facts = json
        .get("facts")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "memory context missing facts array".to_string())?
        .iter()
        .map(parse_project_fact_hit)
        .collect::<Result<Vec<_>, _>>()?;
    let themes = json
        .get("themes")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "memory context missing themes array".to_string())?
        .iter()
        .map(parse_memory_context_theme)
        .collect::<Result<Vec<_>, _>>()?;
    let graph = parse_memory_context_graph(json.get("graph"))?;
    Ok(MemoryContextSummary {
        query,
        subject,
        resolved_subject,
        sections,
        recall_hits,
        facts,
        themes,
        graph,
    })
}

fn parse_memory_context_sections(
    sections: &serde_json::Value,
) -> Result<Vec<MemoryContextSection>, String> {
    let mut out = ["recall", "themes", "entities", "facts", "contradictions"]
        .into_iter()
        .map(|name| parse_memory_context_section(sections, name))
        .collect::<Result<Vec<_>, _>>()?;
    if sections.get("graph").is_some() {
        out.push(parse_memory_context_section(sections, "graph")?);
    } else {
        out.push(MemoryContextSection {
            name: "graph",
            status: "ok".to_string(),
            count: 0,
            warning: None,
        });
    }
    Ok(out)
}

fn parse_memory_context_section(
    sections: &serde_json::Value,
    name: &'static str,
) -> Result<MemoryContextSection, String> {
    let section = sections
        .get(name)
        .ok_or_else(|| format!("memory context missing {name} section"))?;
    Ok(MemoryContextSection {
        name,
        status: required_json_string(section, "status")?.to_string(),
        count: section
            .get("count")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        warning: section
            .get("warning")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })
}

fn parse_memory_context_graph(
    graph: Option<&serde_json::Value>,
) -> Result<MemoryContextGraph, String> {
    let Some(graph) = graph else {
        return Ok(MemoryContextGraph::default());
    };
    let seed_entities = graph
        .get("seed_entities")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let relationship_facts = parse_memory_context_graph_facts(
        graph
            .get("relationship_facts")
            .and_then(|value| value.as_array()),
    )?;
    let literal_facts = parse_memory_context_graph_facts(
        graph
            .get("literal_facts")
            .and_then(|value| value.as_array()),
    )?;
    let review_warnings = graph
        .get("review_warnings")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .map(parse_memory_context_graph_review_warning)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(MemoryContextGraph {
        seed_entities,
        relationship_facts,
        literal_facts,
        review_warnings,
    })
}

fn parse_memory_context_graph_facts(
    facts: Option<&Vec<serde_json::Value>>,
) -> Result<Vec<MemoryContextGraphFact>, String> {
    facts
        .map(|items| {
            items
                .iter()
                .map(parse_memory_context_graph_fact)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn parse_memory_context_graph_fact(
    value: &serde_json::Value,
) -> Result<MemoryContextGraphFact, String> {
    Ok(MemoryContextGraphFact {
        subject_id: required_json_string(value, "subject_id")?.to_string(),
        predicate: required_json_string(value, "predicate")?.to_string(),
        object_id: required_json_string(value, "object_id")?.to_string(),
        object_kind: required_json_string(value, "object_kind")?.to_string(),
        confidence: value
            .get("confidence")
            .and_then(|field| field.as_f64())
            .unwrap_or(0.0) as f32,
        evidence_preview: value
            .get("evidence_preview")
            .and_then(|field| field.as_str())
            .map(str::to_string),
    })
}

fn parse_memory_context_graph_review_warning(
    value: &serde_json::Value,
) -> Result<MemoryContextGraphReviewWarning, String> {
    Ok(MemoryContextGraphReviewWarning {
        reason_code: required_json_string(value, "reason_code")?.to_string(),
        subject_id: required_json_string(value, "subject_id")?.to_string(),
        predicate: required_json_string(value, "predicate")?.to_string(),
        object_id: required_json_string(value, "object_id")?.to_string(),
    })
}

fn parse_memory_context_theme(value: &serde_json::Value) -> Result<MemoryContextTheme, String> {
    Ok(MemoryContextTheme {
        cluster_id: required_json_string(value, "cluster_id")?.to_string(),
        abstraction_text: value
            .get("abstraction_text")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        episode_count: value
            .get("episode_count")
            .and_then(|field| field.as_i64())
            .unwrap_or(0),
        coherence: value
            .get("coherence")
            .and_then(|field| field.as_f64())
            .unwrap_or(0.0) as f32,
        created_at_ms: value
            .get("created_at_ms")
            .and_then(|field| field.as_i64())
            .unwrap_or(0),
    })
}

fn parse_recent_memories_response(body: &str) -> Result<Vec<RecentMemory>, String> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("decode recent memories response JSON: {e}"))?;
    if let Some(items) = json.get("items").and_then(|value| value.as_array()) {
        return items.iter().map(parse_recent_memory_item).collect();
    }
    if let Some(nodes) = json.get("nodes").and_then(|value| value.as_array()) {
        return nodes
            .iter()
            .filter(|node| node.get("kind").and_then(|value| value.as_str()) == Some("episode"))
            .map(parse_recent_memory_node)
            .collect();
    }
    Err("recent memories response missing items or nodes array".to_string())
}

fn parse_recent_memory_item(item: &serde_json::Value) -> Result<RecentMemory, String> {
    let memory_id = item
        .get("memory_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "inbox item missing memory_id".to_string())?
        .to_string();
    let label = item
        .get("label")
        .and_then(|value| value.as_str())
        .unwrap_or("untitled memory")
        .to_string();
    let preview = item
        .get("preview")
        .and_then(|value| value.as_str())
        .unwrap_or(label.as_str())
        .to_string();
    Ok(RecentMemory {
        memory_id,
        label,
        preview,
        ts_ms: item.get("ts_ms").and_then(|value| value.as_i64()),
        source_type: item
            .get("source_type")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        salience: item.get("salience").and_then(|value| value.as_f64()),
        status: item
            .get("status")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        review_state: item
            .get("review_state")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        reviewed_at_ms: item.get("reviewed_at_ms").and_then(|value| value.as_i64()),
        review_note: item
            .get("review_note")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })
}

fn parse_recent_memory_node(node: &serde_json::Value) -> Result<RecentMemory, String> {
    let id = node
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "graph node missing id".to_string())?;
    let memory_id = id.strip_prefix("ep:").unwrap_or(id).to_string();
    let label = node
        .get("label")
        .and_then(|value| value.as_str())
        .unwrap_or("untitled memory")
        .to_string();
    let preview = node
        .get("preview")
        .and_then(|value| value.as_str())
        .unwrap_or(label.as_str())
        .to_string();
    let ts_ms = node.get("ts_ms").and_then(|value| value.as_i64());
    let source_type = node
        .get("source_type")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let salience = node.get("salience").and_then(|value| value.as_f64());
    let status = node
        .get("status")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    Ok(RecentMemory {
        memory_id,
        label,
        preview,
        ts_ms,
        source_type,
        salience,
        status,
        review_state: node
            .get("review_state")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        reviewed_at_ms: node.get("reviewed_at_ms").and_then(|value| value.as_i64()),
        review_note: node
            .get("review_note")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })
}

fn parse_memory_detail_response(body: &str) -> Result<MemoryDetail, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode inspect response JSON: {e}"))?;
    let memory_id = required_json_string(&json, "memory_id")?.to_string();
    let content = required_json_string(&json, "content")?.to_string();
    let source_type = required_json_string(&json, "source_type")?.to_string();
    let source_id = json
        .get("source_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let tier = required_json_string(&json, "tier")?.to_string();
    let status = required_json_string(&json, "status")?.to_string();
    let salience = json
        .get("salience")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let confidence = json
        .get("confidence")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let strength = json
        .get("strength")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let created_at_ms = json.get("created_at_ms").and_then(|value| value.as_i64());
    let updated_at_ms = json.get("updated_at_ms").and_then(|value| value.as_i64());
    Ok(MemoryDetail {
        memory_id,
        content,
        source_type,
        source_id,
        tier,
        status,
        salience,
        confidence,
        strength,
        created_at_ms,
        updated_at_ms,
    })
}

fn required_json_string<'a>(json: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    json.get(field)
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("response missing {field}"))
}

fn parse_memory_update_response(body: &str) -> Result<MemoryUpdateSuccess, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode update response JSON: {e}"))?;
    Ok(MemoryUpdateSuccess {
        memory_id: required_json_string(&json, "memory_id")?.to_string(),
        content: required_json_string(&json, "content")?.to_string(),
        updated_at_ms: json.get("updated_at_ms").and_then(|value| value.as_i64()),
    })
}

fn parse_memory_contradictions_response(body: &str) -> Result<Vec<MemoryContradiction>, String> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("decode contradictions response JSON: {e}"))?;
    let items = json
        .as_array()
        .ok_or_else(|| "contradictions response must be an array".to_string())?;
    items.iter().map(parse_memory_contradiction).collect()
}

fn parse_memory_contradiction(item: &serde_json::Value) -> Result<MemoryContradiction, String> {
    Ok(MemoryContradiction {
        a_id: required_json_string(item, "a_id")?.to_string(),
        b_id: required_json_string(item, "b_id")?.to_string(),
        kind: required_json_string(item, "kind")?.to_string(),
        explanation: required_json_string(item, "explanation")?.to_string(),
        detected_at_ms: item.get("detected_at_ms").and_then(|value| value.as_i64()),
        status: required_json_string(item, "status")?.to_string(),
        resolved_at_ms: item.get("resolved_at_ms").and_then(|value| value.as_i64()),
        resolution_note: item
            .get("resolution_note")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        winning_triple_id: item
            .get("winning_triple_id")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        a_triple: item
            .get("a_triple")
            .and_then(parse_memory_contradiction_triple)
            .transpose()?,
        b_triple: item
            .get("b_triple")
            .and_then(parse_memory_contradiction_triple)
            .transpose()?,
    })
}

fn parse_memory_contradiction_triple(
    item: &serde_json::Value,
) -> Option<Result<MemoryContradictionTriple, String>> {
    if item.is_null() {
        return None;
    }
    Some((|| {
        Ok(MemoryContradictionTriple {
            triple_id: required_json_string(item, "triple_id")?.to_string(),
            subject_id: required_json_string(item, "subject_id")?.to_string(),
            predicate: required_json_string(item, "predicate")?.to_string(),
            object_id: required_json_string(item, "object_id")?.to_string(),
            object_kind: required_json_string(item, "object_kind")?.to_string(),
            valid_from_ms: item.get("valid_from_ms").and_then(|value| value.as_i64()),
            valid_to_ms: item.get("valid_to_ms").and_then(|value| value.as_i64()),
        })
    })())
}

fn parse_memory_contradiction_resolution_response(
    body: &str,
) -> Result<MemoryContradictionResolution, String> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("decode contradiction resolution response JSON: {e}"))?;
    Ok(MemoryContradictionResolution {
        a_id: required_json_string(&json, "a_id")?.to_string(),
        b_id: required_json_string(&json, "b_id")?.to_string(),
        kind: required_json_string(&json, "kind")?.to_string(),
        status: required_json_string(&json, "status")?.to_string(),
        resolved_at_ms: json.get("resolved_at_ms").and_then(|value| value.as_i64()),
        resolution_note: json
            .get("resolution_note")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        winning_triple_id: json
            .get("winning_triple_id")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })
}

fn parse_native_import_response(body: &str) -> Result<NativeImportReport, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode import response JSON: {e}"))?;
    let results = if let Some(items) = json.get("results").and_then(|value| value.as_array()) {
        items
            .iter()
            .map(parse_native_import_result)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok(NativeImportReport {
        path: required_json_string(&json, "path")?.to_string(),
        dry_run: json
            .get("dry_run")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        recursive: json
            .get("recursive")
            .and_then(|value| value.as_bool())
            .unwrap_or(true),
        truncated: json
            .get("truncated")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        total_files: json
            .get("total_files")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        total_bytes: json
            .get("total_bytes")
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
        store_original_file: json
            .get("store_original_file")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        imported: json
            .get("imported")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32,
        deduped: json
            .get("deduped")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32,
        failed: json
            .get("failed")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32,
        chunks_persisted: json
            .get("chunks_persisted")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32,
        assets_retained: json
            .get("assets_retained")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32,
        assets_deduped: json
            .get("assets_deduped")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32,
        asset_links: json
            .get("asset_links")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32,
        asset_failed: json
            .get("asset_failed")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32,
        results,
    })
}

fn parse_native_import_result(value: &serde_json::Value) -> Result<NativeImportResult, String> {
    Ok(NativeImportResult {
        path: required_json_string(value, "path")?.to_string(),
        bytes: value
            .get("bytes")
            .and_then(|field| field.as_u64())
            .unwrap_or(0),
        doc_id: value
            .get("doc_id")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        chunks_persisted: value
            .get("chunks_persisted")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
        bytes_ingested: value
            .get("bytes_ingested")
            .and_then(|field| field.as_u64())
            .unwrap_or(0),
        deduped: value
            .get("deduped")
            .and_then(|field| field.as_bool())
            .unwrap_or(false),
        asset_id: value
            .pointer("/asset/asset_id")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        asset_error: value
            .get("asset_error")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        error: value
            .get("error")
            .and_then(|field| field.as_str())
            .map(str::to_string),
    })
}

fn parse_document_list_response(body: &str) -> Result<Vec<DocumentSummary>, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode documents response JSON: {e}"))?;
    let items = json
        .as_array()
        .ok_or_else(|| "documents response must be an array".to_string())?;
    items.iter().map(parse_document_summary).collect()
}

fn parse_document_summary(value: &serde_json::Value) -> Result<DocumentSummary, String> {
    Ok(DocumentSummary {
        doc_id: required_json_string(value, "doc_id")?.to_string(),
        title: value
            .get("title")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        source: value
            .get("source")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        mime_type: value
            .get("mime_type")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        ingested_at_ms: value.get("ingested_at_ms").and_then(|field| field.as_i64()),
        chunk_count: value
            .get("chunk_count")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
        status: required_json_string(value, "status")?.to_string(),
    })
}

fn parse_document_search_response(
    query: &str,
    body: &str,
) -> Result<DocumentSearchSuccess, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode document search JSON: {e}"))?;
    let items = json
        .as_array()
        .ok_or_else(|| "document search response must be an array".to_string())?;
    let hits = items
        .iter()
        .map(parse_document_search_hit)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DocumentSearchSuccess {
        query: query.to_string(),
        hits,
    })
}

fn parse_document_search_hit(value: &serde_json::Value) -> Result<DocumentSearchHit, String> {
    Ok(DocumentSearchHit {
        chunk_id: required_json_string(value, "chunk_id")?.to_string(),
        doc_id: required_json_string(value, "doc_id")?.to_string(),
        doc_title: value
            .get("doc_title")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        doc_source: value
            .get("doc_source")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        doc_mime_type: value
            .get("doc_mime_type")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        chunk_index: value
            .get("chunk_index")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
        content: required_json_string(value, "content")?.to_string(),
        cos_distance: value
            .get("cos_distance")
            .and_then(|field| field.as_f64())
            .unwrap_or(0.0) as f32,
        start_offset: value
            .get("start_offset")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
        end_offset: value
            .get("end_offset")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
    })
}

fn parse_document_detail_response(body: &str) -> Result<DocumentDetail, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode document detail JSON: {e}"))?;
    let document = json
        .get("document")
        .ok_or_else(|| "document detail response missing document".to_string())?;
    let chunks = json
        .get("chunks")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "document detail response missing chunks array".to_string())?
        .iter()
        .map(parse_document_chunk_summary)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DocumentDetail {
        doc_id: required_json_string(document, "doc_id")?.to_string(),
        title: document
            .get("title")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        source: document
            .get("source")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        mime_type: document
            .get("mime_type")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        ingested_at_ms: document
            .get("ingested_at_ms")
            .and_then(|field| field.as_i64()),
        modified_at_ms: document
            .get("modified_at_ms")
            .and_then(|field| field.as_i64()),
        status: required_json_string(document, "status")?.to_string(),
        chunk_count: document
            .get("chunk_count")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
        content_hash: document
            .get("content_hash")
            .and_then(|field| field.as_str())
            .map(str::to_string),
        byte_size: document.get("byte_size").and_then(|field| field.as_u64()),
        chunks,
    })
}

fn parse_document_chunk_summary(value: &serde_json::Value) -> Result<DocumentChunkSummary, String> {
    Ok(DocumentChunkSummary {
        chunk_id: required_json_string(value, "chunk_id")?.to_string(),
        chunk_index: value
            .get("chunk_index")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
        content_preview: required_json_string(value, "content_preview")?.to_string(),
        token_count: value
            .get("token_count")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
    })
}

fn parse_document_forget_response(body: &str) -> Result<DocumentForgetReport, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("decode document forget JSON: {e}"))?;
    Ok(DocumentForgetReport {
        doc_id: required_json_string(&json, "doc_id")?.to_string(),
        chunks_tombstoned: json
            .get("chunks_tombstoned")
            .and_then(|field| field.as_u64())
            .unwrap_or(0) as u32,
    })
}

async fn run_mcp_probe_with_token(
    url: &str,
    profile: &str,
    bearer_token: Option<&str>,
) -> Result<McpProbeSuccess, McpProbeError> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| McpProbeError::Other(format!("build HTTP client: {e}")))?;

    let initialize_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "solo-tray",
                "version": solo_core::build_info::version_with_build_metadata(),
            },
        },
    });
    let initialize_response = with_mcp_probe_headers(client.post(url), profile, bearer_token)
        .json(&initialize_body)
        .send()
        .await
        .map_err(|e| McpProbeError::Other(format!("POST initialize {url}: {e}")))?;
    let initialize_status = initialize_response.status();
    let session_id = initialize_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|header| header.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let initialize_body = initialize_response
        .text()
        .await
        .map_err(|e| McpProbeError::Other(format!("read initialize response: {e}")))?;
    if !initialize_status.is_success() {
        if !session_id.is_empty() {
            delete_mcp_probe_session(&client, url, &session_id, profile, bearer_token).await;
        }
        let message = format!(
            "initialize returned {initialize_status}: {}",
            summarize_http_body(&initialize_body)
        );
        return if initialize_status == reqwest::StatusCode::UNAUTHORIZED {
            Err(McpProbeError::Unauthorized(message))
        } else {
            Err(McpProbeError::Other(message))
        };
    }
    if session_id.is_empty() {
        return Err(McpProbeError::Other(
            "initialize response missing `mcp-session-id` header".to_string(),
        ));
    }

    let initialize_json: serde_json::Value = match serde_json::from_str(&initialize_body) {
        Ok(value) => value,
        Err(e) => {
            delete_mcp_probe_session(&client, url, &session_id, profile, bearer_token).await;
            return Err(McpProbeError::Other(format!(
                "decode initialize response JSON: {e}"
            )));
        }
    };
    if let Some(message) = json_rpc_error_message(&initialize_json) {
        delete_mcp_probe_session(&client, url, &session_id, profile, bearer_token).await;
        return Err(McpProbeError::Other(format!(
            "initialize JSON-RPC error: {message}"
        )));
    }
    let (server_name, server_version, protocol_version) =
        match parse_mcp_initialize_summary(&initialize_json) {
            Ok(summary) => summary,
            Err(message) => {
                delete_mcp_probe_session(&client, url, &session_id, profile, bearer_token).await;
                return Err(McpProbeError::Other(message));
            }
        };

    let tool_count =
        run_mcp_probe_tool_list(&client, url, &session_id, profile, bearer_token).await;
    delete_mcp_probe_session(&client, url, &session_id, profile, bearer_token).await;
    let tool_count = tool_count?;
    Ok(McpProbeSuccess {
        profile: profile.to_string(),
        server_name,
        server_version,
        protocol_version,
        tool_count,
        session_id,
        used_bearer_token: bearer_token.is_some(),
    })
}

async fn run_mcp_probe_tool_list(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    profile: &str,
    bearer_token: Option<&str>,
) -> Result<usize, McpProbeError> {
    let initialized_response = with_mcp_probe_headers(
        client.post(url).header(MCP_SESSION_ID_HEADER, session_id),
        profile,
        bearer_token,
    )
    .json(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    }))
    .send()
    .await
    .map_err(|e| McpProbeError::Other(format!("POST initialized notification {url}: {e}")))?;
    let initialized_status = initialized_response.status();
    let initialized_body = initialized_response.text().await.map_err(|e| {
        McpProbeError::Other(format!("read initialized notification response: {e}"))
    })?;
    if !initialized_status.is_success() {
        let message = format!(
            "initialized notification returned {initialized_status}: {}",
            summarize_http_body(&initialized_body)
        );
        return if initialized_status == reqwest::StatusCode::UNAUTHORIZED {
            Err(McpProbeError::Unauthorized(message))
        } else {
            Err(McpProbeError::Other(message))
        };
    }

    let tools_response = with_mcp_probe_headers(
        client.post(url).header(MCP_SESSION_ID_HEADER, session_id),
        profile,
        bearer_token,
    )
    .json(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {},
    }))
    .send()
    .await
    .map_err(|e| McpProbeError::Other(format!("POST tools/list {url}: {e}")))?;
    let tools_status = tools_response.status();
    let tools_body = tools_response
        .text()
        .await
        .map_err(|e| McpProbeError::Other(format!("read tools/list response: {e}")))?;
    if !tools_status.is_success() {
        let message = format!(
            "tools/list returned {tools_status}: {}",
            summarize_http_body(&tools_body)
        );
        return if tools_status == reqwest::StatusCode::UNAUTHORIZED {
            Err(McpProbeError::Unauthorized(message))
        } else {
            Err(McpProbeError::Other(message))
        };
    }

    let tools_json: serde_json::Value = serde_json::from_str(&tools_body)
        .map_err(|e| McpProbeError::Other(format!("decode tools/list JSON: {e}")))?;
    if let Some(message) = json_rpc_error_message(&tools_json) {
        return Err(McpProbeError::Other(format!(
            "tools/list JSON-RPC error: {message}"
        )));
    }
    parse_mcp_tools_count(&tools_json).map_err(McpProbeError::Other)
}

fn with_mcp_probe_headers(
    request: reqwest::RequestBuilder,
    profile: &str,
    bearer_token: Option<&str>,
) -> reqwest::RequestBuilder {
    with_profile_and_auth_headers(request, profile, bearer_token)
}

fn with_profile_and_auth_headers(
    request: reqwest::RequestBuilder,
    profile: &str,
    bearer_token: Option<&str>,
) -> reqwest::RequestBuilder {
    let _ = profile;
    if let Some(token) = bearer_token {
        request.bearer_auth(token)
    } else {
        request
    }
}

fn parse_mcp_initialize_summary(
    json: &serde_json::Value,
) -> Result<(String, String, String), String> {
    let server_name = json
        .pointer("/result/serverInfo/name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "initialize response missing result.serverInfo.name".to_string())?
        .to_string();
    let server_version = json
        .pointer("/result/serverInfo/version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "initialize response missing result.serverInfo.version".to_string())?
        .to_string();
    let protocol_version = json
        .pointer("/result/protocolVersion")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "initialize response missing result.protocolVersion".to_string())?
        .to_string();
    Ok((server_name, server_version, protocol_version))
}

fn parse_mcp_tools_count(json: &serde_json::Value) -> Result<usize, String> {
    json.pointer("/result/tools")
        .and_then(|value| value.as_array())
        .map(Vec::len)
        .ok_or_else(|| "tools/list response missing result.tools array".to_string())
}

fn json_rpc_error_message(json: &serde_json::Value) -> Option<String> {
    let error = json.get("error")?;
    if let Some(message) = error.get("message").and_then(|value| value.as_str()) {
        return Some(message.to_string());
    }
    Some(error.to_string())
}

async fn delete_mcp_probe_session(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    profile: &str,
    bearer_token: Option<&str>,
) {
    match with_mcp_probe_headers(
        client.delete(url).header(MCP_SESSION_ID_HEADER, session_id),
        profile,
        bearer_token,
    )
    .send()
    .await
    {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            tracing::debug!(
                status = %response.status(),
                "MCP probe session cleanup returned non-success status"
            );
        }
        Err(error) => {
            tracing::debug!(error = %error, "MCP probe session cleanup failed");
        }
    }
}

async fn run_daemon_backup(
    url: String,
    backup_dir: PathBuf,
    dest: PathBuf,
) -> Result<BackupActionSuccess, String> {
    tokio::fs::create_dir_all(&backup_dir)
        .await
        .map_err(|e| format!("create backup folder {}: {e}", backup_dir.display()))?;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;
    let dest_text = display_path(&dest);
    let response = client
        .post(&url)
        .json(&serde_json::json!({
            "to": dest_text,
            "force": false,
        }))
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("read backup response: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "POST {url} returned {status}: {}",
            summarize_http_body(&body)
        ));
    }

    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode backup response JSON: {e}"))?;
    let path = json
        .get("path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or(dest);
    let elapsed_ms = json.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0);

    Ok(BackupActionSuccess { path, elapsed_ms })
}

fn summarize_http_body(body: &str) -> String {
    const LIMIT: usize = 240;
    let trimmed = body.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let mut summary: String = trimmed.chars().take(LIMIT).collect();
    summary.push_str("...");
    summary
}

fn format_age(at: std::time::SystemTime) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(at)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 60 * 60 {
        format!("{}m ago", secs / 60)
    } else if secs < 60 * 60 * 24 {
        format!("{}h ago", secs / (60 * 60))
    } else {
        format!("{}d ago", secs / (60 * 60 * 24))
    }
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn system_time_from_unix_ms(ms: i64) -> Option<std::time::SystemTime> {
    if ms < 0 {
        return None;
    }
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64))
}

fn draw_health_row(ui: &mut egui::Ui, label: &str, value: impl std::fmt::Display) {
    ui.label(RichText::new(label).strong());
    ui.label(value.to_string());
    ui.end_row();
}

fn health_state_text(health: DaemonHealth) -> &'static str {
    match health {
        DaemonHealth::Healthy => "healthy",
        DaemonHealth::Starting => "starting",
        DaemonHealth::Down => "down",
    }
}

fn status_payload_bool(payload: Option<&serde_json::Value>, pointer: &str) -> Option<bool> {
    payload.and_then(|json| json.pointer(pointer)?.as_bool())
}

fn status_payload_opt_string(payload: Option<&serde_json::Value>, pointer: &str) -> Option<String> {
    let value = payload.and_then(|json| json.pointer(pointer))?;
    match value {
        serde_json::Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        serde_json::Value::String(_) | serde_json::Value::Null => None,
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => Some(value.to_string()),
    }
}

fn status_payload_string(payload: Option<&serde_json::Value>, pointer: &str) -> String {
    let Some(value) = payload.and_then(|json| json.pointer(pointer)) else {
        return "not reported".to_string();
    };
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Null => "null".to_string(),
        _ => value.to_string(),
    }
}

fn status_embedder_summary(payload: Option<&serde_json::Value>) -> String {
    let Some(embedder) = payload.and_then(|json| json.get("embedder")) else {
        return "not reported".to_string();
    };
    let name = embedder
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let version = embedder
        .get("version")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let dim = embedder
        .get("dim")
        .and_then(|value| value.as_u64())
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".to_string());
    let dtype = embedder
        .get("dtype")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    format!("{name} {version}, dim {dim}, {dtype}")
}

fn status_steward_summary(payload: Option<&serde_json::Value>) -> String {
    let Some(json) = payload else {
        return "not reported".to_string();
    };
    for pointer in [
        "/steward/model",
        "/steward/name",
        "/llm/model",
        "/llm/name",
        "/model",
    ] {
        let value = status_payload_string(Some(json), pointer);
        if value != "not reported" {
            return value;
        }
    }
    "not reported by /v1/status".to_string()
}

/// Render the parsed /v1/status JSON as a friendly table rather than
/// a raw JSON dump. Falls back to a code-editor of the raw JSON if the
/// shape doesn't match what /v1/status normally returns.
fn render_status_summary(
    ui: &mut egui::Ui,
    json: &serde_json::Value,
    last_ok_at: Option<std::time::SystemTime>,
    last_error: &Option<String>,
) {
    let ok = json.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let version = json.get("version").and_then(|v| v.as_str()).unwrap_or("?");
    let library_name = json
        .pointer("/library/name")
        .and_then(|v| v.as_str())
        .unwrap_or("Community Memory Library");
    let mcp_sessions = json
        .pointer("/mcp/sessions")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    egui::Grid::new("status_summary")
        .num_columns(2)
        .spacing([20.0, 6.0])
        .show(ui, |ui| {
            ui.label(RichText::new("Status").strong());
            ui.label(if ok {
                RichText::new("● healthy").color(egui::Color32::from_rgb(40, 180, 80))
            } else {
                RichText::new("● degraded").color(egui::Color32::from_rgb(200, 160, 30))
            });
            ui.end_row();

            ui.label(RichText::new("Version").strong());
            ui.label(version);
            ui.end_row();

            ui.label(RichText::new("Memory library").strong());
            ui.label(library_name);
            ui.end_row();

            ui.label(RichText::new("MCP sessions").strong());
            ui.label(mcp_sessions.to_string());
            ui.end_row();

            if let Some(embedder) = json.get("embedder") {
                let name = embedder.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let dim = embedder.get("dim").and_then(|v| v.as_u64()).unwrap_or(0);
                ui.label(RichText::new("Embedder").strong());
                ui.label(format!("{name} (dim {dim})"));
                ui.end_row();

                let (state, tone, detail) = embedder_runtime_status(
                    Some(json),
                    if ok {
                        DaemonHealth::Healthy
                    } else {
                        DaemonHealth::Starting
                    },
                    last_error.as_deref(),
                );
                ui.label(RichText::new("Embedder runtime").strong());
                ui.label(state_text(
                    &format!("{state}: {detail}"),
                    tone,
                    ui.visuals().dark_mode,
                ));
                ui.end_row();
            }

            if json.get("steward").is_some() {
                let (state, tone, detail) = steward_runtime_status(
                    Some(json),
                    if ok {
                        DaemonHealth::Healthy
                    } else {
                        DaemonHealth::Starting
                    },
                );
                ui.label(RichText::new("Steward runtime").strong());
                ui.label(state_text(
                    &format!("{state}: {detail}"),
                    tone,
                    ui.visuals().dark_mode,
                ));
                ui.end_row();
            }

            if let Some(at) = last_ok_at {
                ui.label(RichText::new("Last successful poll").strong());
                let secs_ago = std::time::SystemTime::now()
                    .duration_since(at)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                ui.label(format!("{secs_ago}s ago"));
                ui.end_row();
            }
        });

    if let Some(err) = last_error {
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!("Last poll error: {err}"))
                .color(egui::Color32::from_rgb(220, 80, 80)),
        );
    }

    ui.add_space(12.0);
    ui.collapsing("Raw /v1/status JSON", |ui| {
        let pretty = serde_json::to_string_pretty(json)
            .unwrap_or_else(|e| format!("(JSON serialise error: {e})"));
        ui.code_editor(&mut pretty.as_str().to_string());
    });
}

/// Friendly bytes formatter — KiB/MiB/GiB. No external dep needed.
fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

impl App for SoloTrayApp {
    fn update(&mut self, ctx: &Context, frame: &mut Frame) {
        let _ = frame; // unused; viewport commands go through ctx

        // Drive GTK from the eframe loop on Linux. The tray is GTK-backed
        // there and needs a GTK event loop on the thread that owns it; winit
        // provides its own loop and never iterates GTK. The repaint pump
        // guarantees this runs at ≈4 Hz even when the viewport is hidden.
        // Bounded so a busy GTK queue cannot stall a frame indefinitely.
        // See docs/adr/0016-linux-gtk-initialization.md.
        #[cfg(target_os = "linux")]
        {
            for _ in 0..MAX_GTK_ITERATIONS_PER_FRAME {
                if !gtk::events_pending() {
                    break;
                }
                // `false` = non-blocking: return immediately when the queue
                // drains rather than parking until the next GTK event.
                gtk::main_iteration_do(false);
            }
        }

        self.sync_supervisor_state();

        // Heartbeat: every ~10 seconds at the 4 Hz repaint cadence,
        // confirm `update()` is still firing. Lets us distinguish
        // "menu did nothing because update() stopped running" from
        // "menu did nothing because the channel was empty". Logged
        // at info so it shows up under the default RUST_LOG.
        self.update_ticks = self.update_ticks.wrapping_add(1);
        if self.update_ticks % 40 == 1 {
            tracing::info!(
                tick = self.update_ticks,
                window_visible = self.window_visible,
                "update() heartbeat"
            );
        }

        // Handle pending tray menu events. We do this once per frame;
        // egui's update() is called whenever there's input or a repaint
        // request, which is plenty fast.
        let events = tray::drain_menu_events();
        if !events.is_empty() {
            tracing::debug!(count = events.len(), "drained tray menu events");
        }
        for ev in events {
            let id_string = ev.id.0.to_string();
            tracing::info!(menu_id = %id_string, "tray menu event");
            self.handle_menu_event(ctx, &id_string);
        }

        // Refresh the tray icon. On health transition, swap tooltip
        // + icon. While in the Starting state, redraw every frame so
        // the pulse animation appears (capped at the 4Hz repaint
        // cadence below).
        let health = self.current_health();
        let pulse = pulse_factor(self.started_at, health);
        if health != self.last_health {
            if let Some(t) = self.tray.as_ref() {
                let _ = t.set_icon(Some(tray::icon_for(health, pulse)));
                let tooltip = match health {
                    DaemonHealth::Healthy => "Solo daemon: healthy",
                    DaemonHealth::Starting => "Solo daemon: starting / reconnecting",
                    DaemonHealth::Down => "Solo daemon: stopped",
                };
                let _ = t.set_tooltip(Some(tooltip));
            }
            self.last_health = health;
        } else if health == DaemonHealth::Starting {
            // Animate the amber icon while we wait for first /v1/status
            // success. set_icon is cheap (16x16 RGBA = 1KB upload).
            if let Some(t) = self.tray.as_ref() {
                let _ = t.set_icon(Some(tray::icon_for(health, pulse)));
            }
        }

        // If the user closed the viewport via X, MINIMISE rather
        // than quit. Quit is reserved for the tray-menu Quit item.
        // We deliberately don't hide via `Visible(false)` because
        // winit's `request_redraw` is a no-op for invisible windows
        // on Windows — once hidden, `update()` stops firing and the
        // tray-menu channel can never be drained, so every tray-
        // menu click silently vanishes. Minimised windows still
        // generate paint events on restore, so the event loop stays
        // responsive enough that Show-logs can bring the window
        // back. The cost is a small taskbar entry while minimised.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            self.window_visible = false;
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
        }

        // Repaint at 4 Hz so the log window stays "live" without
        // burning CPU. Faster than that doesn't help; slower feels
        // janky.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        self.poll_first_run_init_result();
        self.poll_setup_result();
        self.poll_mcp_probe_result();
        self.poll_client_check_result();
        self.poll_setup_doctor_result();
        self.poll_backup_result();
        self.poll_ollama_migration_result();
        self.poll_project_result();
        self.poll_project_docs_import_result();
        self.poll_project_decision_result();
        self.poll_project_facts_result();
        self.poll_import_result();
        self.poll_import_commit_result();
        self.poll_document_list_result();
        self.poll_document_search_result();
        self.poll_document_detail_result();
        self.poll_document_forget_result();
        self.poll_memory_result();
        self.poll_memory_context_result();
        self.poll_memory_recent_result();
        self.poll_memory_detail_result();
        self.poll_memory_update_result();
        self.poll_memory_forget_result();
        self.poll_memory_contradictions_result();
        self.poll_memory_contradiction_resolve_result();
        self.refresh_detected_info_if_needed();
        self.draw_main_window(ctx);
    }
}

impl Drop for SoloTrayApp {
    fn drop(&mut self) {
        tray::stop_launched_helpers();
        self.passphrase_input.zeroize();
        self.init_passphrase_confirm.zeroize();
        self.keychain_passphrase_input.zeroize();
        self.bearer_token_input.zeroize();
        self.ollama_migration_passphrase.zeroize();
        self.pending_keychain_passphrase.take();
        self.ollama_migration_restart_passphrase.take();
    }
}

fn level_color(level: Level, dark_mode: bool) -> egui::Color32 {
    match level {
        Level::Error => error_color(dark_mode),
        Level::Warn => warning_color(dark_mode),
        Level::Info => {
            if dark_mode {
                egui::Color32::from_rgb(218, 223, 226)
            } else {
                egui::Color32::from_rgb(38, 45, 50)
            }
        }
        Level::Debug => {
            if dark_mode {
                egui::Color32::from_rgb(142, 184, 232)
            } else {
                egui::Color32::from_rgb(38, 94, 150)
            }
        }
        Level::Trace => muted_text_color(dark_mode),
    }
}

fn daemon_log_visible_lines(buf: &RingBuffer, min_level: Level) -> Vec<(Level, String)> {
    buf.iter_filtered(min_level)
        .map(|line| (line.level, line.text.clone()))
        .collect()
}

fn daemon_log_status(buf: &RingBuffer) -> String {
    format!(
        "daemon stderr: retained {} / seen {} / dropped {}",
        buf.len(),
        buf.seen,
        buf.dropped
    )
}

fn tray_log_visible_lines(lines: &[String], min_level: Level) -> Vec<(Level, String)> {
    lines
        .iter()
        .map(|line| (Level::infer(line), line.clone()))
        .filter(|(level, _)| level.severity() >= min_level.severity())
        .collect()
}

fn format_log_copy(lines: &[(Level, String)], max_lines: usize) -> String {
    if max_lines == 0 || lines.is_empty() {
        return String::new();
    }
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn health_label(health: DaemonHealth, dark_mode: bool) -> RichText {
    match health {
        DaemonHealth::Healthy => RichText::new("daemon healthy")
            .color(success_color(dark_mode))
            .strong(),
        DaemonHealth::Starting => RichText::new("daemon starting")
            .color(warning_color(dark_mode))
            .strong(),
        DaemonHealth::Down => RichText::new("daemon down")
            .color(error_color(dark_mode))
            .strong(),
    }
}

/// Apply a Settings::Theme to the egui visuals.
///
/// Light and dark both get Solo-specific styling, and `System` keeps
/// eframe's OS theme detection while still using those tuned palettes.
fn apply_theme(ctx: &Context, theme: Theme) {
    ctx.style_mut_of(egui::Theme::Dark, |style| apply_solo_style(style, true));
    ctx.style_mut_of(egui::Theme::Light, |style| {
        apply_solo_style(style, false);
    });

    let preference = match theme {
        Theme::Dark => egui::ThemePreference::Dark,
        Theme::Light => egui::ThemePreference::Light,
        Theme::System => egui::ThemePreference::System,
    };
    ctx.set_theme(preference);
}

fn apply_solo_style(style: &mut egui::Style, dark_mode: bool) {
    style.visuals = solo_visuals(dark_mode);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size = egui::vec2(40.0, 24.0);
    style.spacing.window_margin = egui::Margin::symmetric(12, 10);
}

fn solo_visuals(dark_mode: bool) -> egui::Visuals {
    let mut visuals = if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let radius = egui::CornerRadius::same(6);

    visuals.panel_fill = content_fill(dark_mode);
    visuals.window_fill = if dark_mode {
        egui::Color32::from_rgb(29, 32, 36)
    } else {
        egui::Color32::from_rgb(255, 255, 255)
    };
    visuals.window_stroke = egui::Stroke::new(1.0_f32, border_color(dark_mode));
    visuals.window_corner_radius = egui::CornerRadius::same(8);
    visuals.menu_corner_radius = radius;
    visuals.faint_bg_color = if dark_mode {
        egui::Color32::from_rgb(34, 38, 42)
    } else {
        egui::Color32::from_rgb(236, 240, 242)
    };
    visuals.extreme_bg_color = if dark_mode {
        egui::Color32::from_rgb(13, 15, 17)
    } else {
        egui::Color32::from_rgb(255, 255, 255)
    };
    visuals.code_bg_color = if dark_mode {
        egui::Color32::from_rgb(34, 38, 42)
    } else {
        egui::Color32::from_rgb(233, 238, 240)
    };
    visuals.hyperlink_color = if dark_mode {
        egui::Color32::from_rgb(111, 185, 240)
    } else {
        egui::Color32::from_rgb(25, 104, 174)
    };
    visuals.warn_fg_color = warning_color(dark_mode);
    visuals.error_fg_color = error_color(dark_mode);
    visuals.selection.bg_fill = selected_sidebar_fill(dark_mode);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, accent_color(dark_mode));

    visuals.widgets.noninteractive.corner_radius = radius;
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.active.corner_radius = radius;
    visuals.widgets.open.corner_radius = radius;

    visuals.widgets.noninteractive.bg_fill = content_fill(dark_mode);
    visuals.widgets.noninteractive.weak_bg_fill = content_fill(dark_mode);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, border_color(dark_mode));

    visuals.widgets.inactive.weak_bg_fill = if dark_mode {
        egui::Color32::from_rgb(42, 47, 52)
    } else {
        egui::Color32::from_rgb(229, 235, 238)
    };
    visuals.widgets.inactive.bg_fill = visuals.widgets.inactive.weak_bg_fill;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, border_color(dark_mode));

    visuals.widgets.hovered.weak_bg_fill = if dark_mode {
        egui::Color32::from_rgb(51, 58, 64)
    } else {
        egui::Color32::from_rgb(219, 228, 231)
    };
    visuals.widgets.hovered.bg_fill = visuals.widgets.hovered.weak_bg_fill;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, accent_color(dark_mode));

    visuals.widgets.active.weak_bg_fill = selected_sidebar_fill(dark_mode);
    visuals.widgets.active.bg_fill = selected_sidebar_fill(dark_mode);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, accent_color(dark_mode));
    visuals.widgets.active.fg_stroke =
        egui::Stroke::new(1.0_f32, selected_sidebar_text_color(dark_mode));

    visuals
}

fn accent_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(84, 196, 172)
    } else {
        egui::Color32::from_rgb(0, 119, 101)
    }
}

fn content_fill(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(24, 27, 30)
    } else {
        egui::Color32::from_rgb(247, 249, 250)
    }
}

fn selected_sidebar_fill(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(31, 71, 66)
    } else {
        egui::Color32::from_rgb(204, 232, 226)
    }
}

fn selected_sidebar_text_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(232, 255, 249)
    } else {
        egui::Color32::from_rgb(14, 54, 48)
    }
}

fn muted_text_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(150, 160, 166)
    } else {
        egui::Color32::from_rgb(98, 108, 114)
    }
}

fn border_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(48, 54, 59)
    } else {
        egui::Color32::from_rgb(205, 214, 218)
    }
}

fn success_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(91, 205, 125)
    } else {
        egui::Color32::from_rgb(24, 132, 67)
    }
}

fn warning_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(233, 180, 82)
    } else {
        egui::Color32::from_rgb(150, 96, 18)
    }
}

fn error_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(239, 112, 102)
    } else {
        egui::Color32::from_rgb(188, 48, 39)
    }
}

/// Compute the pulse brightness for the tray icon. Returns 1.0 for
/// healthy/down states (no pulse); for the Starting state, returns a
/// sine-modulated factor in [0.55, 1.0] with a 2-second period.
fn pulse_factor(started_at: std::time::Instant, health: DaemonHealth) -> f32 {
    if health != DaemonHealth::Starting {
        return 1.0;
    }
    let elapsed = started_at.elapsed().as_secs_f32();
    // 0.5 Hz sine; remap from [-1, 1] to [0.55, 1.0] so the dimmest
    // frame still reads as "amber circle" not "near-invisible".
    let s = (elapsed * std::f32::consts::PI).sin();
    0.775 + s * 0.225
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env_lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let map: std::collections::BTreeMap<String, std::ffi::OsString> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), std::ffi::OsString::from(value)))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn write_minimal_solo_config(path: &Path) {
        std::fs::write(
            path,
            r#"
schema_version = 1
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "stub"
version = "v1"
dim = 8
dtype = "f32"
"#,
        )
        .expect("write solo config");
    }

    fn write_first_run_alias_config(path: &Path) {
        let cfg = SoloConfig::new([7; 16], solo_storage::default_embedder());
        cfg.write(path).expect("write solo config");
    }

    #[test]
    fn first_run_user_alias_blank_input_leaves_aliases_empty() {
        let temp = tempfile::tempdir().expect("create temp data dir");
        let config_path = temp.path().join("solo.config.toml");
        write_first_run_alias_config(&config_path);

        let wrote_alias = apply_first_run_user_alias(&config_path, "  \n  ").expect("apply alias");

        assert!(!wrote_alias);
        let cfg = SoloConfig::read(&config_path).expect("read config");
        assert!(cfg.identity.user_aliases.is_empty());
    }

    #[test]
    fn first_run_user_alias_writes_lowercase_name() {
        let temp = tempfile::tempdir().expect("create temp data dir");
        let config_path = temp.path().join("solo.config.toml");
        write_first_run_alias_config(&config_path);

        let wrote_alias =
            apply_first_run_user_alias(&config_path, "  Alex  ").expect("apply alias");

        assert!(wrote_alias);
        let cfg = SoloConfig::read(&config_path).expect("read config");
        assert_eq!(cfg.identity.user_aliases, vec!["alex".to_string()]);
    }

    fn successful_mcp_probe(profile: &str) -> McpProbeState {
        McpProbeState::Succeeded {
            summary: McpProbeSuccess {
                profile: profile.to_string(),
                server_name: "solo".to_string(),
                server_version: "0.0.0".to_string(),
                protocol_version: "2025-03-26".to_string(),
                tool_count: 18,
                session_id: "12345678-1234-1234-1234-123456789abc".to_string(),
                used_bearer_token: false,
            },
            completed_at: std::time::UNIX_EPOCH,
        }
    }

    fn native_import_report(imported: u32) -> NativeImportReport {
        NativeImportReport {
            path: "/tmp/import".to_string(),
            dry_run: false,
            recursive: true,
            truncated: false,
            total_files: imported as usize,
            total_bytes: 128,
            store_original_file: false,
            imported,
            deduped: 0,
            failed: 0,
            chunks_persisted: imported,
            assets_retained: 0,
            assets_deduped: 0,
            asset_links: 0,
            asset_failed: 0,
            results: Vec::new(),
        }
    }

    #[test]
    fn log_source_labels_are_clear_for_support_ui() {
        assert_eq!(LogSource::Daemon.label(), "Daemon stderr");
        assert_eq!(LogSource::Tray.label(), "Tray log");
        assert!(
            LogSource::Daemon
                .description()
                .contains("captured in memory")
        );
        assert!(
            LogSource::Tray
                .description()
                .contains("Solo data directory")
        );
    }

    #[test]
    fn format_log_copy_returns_bounded_tail_in_order() {
        let lines = vec![
            (Level::Info, "one".to_string()),
            (Level::Warn, "two".to_string()),
            (Level::Error, "three".to_string()),
        ];
        assert_eq!(format_log_copy(&lines, 2), "two\nthree");
        assert_eq!(format_log_copy(&lines, 0), "");
    }

    #[test]
    fn tray_log_visible_lines_respects_level_filter() {
        let lines = vec![
            "INFO starting".to_string(),
            "WARN degraded".to_string(),
            "ERROR failed".to_string(),
        ];
        let visible = tray_log_visible_lines(&lines, Level::Warn);
        assert_eq!(
            visible,
            vec![
                (Level::Warn, "WARN degraded".to_string()),
                (Level::Error, "ERROR failed".to_string())
            ]
        );
    }

    #[test]
    fn daemon_lifecycle_status_names_locked_and_running_states() {
        let (text, tone, detail) =
            daemon_lifecycle_status(Some(&SupervisorState::Locked), DaemonHealth::Starting);
        assert_eq!(text, "Daemon locked");
        assert_eq!(tone, StateTone::Warn);
        assert!(detail.contains("passphrase"));

        let (text, tone, detail) =
            daemon_lifecycle_status(Some(&SupervisorState::Running), DaemonHealth::Healthy);
        assert_eq!(text, "Daemon running");
        assert_eq!(tone, StateTone::Good);
        assert!(detail.contains("MCP"));
    }

    #[test]
    fn mcp_status_is_not_ready_until_daemon_is_healthy() {
        let (text, tone, detail) =
            mcp_status(DaemonHealth::Down, None, "http://127.0.0.1:17821/v1/status");
        assert_eq!(text, "MCP not ready");
        assert_eq!(tone, StateTone::Warn);
        assert_eq!(detail, "http://127.0.0.1:17821/mcp");

        let payload = serde_json::json!({ "mcp": { "sessions": 2 } });
        let (text, tone, detail) = mcp_status(
            DaemonHealth::Healthy,
            Some(&payload),
            "http://127.0.0.1:17821/v1/status",
        );
        assert_eq!(text, "MCP ready");
        assert_eq!(tone, StateTone::Good);
        assert!(detail.contains("2 active session"));
    }

    #[test]
    fn mcp_runtime_rows_show_endpoint_library_sessions_and_auth() {
        let payload = serde_json::json!({
            "mcp": { "sessions": 3 },
            "library": { "ready": true }
        });
        let secrets = SecretSnapshot {
            backend: "test",
            passphrase_stored: Some(true),
            bearer_token_stored: Some(true),
            last_error: None,
        };
        let rows = mcp_runtime_rows(Some(&payload), "http://127.0.0.1:17821/v1/status", &secrets);
        assert!(rows.iter().any(|(label, value)| {
            *label == "MCP URL" && value == "http://127.0.0.1:17821/mcp"
        }));
        assert!(
            rows.iter()
                .any(|(label, value)| *label == "Active sessions" && value == "3")
        );
        assert!(rows.iter().any(
            |(label, value)| *label == "Memory library" && value == "Community Memory Library"
        ));
        assert!(
            rows.iter()
                .any(|(label, value)| *label == "Library ready" && value == "true")
        );
        assert!(
            rows.iter()
                .any(|(label, value)| *label == "Bearer auth" && value.contains("keychain"))
        );
    }

    #[test]
    fn mcp_doctor_endpoint_status_treats_auth_required_as_reachable() {
        let state = SetupDoctorState::Succeeded {
            target: SetupTarget::CodexUser,
            report: SetupDoctorReport {
                profile_route: None,
                endpoint: SetupDoctorEndpoint {
                    url: "http://127.0.0.1:17821/mcp".to_string(),
                    status: "auth_required".to_string(),
                    detail: "endpoint requires bearer token".to_string(),
                    http_status: Some(401),
                    tools: None,
                },
                clients: Vec::new(),
            },
            completed_at: std::time::UNIX_EPOCH,
        };
        let (text, tone, detail) = mcp_doctor_endpoint_status(&state);
        assert_eq!(text, "auth required");
        assert_eq!(tone, StateTone::Good);
        assert!(detail.contains("bearer token"));
    }

    #[test]
    fn mcp_probe_status_reports_real_handshake_result() {
        let (text, tone, detail) = mcp_probe_status(&McpProbeState::Idle);
        assert_eq!(text, "Not probed");
        assert_eq!(tone, StateTone::Warn);
        assert!(detail.contains("initialize"));

        let success = successful_mcp_probe("work");
        let (text, tone, detail) = mcp_probe_status(&success);
        assert_eq!(text, "MCP verified");
        assert_eq!(tone, StateTone::Good);
        assert!(detail.contains("profile `work`"));
        assert!(detail.contains("18 tool"));
        assert!(detail.contains("no bearer auth"));
        assert!(detail.contains("session 12345678"));
    }

    #[test]
    fn mcp_probe_status_reports_keychain_bearer_auth_without_token_value() {
        let probe = McpProbeState::Succeeded {
            summary: McpProbeSuccess {
                profile: "work".to_string(),
                server_name: "solo".to_string(),
                server_version: "0.0.0".to_string(),
                protocol_version: "2025-03-26".to_string(),
                tool_count: 18,
                session_id: "12345678-1234-1234-1234-123456789abc".to_string(),
                used_bearer_token: true,
            },
            completed_at: std::time::UNIX_EPOCH,
        };
        let (_, _, detail) = mcp_probe_status(&probe);
        assert!(detail.contains("keychain bearer auth"));
        assert!(!detail.contains("secret-token"));
    }

    #[test]
    fn health_status_helpers_summarize_runtime_fields() {
        let payload = serde_json::json!({
            "version": "0.11.9",
            "active_tenants": 2,
            "tenant": { "id": "work" },
            "mcp": { "sessions": 3 },
            "embedder": {
                "name": "stub",
                "version": "1",
                "dim": 384,
                "dtype": "f32"
            },
            "steward": { "model": "claude-test" }
        });

        assert_eq!(status_payload_string(Some(&payload), "/tenant/id"), "work");
        assert_eq!(status_payload_string(Some(&payload), "/mcp/sessions"), "3");
        assert_eq!(
            status_embedder_summary(Some(&payload)),
            "stub 1, dim 384, f32"
        );
        assert_eq!(status_steward_summary(Some(&payload)), "claude-test");
        assert_eq!(health_state_text(DaemonHealth::Healthy), "healthy");
    }

    #[test]
    fn health_status_helpers_are_clear_when_payload_is_missing() {
        assert_eq!(status_payload_string(None, "/version"), "not reported");
        assert_eq!(status_embedder_summary(None), "not reported");
        assert_eq!(
            status_steward_summary(Some(&serde_json::json!({}))),
            "not reported by /v1/status"
        );
    }

    #[test]
    fn runtime_status_helpers_surface_embedder_and_steward_state() {
        let payload = serde_json::json!({
            "embedder": {
                "name": "ollama:nomic-embed-text",
                "version": "v1",
                "dim": 768,
                "dtype": "f32",
                "runtime": {
                    "running": false,
                    "status": "error",
                    "detail": "embedding probe failed: connection refused",
                    "checked_at_ms": 1
                }
            },
            "steward": {
                "running": true,
                "status": "no_llm",
                "runtime_wired": true,
                "runtime_has_llm": false,
                "pending_clusters": 3,
                "note": "Steward is running without a real LLM"
            }
        });

        let (embedder_state, embedder_tone, embedder_detail) =
            embedder_runtime_status(Some(&payload), DaemonHealth::Healthy, None);
        assert_eq!(embedder_state, "Offline");
        assert_eq!(embedder_tone, StateTone::Bad);
        assert!(embedder_detail.contains("connection refused"));

        let (steward_state, steward_tone, steward_detail) =
            steward_runtime_status(Some(&payload), DaemonHealth::Healthy);
        assert_eq!(steward_state, "No LLM");
        assert_eq!(steward_tone, StateTone::Warn);
        assert!(steward_detail.contains("3 pending"));
    }

    #[test]
    fn keychain_status_distinguishes_enabled_but_empty() {
        let snapshot = SecretSnapshot {
            backend: "Windows Credential Manager",
            passphrase_stored: Some(false),
            bearer_token_stored: None,
            last_error: None,
        };
        let (state, tone, detail) = keychain_passphrase_status(&snapshot, true, false);
        assert_eq!(state, "Not stored");
        assert_eq!(tone, StateTone::Warn);
        assert!(detail.contains("saved immediately"));
    }

    #[test]
    fn secret_snapshot_status_reports_both_slots_without_values() {
        let snapshot = SecretSnapshot {
            backend: "test keychain",
            passphrase_stored: Some(true),
            bearer_token_stored: Some(false),
            last_error: None,
        };
        assert_eq!(
            secret_snapshot_status(&snapshot),
            "passphrase stored; token not stored"
        );
    }

    #[test]
    fn passphrase_status_ignores_unrelated_token_state() {
        let snapshot = SecretSnapshot {
            backend: "test keychain",
            passphrase_stored: Some(false),
            bearer_token_stored: Some(true),
            last_error: None,
        };
        let (text, tone, detail) =
            passphrase_status(Some(&SupervisorState::Locked), &snapshot, true);
        assert_eq!(text, "Enter passphrase");
        assert_eq!(tone, StateTone::Warn);
        assert_eq!(detail, "Enter passphrase to start Solo.");
    }

    #[test]
    fn passphrase_status_names_keychain_disabled_restart_path() {
        let snapshot = SecretSnapshot {
            backend: "test keychain",
            passphrase_stored: None,
            bearer_token_stored: None,
            last_error: None,
        };
        let (text, tone, detail) =
            passphrase_status(Some(&SupervisorState::Stopped), &snapshot, false);
        assert_eq!(text, "Enter passphrase");
        assert_eq!(tone, StateTone::Warn);
        assert!(detail.contains("Keychain unlock is off"));
    }

    #[test]
    fn lockfile_snapshot_reports_free_and_invalid_pid_as_actionable() {
        let dir = tempfile::tempdir().expect("data dir");
        let snapshot = collect_lockfile_snapshot(dir.path());
        assert_eq!(snapshot.state, LockfileState::Free);

        std::fs::write(dir.path().join("solo.lock"), "not-a-pid").expect("write lock");
        let snapshot = collect_lockfile_snapshot(dir.path());
        assert_eq!(snapshot.state, LockfileState::Stale);
        assert!(snapshot.detail.contains("invalid PID"));
    }

    #[test]
    fn lockfile_owner_detection_accepts_only_solo_binary_name() {
        assert!(path_or_name_stem_is_solo("solo"));
        assert!(path_or_name_stem_is_solo("solo.exe"));
        assert!(!path_or_name_stem_is_solo("solo-tray"));
        assert!(!path_or_name_stem_is_solo("not-solo"));
    }

    #[test]
    fn mcp_probe_headers_ignore_legacy_profile_and_include_optional_bearer_token() {
        let client = reqwest::Client::new();
        let request = with_mcp_probe_headers(
            client.post("http://127.0.0.1:17821/mcp"),
            "work",
            Some("secret-token"),
        )
        .build()
        .unwrap();
        assert!(request.headers().get(SOLO_TENANT_HEADER).is_none());
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer secret-token"
        );

        let request = with_mcp_probe_headers(client.post("http://127.0.0.1:17821/mcp"), "", None)
            .build()
            .unwrap();
        assert!(request.headers().get(SOLO_TENANT_HEADER).is_none());
        assert!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );
    }

    #[test]
    fn memory_urls_are_derived_from_status_url() {
        assert_eq!(
            memory_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/memory"
        );
        assert_eq!(
            memory_search_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/memory/search"
        );
        assert_eq!(
            memory_context_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/memory/context"
        );
        assert_eq!(
            project_facts_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/v1/project/facts"
        );
        assert_eq!(
            project_decision_add_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/v1/project/decisions"
        );
        assert_eq!(
            project_decision_search_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/v1/project/decisions/search"
        );
        assert_eq!(
            memory_inspect_url_from_status_url("http://localhost:9999/v1/status", "019e-memory"),
            "http://localhost:9999/memory/019e-memory"
        );
        assert_eq!(
            memory_forget_url_from_status_url("http://localhost:9999/v1/status", "019e-memory"),
            "http://localhost:9999/memory/019e-memory?reason=solo_desktop"
        );
        assert_eq!(
            memory_contradictions_url_from_status_url("http://localhost:9999/v1/status", 10),
            "http://localhost:9999/memory/contradictions?limit=10"
        );
        assert_eq!(
            memory_contradiction_resolve_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/memory/contradictions/resolve"
        );
        assert_eq!(
            memory_documents_import_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/memory/documents/import"
        );
        assert_eq!(
            memory_documents_list_url_from_status_url("http://localhost:9999/v1/status", 20),
            "http://localhost:9999/memory/documents?limit=20&offset=0"
        );
        assert_eq!(
            memory_documents_search_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/memory/documents/search"
        );
        assert_eq!(
            memory_document_inspect_url_from_status_url(
                "http://localhost:9999/v1/status",
                "00000000-0000-7000-8000-000000000001",
            ),
            "http://localhost:9999/memory/documents/00000000-0000-7000-8000-000000000001"
        );
        assert_eq!(
            memory_document_forget_url_from_status_url(
                "http://localhost:9999/v1/status",
                "00000000-0000-7000-8000-000000000001",
            ),
            "http://localhost:9999/memory/documents/00000000-0000-7000-8000-000000000001"
        );
        assert_eq!(
            memory_inbox_url_from_status_url(
                "http://localhost:9999/v1/status",
                MEMORY_INBOX_RECENT_LIMIT,
            ),
            "http://localhost:9999/v1/inbox?limit=100"
        );
        assert_eq!(
            memory_inbox_review_url_from_status_url(
                "http://localhost:9999/v1/status",
                "019e-memory",
            ),
            "http://localhost:9999/v1/inbox/019e-memory/review"
        );
        assert_eq!(
            memory_url_from_status_url("http://localhost:9999/status"),
            "http://127.0.0.1:17821/memory"
        );
    }

    #[test]
    fn memory_response_parsers_extract_ids_and_search_hits() {
        assert_eq!(
            parse_memory_remember_response(r#"{ "memory_id": "019e-memory" }"#).unwrap(),
            "019e-memory"
        );

        let response = serde_json::json!({
            "hits": [{
                "memory_id": "019e-memory",
                "content": "Avery prefers concise plans.",
                "source_type": "solo_desktop.inbox",
                "tier": "Hot",
                "fused_score": 0.42,
                "cos_distance": 0.12
            }],
            "index_len": 7,
            "candidates_considered": 3
        });
        let parsed =
            parse_memory_search_response("plans", &serde_json::to_string(&response).unwrap())
                .unwrap();
        match parsed {
            MemoryActionSuccess::Search {
                query,
                hits,
                index_len,
                candidates_considered,
            } => {
                assert_eq!(query, "plans");
                assert_eq!(index_len, 7);
                assert_eq!(candidates_considered, 3);
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].memory_id, "019e-memory");
                assert_eq!(hits[0].source_type, "solo_desktop.inbox");
            }
            MemoryActionSuccess::Remembered { .. } => panic!("expected search result"),
        }
    }

    #[test]
    fn memory_context_parser_extracts_sections_and_preview_lists() {
        let response = serde_json::json!({
            "query": "desktop setup",
            "subject": "Solo",
            "resolved_subject": "Solo",
            "sections": {
                "recall": { "status": "ok", "count": 1, "warning": null },
                "themes": { "status": "ok", "count": 1, "warning": null },
                "entities": { "status": "ok", "count": 0, "warning": null },
                "facts": { "status": "ok", "count": 1, "warning": null },
                "contradictions": { "status": "ok", "count": 0, "warning": null },
                "graph": { "status": "ok", "count": 2, "warning": null }
            },
            "recall": {
                "hits": [{
                    "memory_id": "019e-memory",
                    "content": "Solo owns the daemon window.",
                    "source_type": "project_decision",
                    "tier": "Hot",
                    "fused_score": 0.91,
                    "cos_distance": 0.08
                }],
                "index_len": 4,
                "candidates_considered": 2
            },
            "themes": [{
                "cluster_id": "cluster-1",
                "abstraction_id": "abs-1",
                "abstraction_text": "Desktop setup work",
                "episode_count": 2,
                "coherence": 0.77,
                "created_at_ms": 1715625610000_i64
            }],
            "entities": [],
            "facts": [{
                "triple_id": "triple-1",
                "subject_id": "Solo",
                "predicate": "uses",
                "object_id": "daemon HTTP",
                "object_kind": "concept",
                "valid_from_ms": 1715625610000_i64,
                "valid_to_ms": null,
                "confidence": 0.82,
                "cluster_id": "cluster-1"
            }],
            "contradictions": [],
            "graph": {
                "seed_entities": ["Solo"],
                "aliases": [],
                "relationship_facts": [{
                    "edge_id": "edge-1",
                    "subject_id": "Solo",
                    "predicate": "uses",
                    "object_id": "daemon HTTP",
                    "object_kind": "entity",
                    "confidence": 0.92,
                    "strength": 0.92,
                    "evidence_count": 1,
                    "valid_from_ms": 1715625610000_i64,
                    "valid_to_ms": null,
                    "cluster_id": "cluster-1",
                    "source_episode_id": 42,
                    "memory_id": "019e-memory",
                    "evidence_preview": "Solo owns the daemon window."
                }],
                "literal_facts": [],
                "review_warnings": [{
                    "review_id": "review-1",
                    "reason_code": "weak_literal_claim",
                    "reason": "needs rewrite",
                    "subject_id": "Solo",
                    "predicate": "has",
                    "object_id": "weak literal",
                    "object_kind": "literal",
                    "confidence": 0.5
                }]
            }
        });
        let summary =
            parse_memory_context_response(&serde_json::to_string(&response).unwrap()).unwrap();

        assert_eq!(summary.query, "desktop setup");
        assert_eq!(memory_context_subject_label(&summary), "Solo");
        assert_eq!(summary.sections.len(), 6);
        assert_eq!(summary.recall_hits.len(), 1);
        assert_eq!(summary.facts.len(), 1);
        assert_eq!(summary.themes[0].cluster_id, "cluster-1");
        assert_eq!(summary.graph.seed_entities, vec!["Solo".to_string()]);
        assert_eq!(summary.graph.relationship_facts.len(), 1);
        assert_eq!(summary.graph.review_warnings.len(), 1);
        assert!(
            memory_context_status(
                &MemoryContextState::Loaded {
                    summary,
                    completed_at: std::time::UNIX_EPOCH,
                },
                DaemonHealth::Healthy,
            )
            .contains("context ready")
        );
    }

    #[test]
    fn memory_context_parser_tolerates_legacy_response_without_graph() {
        let response = serde_json::json!({
            "query": "desktop setup",
            "subject": null,
            "resolved_subject": null,
            "sections": {
                "recall": { "status": "ok", "count": 0, "warning": null },
                "themes": { "status": "ok", "count": 0, "warning": null },
                "entities": { "status": "ok", "count": 0, "warning": null },
                "facts": { "status": "ok", "count": 0, "warning": null },
                "contradictions": { "status": "ok", "count": 0, "warning": null }
            },
            "recall": {
                "hits": [],
                "index_len": 0,
                "candidates_considered": 0
            },
            "themes": [],
            "facts": [],
            "contradictions": []
        });
        let summary =
            parse_memory_context_response(&serde_json::to_string(&response).unwrap()).unwrap();

        assert_eq!(summary.sections.len(), 6);
        let graph_section = summary
            .sections
            .iter()
            .find(|section| section.name == "graph")
            .expect("default graph section");
        assert_eq!(graph_section.status, "ok");
        assert_eq!(graph_section.count, 0);
        assert!(summary.graph.seed_entities.is_empty());
        assert!(summary.graph.relationship_facts.is_empty());
        assert!(summary.graph.literal_facts.is_empty());
        assert!(summary.graph.review_warnings.is_empty());
    }

    #[test]
    fn memory_action_status_does_not_echo_memory_content() {
        let action = MemoryActionState::Failed {
            verb: MemoryActionVerb::Remember,
            message: "POST /memory returned 500".to_string(),
            completed_at: std::time::UNIX_EPOCH,
        };
        let status = memory_action_status(&action);
        assert!(status.contains("remember failed"));
        assert!(!status.contains("secret preference"));
    }

    #[test]
    fn recent_memory_parser_keeps_episode_nodes_only() {
        let response = serde_json::json!({
            "nodes": [
                {
                    "id": "ep:019e-memory",
                    "kind": "episode",
                    "label": "Avery prefers concise plans.",
                    "preview": "Avery prefers concise plans with owners.",
                    "ts_ms": 1715625610000_i64,
                    "tenant_id": "default",
                    "source_type": "solo_desktop.inbox",
                    "salience": 0.82,
                    "status": "active"
                },
                {
                    "id": "doc:skip",
                    "kind": "document",
                    "label": "skip",
                    "tenant_id": "default"
                }
            ],
            "next_cursor": null
        });
        let memories =
            parse_recent_memories_response(&serde_json::to_string(&response).unwrap()).unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].memory_id, "019e-memory");
        assert_eq!(memories[0].label, "Avery prefers concise plans.");
        assert_eq!(
            memories[0].preview,
            "Avery prefers concise plans with owners."
        );
        assert_eq!(memories[0].ts_ms, Some(1715625610000));
        assert_eq!(
            memories[0].source_type.as_deref(),
            Some("solo_desktop.inbox")
        );
        assert_eq!(memories[0].salience, Some(0.82));
        assert!(memory_source_matches_filter(
            &memories[0],
            MemorySourceFilter::HighSalience
        ));
        assert!(memory_source_matches_filter(
            &memories[0],
            MemorySourceFilter::SoloDesktop
        ));

        let inbox_response = serde_json::json!({
            "items": [{
                "memory_id": "019e-inbox",
                "label": "Inbox label",
                "preview": "Inbox preview",
                "ts_ms": 1715625620000_i64,
                "source_type": "mcp_agent",
                "salience": 0.45,
                "status": "active",
                "review_state": "dismissed",
                "reviewed_at_ms": 1715625630000_i64,
                "review_note": "not useful"
            }]
        });
        let inbox_memories =
            parse_recent_memories_response(&serde_json::to_string(&inbox_response).unwrap())
                .unwrap();
        assert_eq!(inbox_memories.len(), 1);
        assert_eq!(inbox_memories[0].memory_id, "019e-inbox");
        assert_eq!(inbox_memories[0].review_state.as_deref(), Some("dismissed"));
        assert_eq!(inbox_memories[0].reviewed_at_ms, Some(1715625630000));
        assert_eq!(inbox_memories[0].review_note.as_deref(), Some("not useful"));
    }

    #[test]
    fn memory_review_filter_counts_local_inbox_state() {
        let memories = vec![
            RecentMemory {
                memory_id: "mem-a".to_string(),
                label: "A".to_string(),
                preview: "A".to_string(),
                ts_ms: None,
                source_type: Some("user_message".to_string()),
                salience: Some(0.5),
                status: Some("active".to_string()),
                review_state: None,
                reviewed_at_ms: None,
                review_note: None,
            },
            RecentMemory {
                memory_id: "mem-b".to_string(),
                label: "B".to_string(),
                preview: "B".to_string(),
                ts_ms: None,
                source_type: Some("mcp_agent".to_string()),
                salience: Some(0.9),
                status: Some("active".to_string()),
                review_state: None,
                reviewed_at_ms: None,
                review_note: None,
            },
            RecentMemory {
                memory_id: "mem-c".to_string(),
                label: "C".to_string(),
                preview: "C".to_string(),
                ts_ms: None,
                source_type: Some("document_import".to_string()),
                salience: Some(0.3),
                status: Some("active".to_string()),
                review_state: None,
                reviewed_at_ms: None,
                review_note: None,
            },
        ];
        let mut settings = Settings::default();
        settings.memory_reviews.insert(
            "mem-a".to_string(),
            MemoryReviewStatus {
                state: "approved".to_string(),
                reviewed_at_ms: Some(0),
                note: None,
            },
        );
        settings.memory_reviews.insert(
            "mem-c".to_string(),
            MemoryReviewStatus {
                state: "dismissed".to_string(),
                reviewed_at_ms: Some(0),
                note: None,
            },
        );

        let counts = memory_review_counts(&memories, &settings);

        assert_eq!(
            counts,
            MemoryReviewCounts {
                total: 3,
                needs_review: 1,
                approved: 1,
                dismissed: 1,
            }
        );
        assert_eq!(
            memory_review_counts_label(&counts),
            "3 loaded; 1 need review; 1 approved; 1 dismissed"
        );
        assert_eq!(memory_review_status_label(&counts), "1 to review");
        assert_eq!(memory_review_status_tone(&counts), StateTone::Warn);
        assert!(
            memory_review_visible_label(
                2,
                counts.total,
                MemoryReviewFilter::NeedsReview,
                MemorySourceFilter::HighSalience,
            )
            .contains("showing 2/3")
        );
        assert!(memory_review_scope_detail(&counts).contains("2 reviewed"));
        assert!(
            memory_review_clipboard_summary(
                &counts,
                2,
                MemoryReviewFilter::NeedsReview,
                MemorySourceFilter::HighSalience,
            )
            .contains("Solo Memory Inbox")
        );
        assert!(memory_review_matches_filter(
            memory_review_status(&settings, "mem-b"),
            MemoryReviewFilter::NeedsReview
        ));
        assert!(memory_review_matches_filter(
            memory_review_status(&settings, "mem-a"),
            MemoryReviewFilter::Approved
        ));
        assert!(!memory_review_matches_filter(
            memory_review_status(&settings, "mem-c"),
            MemoryReviewFilter::NeedsReview
        ));
        assert!(memory_source_matches_filter(
            &memories[1],
            MemorySourceFilter::AgentCreated
        ));
        assert!(memory_source_matches_filter(
            &memories[2],
            MemorySourceFilter::DocumentDerived
        ));
        assert_eq!(
            memory_source_summary(&memories[0]),
            "user_message; salience 0.50; active"
        );

        let mut daemon_memories = memories.clone();
        daemon_memories[1].review_state = Some("approved".to_string());
        daemon_memories[1].reviewed_at_ms = Some(123);
        daemon_memories[1].review_note = Some("daemon wins".to_string());
        let daemon_counts = memory_review_counts(&daemon_memories, &settings);
        assert_eq!(
            daemon_counts,
            MemoryReviewCounts {
                total: 3,
                needs_review: 0,
                approved: 2,
                dismissed: 1,
            }
        );
        let effective = memory_effective_review_status(&settings, &daemon_memories[1]).unwrap();
        assert_eq!(effective.note.as_deref(), Some("daemon wins"));
        assert!(memory_review_detail(Some(&effective)).contains("Note: daemon wins"));
        assert_eq!(
            memory_review_note_summary("  keep\nthis\tshort  ").as_deref(),
            Some("keep this short")
        );
    }

    #[test]
    fn memory_review_state_updates_are_library_scoped_and_validated() {
        let mut settings = Settings::default();

        assert!(set_memory_review_state_cached(
            &mut settings,
            " mem-a ",
            Some("approved"),
            42,
        ));
        assert_eq!(
            memory_review_status(&settings, "mem-a").map(|review| review.state.as_str()),
            Some("approved")
        );

        assert!(!set_memory_review_state_cached(
            &mut settings,
            "mem-a",
            Some("maybe"),
            43,
        ));
        assert_eq!(
            memory_review_status(&settings, "mem-a").and_then(|review| review.reviewed_at_ms),
            Some(42)
        );

        assert!(set_memory_review_state_cached(
            &mut settings,
            "mem-a",
            None,
            44,
        ));
        assert_eq!(memory_review_status(&settings, "mem-a"), None);
        assert!(!set_memory_review_state_cached(
            &mut settings,
            " ",
            Some("dismissed"),
            45,
        ));
    }

    #[test]
    fn memory_detail_parser_extracts_inspect_record() {
        let response = serde_json::json!({
            "memory_id": "019e-memory",
            "ts_ms": 1715625610000_i64,
            "source_type": "solo_desktop.inbox",
            "source_id": "solo-tray-memory-inbox",
            "content": "Avery prefers concise plans.",
            "tier": "Hot",
            "status": "active",
            "confidence": 0.9,
            "strength": 0.5,
            "salience": 0.7,
            "created_at_ms": 1715625610000_i64,
            "updated_at_ms": 1715625620000_i64,
            "encoding_context_json": "{}",
            "provenance_json": null
        });
        let detail =
            parse_memory_detail_response(&serde_json::to_string(&response).unwrap()).unwrap();
        assert_eq!(detail.memory_id, "019e-memory");
        assert_eq!(detail.source_type, "solo_desktop.inbox");
        assert_eq!(
            memory_detail_source_label(&detail),
            "solo_desktop.inbox / solo-tray-memory-inbox"
        );
        assert_eq!(detail.status, "active");
        assert_eq!(detail.salience, 0.7);
        assert_eq!(detail.updated_at_ms, Some(1715625620000));
    }

    #[test]
    fn memory_update_parser_extracts_update_result() {
        let response = serde_json::json!({
            "memory_id": "019e-memory",
            "rowid": 7,
            "content": "Updated memory text.",
            "updated_at_ms": 1715625630000_i64
        });
        let update =
            parse_memory_update_response(&serde_json::to_string(&response).unwrap()).unwrap();
        assert_eq!(update.memory_id, "019e-memory");
        assert_eq!(update.content, "Updated memory text.");
        assert_eq!(update.updated_at_ms, Some(1715625630000));
    }

    #[test]
    fn memory_forget_status_tracks_selected_id_only() {
        let status = memory_forget_status(&MemoryForgetState::Failed {
            memory_id: "019e-memory".to_string(),
            message: "DELETE /memory failed".to_string(),
            completed_at: std::time::UNIX_EPOCH,
        });

        assert!(status.contains("forget 019e-memory failed"));
        assert!(!status.contains("private memory body"));
    }

    #[test]
    fn memory_contradiction_parser_extracts_lifecycle_and_triples() {
        let response = serde_json::json!([
            {
                "a_id": "triple-a",
                "b_id": "triple-b",
                "kind": "other",
                "explanation": "Preference changed.",
                "detected_at_ms": 1715625610000_i64,
                "status": "unresolved",
                "resolved_at_ms": null,
                "resolution_note": null,
                "winning_triple_id": null,
                "a_triple": {
                    "triple_id": "triple-a",
                    "subject_id": "user",
                    "predicate": "prefers",
                    "object_id": "tea",
                    "object_kind": "entity",
                    "valid_from_ms": 1715625600000_i64,
                    "valid_to_ms": null
                },
                "b_triple": {
                    "triple_id": "triple-b",
                    "subject_id": "user",
                    "predicate": "prefers",
                    "object_id": "coffee",
                    "object_kind": "entity",
                    "valid_from_ms": 1715625610000_i64,
                    "valid_to_ms": null
                }
            }
        ]);
        let contradictions =
            parse_memory_contradictions_response(&serde_json::to_string(&response).unwrap())
                .unwrap();

        assert_eq!(contradictions.len(), 1);
        let contradiction = &contradictions[0];
        assert_eq!(contradiction.a_id, "triple-a");
        assert_eq!(contradiction.status, "unresolved");
        assert_eq!(contradiction.detected_at_ms, Some(1715625610000));
        let side =
            contradiction_side_label("B", contradiction.b_triple.as_ref(), &contradiction.b_id);
        assert!(side.starts_with("B: triple-b: user --prefers--> coffee (entity); valid from "));
    }

    #[test]
    fn memory_contradiction_resolution_parser_extracts_lifecycle() {
        let response = serde_json::json!({
            "a_id": "triple-a",
            "b_id": "triple-b",
            "kind": "other",
            "status": "resolved",
            "resolved_at_ms": 1715625620000_i64,
            "resolution_note": "Resolved in Solo: triple-b is current.",
            "winning_triple_id": "triple-b"
        });
        let resolution = parse_memory_contradiction_resolution_response(
            &serde_json::to_string(&response).unwrap(),
        )
        .unwrap();

        assert_eq!(resolution.status, "resolved");
        assert_eq!(resolution.winning_triple_id.as_deref(), Some("triple-b"));
        assert_eq!(
            contradiction_resolution_note("resolved", Some("triple-b")).as_deref(),
            Some("Resolved in Solo: triple-b is current.")
        );
    }

    #[test]
    fn native_import_parser_summarizes_document_results() {
        let response = serde_json::json!({
            "path": "C:/notes",
            "dry_run": false,
            "recursive": true,
            "truncated": false,
            "total_files": 2,
            "total_bytes": 42,
            "imported": 1,
            "deduped": 1,
            "failed": 0,
            "chunks_persisted": 3,
            "files": [],
            "results": [
                {
                    "path": "C:/notes/a.md",
                    "bytes": 20,
                    "doc_id": "00000000-0000-7000-8000-000000000001",
                    "chunks_persisted": 3,
                    "bytes_ingested": 20,
                    "deduped": false
                },
                {
                    "path": "C:/notes/b.md",
                    "bytes": 22,
                    "doc_id": "00000000-0000-7000-8000-000000000002",
                    "chunks_persisted": 0,
                    "bytes_ingested": 22,
                    "deduped": true
                }
            ]
        });
        let report = parse_native_import_response(&serde_json::to_string(&response).unwrap())
            .expect("import response parses");

        assert_eq!(report.imported, 1);
        assert_eq!(report.deduped, 1);
        assert_eq!(report.results.len(), 2);
        assert!(format_native_import_report(&report).contains("doc_id=00000000"));
        assert_eq!(ImportSource::Markdown.picker_label(), "Markdown");
        assert_eq!(ImportSource::ChatGpt.picker_label(), "ChatGPT");
    }

    #[test]
    fn document_list_parser_extracts_summary_rows() {
        let response = serde_json::json!([
            {
                "doc_id": "00000000-0000-7000-8000-000000000001",
                "title": "Project Notes",
                "source": "C:/notes/project.md",
                "mime_type": "text/markdown",
                "ingested_at_ms": 1715625620000_i64,
                "chunk_count": 3,
                "status": "active"
            }
        ]);
        let documents = parse_document_list_response(&serde_json::to_string(&response).unwrap())
            .expect("documents parse");

        assert_eq!(documents.len(), 1);
        assert_eq!(document_title_label(&documents[0]), "Project Notes");
        assert_eq!(
            document_source_label(&documents[0]),
            "C:/notes/project.md / text/markdown"
        );
        assert_eq!(documents[0].chunk_count, 3);
        assert_eq!(documents[0].status, "active");
    }

    #[test]
    fn document_search_parser_extracts_chunk_hits() {
        let response = serde_json::json!([
            {
                "chunk_id": "chunk-1",
                "doc_id": "00000000-0000-7000-8000-000000000001",
                "doc_title": "Project Notes",
                "doc_source": "C:/notes/project.md",
                "doc_mime_type": "text/markdown",
                "chunk_index": 2,
                "content": "Meeting notes mention the storage plan.",
                "cos_distance": 0.125,
                "start_offset": 42,
                "end_offset": 88
            }
        ]);
        let parsed =
            parse_document_search_response("storage", &serde_json::to_string(&response).unwrap())
                .expect("document search parses");

        assert_eq!(parsed.query, "storage");
        assert_eq!(parsed.hits.len(), 1);
        assert_eq!(parsed.hits[0].chunk_id, "chunk-1");
        assert_eq!(document_search_hit_title(&parsed.hits[0]), "Project Notes");
        assert_eq!(
            document_search_hit_source(&parsed.hits[0]),
            "C:/notes/project.md / text/markdown"
        );
        assert_eq!(parsed.hits[0].chunk_index, 2);
        assert_eq!(parsed.hits[0].start_offset, 42);
        assert_eq!(parsed.hits[0].end_offset, 88);
    }

    #[test]
    fn document_search_status_reports_daemon_requirement() {
        assert_eq!(
            document_search_status(&DocumentSearchState::Idle, DaemonHealth::Down),
            "Start Solo to search imported documents."
        );
        assert_eq!(
            document_search_status(&DocumentSearchState::Idle, DaemonHealth::Healthy),
            "Search imported document chunks in the active profile."
        );
    }

    #[test]
    fn document_detail_parser_extracts_metadata_and_chunks() {
        let response = serde_json::json!({
            "document": {
                "doc_id": "00000000-0000-7000-8000-000000000001",
                "title": "Project Notes",
                "source": "C:/notes/project.md",
                "mime_type": "text/markdown",
                "ingested_at_ms": 1715625620000_i64,
                "modified_at_ms": 1715625630000_i64,
                "status": "active",
                "chunk_count": 1,
                "content_hash": "abc123",
                "byte_size": 42
            },
            "chunks": [{
                "chunk_id": "chunk-1",
                "chunk_index": 0,
                "content_preview": "# Project\nbody",
                "token_count": 4
            }]
        });
        let detail = parse_document_detail_response(&serde_json::to_string(&response).unwrap())
            .expect("document detail parses");

        assert_eq!(detail.doc_id, "00000000-0000-7000-8000-000000000001");
        assert_eq!(document_detail_title_label(&detail), "Project Notes");
        assert_eq!(detail.byte_size, Some(42));
        assert_eq!(detail.chunks.len(), 1);
        assert_eq!(detail.chunks[0].content_preview, "# Project\nbody");
    }

    #[test]
    fn document_forget_parser_extracts_tombstone_count() {
        let response = serde_json::json!({
            "doc_id": "00000000-0000-7000-8000-000000000001",
            "chunks_tombstoned": 2
        });
        let report = parse_document_forget_response(&serde_json::to_string(&response).unwrap())
            .expect("forget response parses");

        assert_eq!(report.doc_id, "00000000-0000-7000-8000-000000000001");
        assert_eq!(report.chunks_tombstoned, 2);
        assert!(
            document_forget_status(&DocumentForgetState::Forgotten {
                report,
                completed_at: std::time::UNIX_EPOCH,
            })
            .contains("2 chunk(s) tombstoned")
        );
    }

    #[test]
    fn mcp_probe_parses_initialize_and_tool_count() {
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "solo",
                    "version": "0.11.8"
                }
            }
        });
        assert_eq!(
            parse_mcp_initialize_summary(&initialize).unwrap(),
            (
                "solo".to_string(),
                "0.11.8".to_string(),
                "2025-03-26".to_string()
            )
        );

        let tools = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [
                    { "name": "memory_search" },
                    { "name": "memory_remember" }
                ]
            }
        });
        assert_eq!(parse_mcp_tools_count(&tools).unwrap(), 2);
    }

    #[test]
    fn library_status_reports_ready_database() {
        let snapshot = LibrarySnapshot {
            db_path: PathBuf::from("/tmp/solo/solo.db"),
            exists: true,
            size_bytes: Some(42),
            last_error: None,
        };

        let (text, tone, detail) = library_status(&snapshot);
        assert_eq!(text, "Library ready");
        assert_eq!(tone, StateTone::Good);
        assert!(detail.contains("Community memory library ready"));
        assert!(detail.contains("solo.db"));
    }

    #[test]
    fn project_memory_snapshot_reads_project_config() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".solo")).unwrap();
        std::fs::write(
            root.join(".solo").join("project.toml"),
            r#"
            [project]
            name = "Solo"
            id = "solo"
            tags = ["memory", "desktop"]
            "#,
        )
        .unwrap();

        let snapshot = collect_project_memory_snapshot(Some(root));

        assert_eq!(snapshot.state, ProjectMemoryState::Ready);
        let config = snapshot.config.unwrap();
        assert_eq!(config.name, "Solo");
        assert_eq!(config.project_id, "solo");
        assert_eq!(
            config.tags,
            vec!["memory".to_string(), "desktop".to_string()]
        );
        assert!(
            project_memory_summary(&ProjectMemorySnapshot {
                root: None,
                config_path: None,
                state: ProjectMemoryState::Ready,
                config: Some(config),
                detail: String::new(),
            })
            .contains("desktop")
        );
    }

    #[test]
    fn project_decision_helpers_scope_to_project_identity() {
        let root = PathBuf::from("/work/solo");
        let config = ProjectMemoryConfig {
            name: "Solo".to_string(),
            project_id: "solo".to_string(),
            tags: vec!["memory".to_string()],
        };
        let descriptor = project_descriptor_json(&config, &root);

        assert_eq!(descriptor["name"], "Solo");
        assert_eq!(descriptor["id"], "solo");
        assert_eq!(descriptor["root"], "/work/solo");
        assert_eq!(descriptor["tags"][0], "memory");
    }

    #[test]
    fn project_decision_search_parser_filters_other_memory() {
        let response = serde_json::json!({
            "hits": [
                {
                    "memory_id": "project-hit",
                    "content": "Project decision for Solo (id: solo, root: /work/solo): Keep daemon-owned Desktop state.",
                    "source_type": "project_decision",
                    "tier": "Hot",
                    "fused_score": 0.9,
                    "cos_distance": 0.1
                },
                {
                    "memory_id": "other-project",
                    "content": "Project decision for Other (id: other, root: /work/other): unrelated.",
                    "source_type": "project_decision",
                    "tier": "Hot",
                    "fused_score": 0.8,
                    "cos_distance": 0.2
                },
                {
                    "memory_id": "normal-memory",
                    "content": "Solo uses daemon HTTP.",
                    "source_type": "solo_desktop.inbox",
                    "tier": "Warm",
                    "fused_score": 0.7,
                    "cos_distance": 0.3
                }
            ],
            "index_len": 3,
            "candidates_considered": 3
        });
        let parsed = parse_project_decision_search_response(
            "daemon",
            "solo",
            &serde_json::to_string(&response).unwrap(),
        )
        .expect("project decision search parses");

        match parsed {
            ProjectDecisionSuccess::Search { query, hits } => {
                assert_eq!(query, "daemon");
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].memory_id, "project-hit");
            }
            ProjectDecisionSuccess::Added { .. } => panic!("expected search result"),
        }

        let cli_response = serde_json::json!({
            "command": "project decisions",
            "action": "query",
            "project": {
                "id": "solo",
                "name": "Solo",
                "root": "/work/solo",
                "tags": []
            },
            "query": "daemon",
            "limit": 25,
            "hits": [
                {
                    "rowid": 42,
                    "memory_id": "structured-hit",
                    "content": "Keep daemon-owned Desktop state.",
                    "source_type": "project_decision",
                    "tier": "Hot",
                    "fused_score": 0.9,
                    "cos_distance": 0.1
                }
            ]
        });
        let parsed = parse_project_decision_search_response(
            "fallback",
            "solo",
            &serde_json::to_string(&cli_response).unwrap(),
        )
        .expect("project decision CLI JSON parses");
        match parsed {
            ProjectDecisionSuccess::Search { query, hits } => {
                assert_eq!(query, "daemon");
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].memory_id, "structured-hit");
            }
            ProjectDecisionSuccess::Added { .. } => panic!("expected search result"),
        }
    }

    #[test]
    fn project_decision_add_parser_accepts_cli_json_envelope() {
        let response = serde_json::json!({
            "command": "project decisions",
            "action": "add",
            "project": { "id": "solo", "name": "Solo", "root": "/work/solo" },
            "memory_id": "01HZPROJECTDECISION000000000000",
            "source_type": "project_decision",
            "source_id": "project:solo:decision:1715625610000",
            "content": "Project decision for Solo (id: solo): use daemon HTTP"
        });
        let parsed = parse_project_decision_add_response(
            &serde_json::to_string(&response).expect("serialize response"),
        )
        .expect("project decision add parses");

        match parsed {
            ProjectDecisionSuccess::Added { memory_id } => {
                assert_eq!(memory_id, "01HZPROJECTDECISION000000000000");
            }
            ProjectDecisionSuccess::Search { .. } => panic!("expected add result"),
        }
    }

    #[test]
    fn project_decision_status_reports_daemon_and_config_requirements() {
        let mut snapshot = ProjectMemorySnapshot {
            root: Some(PathBuf::from("/work/solo")),
            config_path: None,
            state: ProjectMemoryState::MissingConfig,
            config: None,
            detail: String::new(),
        };
        assert_eq!(
            project_decision_status(
                &ProjectDecisionActionState::Idle,
                &snapshot,
                DaemonHealth::Healthy,
            ),
            "create `.solo/project.toml` before saving project decisions"
        );

        snapshot.state = ProjectMemoryState::Ready;
        snapshot.config = Some(ProjectMemoryConfig {
            name: "Solo".to_string(),
            project_id: "solo".to_string(),
            tags: Vec::new(),
        });
        assert_eq!(
            project_decision_status(
                &ProjectDecisionActionState::Idle,
                &snapshot,
                DaemonHealth::Down
            ),
            "Start Solo to save or search project decisions."
        );
    }

    #[test]
    fn project_facts_parser_extracts_fact_hits() {
        let facts = serde_json::json!([
            {
                "triple_id": "triple-1",
                "subject_id": "Solo",
                "predicate": "uses",
                "object_id": "daemon HTTP",
                "object_kind": "concept",
                "valid_from_ms": 1715625610000_i64,
                "valid_to_ms": null,
                "confidence": 0.82,
                "cluster_id": "cluster-1"
            }
        ]);
        let response = serde_json::json!({
            "command": "project facts",
            "project": { "id": "solo", "name": "Solo", "root": "/work/solo" },
            "subject": "Solo Project",
            "facts": facts
        });
        let parsed = parse_project_facts_response(
            "Fallback",
            &serde_json::to_string(&response).expect("serialize response"),
        )
        .expect("project facts parse");

        assert_eq!(parsed.subject, "Solo Project");
        assert_eq!(parsed.facts.len(), 1);
        assert_eq!(
            project_fact_label(&parsed.facts[0]),
            "Solo --uses--> daemon HTTP (concept)"
        );
        assert_eq!(parsed.facts[0].cluster_id.as_deref(), Some("cluster-1"));

        let raw_facts = response.get("facts").expect("facts array");
        let parsed =
            parse_project_facts_response("Solo", &serde_json::to_string(raw_facts).unwrap())
                .expect("daemon project facts array still parses");
        assert_eq!(parsed.subject, "Solo");
        assert_eq!(parsed.facts.len(), 1);
    }

    #[test]
    fn project_facts_status_reports_daemon_and_config_requirements() {
        let mut snapshot = ProjectMemorySnapshot {
            root: Some(PathBuf::from("/work/solo")),
            config_path: None,
            state: ProjectMemoryState::MissingConfig,
            config: None,
            detail: String::new(),
        };
        assert_eq!(
            project_facts_status(&ProjectFactsState::Idle, &snapshot, DaemonHealth::Healthy),
            "create `.solo/project.toml` before loading project facts"
        );

        snapshot.state = ProjectMemoryState::Ready;
        snapshot.config = Some(ProjectMemoryConfig {
            name: "Solo".to_string(),
            project_id: "solo".to_string(),
            tags: Vec::new(),
        });
        assert_eq!(
            project_facts_subject(snapshot.config.as_ref().unwrap(), ""),
            "Solo"
        );
        assert_eq!(
            project_facts_status(&ProjectFactsState::Idle, &snapshot, DaemonHealth::Down),
            "Start Solo to load project facts."
        );
    }

    #[test]
    fn project_command_fallbacks_quote_project_paths() {
        let root = PathBuf::from(r"C:\Users\Ada Lovelace\project one");
        let data_dir = PathBuf::from(r"C:\Users\Ada Lovelace\.solo data");
        assert_eq!(
            project_init_command(&root),
            "solo project init \"C:\\Users\\Ada Lovelace\\project one\""
        );
        assert!(project_ingest_dry_run_command(&root).contains("--dry-run"));
        assert!(
            project_facts_json_command(&root, "Project One", &data_dir)
                .contains("--subject \"Project One\"")
        );
        assert!(
            project_decision_add_json_command(&root, "Use JSON output", &data_dir)
                .contains("--add \"Use JSON output\"")
        );
        assert!(
            project_decision_search_json_command(&root, "JSON output", &data_dir)
                .contains("--query \"JSON output\"")
        );
        assert!(
            project_codex_setup_command(&root, "http://127.0.0.1:17821/mcp")
                .contains("--scope project")
        );
        assert_eq!(
            project_agent_policy_command(&root, ProjectPolicyClient::Codex),
            "solo project policy \"C:\\Users\\Ada Lovelace\\project one\" --client codex"
        );
    }

    #[test]
    fn project_action_args_use_safe_cli_shapes() {
        let root = Path::new("/work/solo");
        assert_eq!(
            arg_strings(project_action_args(ProjectActionKind::Init, root)),
            vec!["project", "init", "/work/solo"]
        );
        assert_eq!(
            arg_strings(project_action_args(ProjectActionKind::Preview, root)),
            vec!["project", "ingest", "/work/solo", "--dry-run", "--json"]
        );
    }

    #[test]
    fn project_agent_policy_uses_project_config_or_safe_default() {
        let ready = ProjectMemorySnapshot {
            root: Some(PathBuf::from("/work/solo")),
            config_path: Some(PathBuf::from("/work/solo/.solo/project.toml")),
            state: ProjectMemoryState::Ready,
            config: Some(ProjectMemoryConfig {
                name: "Solo".to_string(),
                project_id: "solo".to_string(),
                tags: vec!["memory".to_string(), "desktop".to_string()],
            }),
            detail: String::new(),
        };
        let ready_policy = project_agent_policy(&ready, ProjectPolicyClient::Claude)
            .expect("ready project has a policy");
        assert!(ready_policy.contains("Solo Project Memory Policy - Claude"));
        assert!(ready_policy.contains("Project id: solo"));
        assert!(ready_policy.contains("Project tags: memory, desktop"));
        assert!(ready_policy.contains("Do not store secrets"));

        let missing_config = ProjectMemorySnapshot {
            root: Some(PathBuf::from("/work/my project")),
            config_path: Some(PathBuf::from("/work/my project/.solo/project.toml")),
            state: ProjectMemoryState::MissingConfig,
            config: None,
            detail: String::new(),
        };
        let default_policy = project_agent_policy(&missing_config, ProjectPolicyClient::Codex)
            .expect("missing config can use a default project identity");
        assert!(default_policy.contains("Project name: my project"));
        assert!(default_policy.contains("Project id: my-project"));

        let invalid = ProjectMemorySnapshot {
            root: Some(PathBuf::from("/work/bad")),
            config_path: Some(PathBuf::from("/work/bad/.solo/project.toml")),
            state: ProjectMemoryState::InvalidConfig,
            config: None,
            detail: String::new(),
        };
        assert!(project_agent_policy(&invalid, ProjectPolicyClient::Codex).is_none());
    }

    #[test]
    fn generic_policy_pack_row_exposes_portable_memory_rules() {
        let row = generic_policy_pack_row();
        assert_eq!(row.label, "Generic MCP agent");
        assert!(row.text.contains("memory_context"));
        assert!(row.text.contains("Do not") || row.text.contains("Never store"));
    }

    #[test]
    fn setup_targets_map_to_matching_policy_text() {
        assert!(policy_text_for_setup_target(SetupTarget::ClaudeDesktop).contains("Claude"));
        assert!(policy_text_for_setup_target(SetupTarget::Cursor).contains("Cursor"));
        assert_eq!(
            policy_text_for_setup_target(SetupTarget::CodexUser),
            policy_text_for_setup_target(SetupTarget::CodexProject)
        );
        assert!(policy_text_for_setup_target(SetupTarget::CodexUser).contains("Codex"));
    }

    #[test]
    fn client_smoke_instruction_makes_manual_checks_actionable() {
        assert_eq!(
            client_smoke_instruction(SetupTarget::CodexUser, None),
            "codex mcp list"
        );
        let project = client_smoke_instruction(
            SetupTarget::CodexProject,
            Some(Path::new("/work/my project")),
        );
        assert!(project.contains("cd"));
        assert!(project.contains("codex mcp list"));
        assert!(project.contains("\"/work/my project\""));
        assert!(client_smoke_instruction(SetupTarget::ClaudeDesktop, None).contains("Restart"));
        assert!(client_smoke_instruction(SetupTarget::Cursor, None).contains("Cursor"));
    }

    #[test]
    fn codex_client_check_command_uses_expected_scope() {
        let user = client_check_command(SetupTarget::CodexUser, None).expect("user command");
        assert_eq!(user.bin, PathBuf::from("codex"));
        assert_eq!(arg_strings(user.args), vec!["mcp", "list"]);
        assert_eq!(user.cwd, None);

        let project_root = PathBuf::from("/work/my project");
        let project = client_check_command(SetupTarget::CodexProject, Some(project_root.clone()))
            .expect("project command");
        assert_eq!(project.bin, PathBuf::from("codex"));
        assert_eq!(arg_strings(project.args), vec!["mcp", "list"]);
        assert_eq!(project.cwd, Some(project_root));

        assert!(client_check_command(SetupTarget::Cursor, None).is_err());
        assert!(client_check_command(SetupTarget::CodexProject, None).is_err());
    }

    #[test]
    fn codex_mcp_list_parser_requires_solo_server_token() {
        assert!(codex_mcp_list_contains_solo(
            "solo  http://127.0.0.1:17821/mcp"
        ));
        assert!(codex_mcp_list_contains_solo("Name\nSOLO\n"));
        assert!(!codex_mcp_list_contains_solo("No MCP servers configured"));
        assert!(!codex_mcp_list_contains_solo("soloweb"));
    }

    #[test]
    fn codex_client_check_errors_are_actionable() {
        let permission = client_check_command_error(
            SetupTarget::CodexUser,
            "run codex: Access is denied. (os error 5)",
        );
        assert!(permission.contains("Codex CLI is not runnable"));
        assert!(permission.contains("Copy check"));

        let missing = client_check_command_error(
            SetupTarget::CodexUser,
            "run codex: The system cannot find the file specified. (os error 2)",
        );
        assert!(missing.contains("not found on PATH"));
    }

    #[test]
    fn project_docs_preview_parser_extracts_candidates() {
        let response = serde_json::json!({
            "root": "/work/solo",
            "project": {
                "name": "Solo",
                "project_id": "solo"
            },
            "files_scanned": 7,
            "candidates_found": 2,
            "truncated": false,
            "candidates": [
                {
                    "path": "/work/solo/README.md",
                    "relative_path": "README.md"
                },
                "/work/solo/docs/architecture.md"
            ]
        });

        let preview = parse_project_docs_preview(&serde_json::to_string(&response).unwrap())
            .expect("preview parses");

        assert_eq!(preview.project_name, "Solo");
        assert_eq!(preview.project_id, "solo");
        assert_eq!(preview.files_scanned, 7);
        assert_eq!(preview.candidates_found, 2);
        assert_eq!(preview.candidates[0].label, "README.md");
        assert_eq!(
            preview.candidates[1].path,
            "/work/solo/docs/architecture.md"
        );
        assert!(format_project_docs_preview(&preview).contains("candidate: README.md"));
    }

    #[test]
    fn project_docs_import_requires_preview_daemon_and_confirmation() {
        let preview = ProjectDocsPreview {
            root: "/work/solo".to_string(),
            project_name: "Solo".to_string(),
            project_id: "solo".to_string(),
            files_scanned: 1,
            candidates_found: 1,
            truncated: false,
            candidates: vec![ProjectDocCandidate {
                path: "/work/solo/README.md".to_string(),
                label: "README.md".to_string(),
            }],
        };

        assert!(!can_import_project_docs(
            Some(&preview),
            DaemonHealth::Down,
            false,
            true
        ));
        assert!(!can_import_project_docs(
            Some(&preview),
            DaemonHealth::Healthy,
            false,
            false
        ));
        assert!(can_import_project_docs(
            Some(&preview),
            DaemonHealth::Healthy,
            false,
            true
        ));
    }

    #[test]
    fn project_action_eligibility_matches_project_state() {
        let mut snapshot = ProjectMemorySnapshot {
            root: Some(PathBuf::from("/work/solo")),
            config_path: None,
            state: ProjectMemoryState::MissingConfig,
            config: None,
            detail: String::new(),
        };
        assert!(can_offer_project_init(&snapshot));
        assert!(!can_run_project_action(
            ProjectActionKind::Init,
            &snapshot,
            false
        ));
        assert!(can_run_project_action(
            ProjectActionKind::Init,
            &snapshot,
            true
        ));
        assert!(can_preview_project_docs(&snapshot));

        snapshot.state = ProjectMemoryState::Ready;
        assert!(!can_offer_project_init(&snapshot));
        assert!(!can_run_project_action(
            ProjectActionKind::Init,
            &snapshot,
            true
        ));
        assert!(can_preview_project_docs(&snapshot));

        snapshot.state = ProjectMemoryState::InvalidConfig;
        assert!(!can_run_project_action(
            ProjectActionKind::Init,
            &snapshot,
            true
        ));
        assert!(!can_preview_project_docs(&snapshot));

        snapshot.state = ProjectMemoryState::MissingRoot;
        assert!(!can_run_project_action(
            ProjectActionKind::Init,
            &snapshot,
            true
        ));
        assert!(!can_preview_project_docs(&snapshot));
    }

    #[test]
    fn import_preview_args_are_dry_run_only() {
        let path = Path::new("/exports/chatgpt");
        let data_dir = Path::new("/solo/data");
        assert_eq!(ImportSource::Markdown.picker_label(), "Markdown");
        assert_eq!(ImportSource::ChatGpt.picker_label(), "ChatGPT");
        assert_eq!(
            arg_strings(import_preview_args(ImportSource::ChatGpt, path, data_dir)),
            vec![
                "import",
                "chatgpt",
                "/exports/chatgpt",
                "--dry-run",
                "--json",
                "--data-dir",
                "/solo/data"
            ]
        );
        assert_eq!(
            arg_strings(import_preview_args(ImportSource::Bookmarks, path, data_dir)),
            vec![
                "import",
                "bookmarks",
                "/exports/chatgpt",
                "--dry-run",
                "--json",
                "--data-dir",
                "/solo/data"
            ]
        );
        assert!(
            import_preview_command(
                ImportSource::Markdown,
                Path::new("/docs/my notes"),
                data_dir
            )
            .contains("\"/docs/my notes\"")
        );
        assert!(
            import_preview_command(
                ImportSource::Markdown,
                Path::new("/docs/my notes"),
                data_dir
            )
            .contains("--data-dir")
        );
    }

    #[test]
    fn import_commit_requires_matching_preview_source_and_path() {
        let preview = ImportActionState::Succeeded {
            source: ImportSource::Markdown,
            path: PathBuf::from("/docs/current"),
            message: "preview ok".to_string(),
            output: String::new(),
            completed_at: std::time::SystemTime::UNIX_EPOCH,
        };

        assert!(import_preview_matches(
            &preview,
            ImportSource::Markdown,
            Path::new("/docs/current")
        ));
        assert!(!import_preview_matches(
            &preview,
            ImportSource::Text,
            Path::new("/docs/current")
        ));
        assert!(!import_preview_matches(
            &preview,
            ImportSource::Markdown,
            Path::new("/docs/changed")
        ));

        let status = import_action_status(&preview);
        assert!(status.contains("Markdown preview succeeded"));
        assert!(status.contains("/docs/current"));
    }

    #[test]
    fn import_preview_parser_summarizes_document_json() {
        let body = serde_json::json!({
            "command": "import markdown",
            "path": "/docs",
            "dry_run": true,
            "files_scanned": 3,
            "candidate_files": 2,
            "skipped_files": 1,
            "estimated_chunk_candidates": 4,
            "enabled_extensions": ["md", "markdown"]
        });

        let parsed = parse_import_preview_response(ImportSource::Markdown, &body.to_string())
            .expect("document preview parses");

        assert!(parsed.message.contains("2 candidate file(s)"));
        assert!(parsed.output.contains("files scanned: 3"));
        assert!(parsed.output.contains("source: markdown"));
        assert!(parsed.output.contains("enabled extensions: md, markdown"));
    }

    #[test]
    fn import_preview_parser_summarizes_schema_json() {
        let body = serde_json::json!({
            "command": "import chatgpt",
            "path": "/exports",
            "dry_run": true,
            "records_scanned": 5,
            "candidate_records": 3,
            "filtered_records": 1,
            "skipped_records": 1,
            "estimated_chunk_candidates": 8,
            "materialized_format": "markdown"
        });

        let parsed = parse_import_preview_response(ImportSource::ChatGpt, &body.to_string())
            .expect("schema preview parses");

        assert!(parsed.message.contains("3 candidate record(s)"));
        assert!(parsed.message.contains("1 filtered"));
        assert!(parsed.output.contains("records scanned: 5"));
        assert!(parsed.output.contains("source: ChatGPT"));
        assert!(parsed.output.contains("materialized format: markdown"));
    }

    #[test]
    fn connected_tool_last_status_is_scoped_by_resolved_profile() {
        let mut statuses = std::collections::BTreeMap::new();
        statuses.insert(
            connected_tool_status_key(SetupTarget::CodexUser, "work"),
            ConnectedToolLastStatus {
                status: "verified".to_string(),
                detail: "work profile verified".to_string(),
                resolved_profile: Some("work".to_string()),
                ..ConnectedToolLastStatus::default()
            },
        );
        statuses.insert(
            connected_tool_status_key(SetupTarget::CodexUser, "private"),
            ConnectedToolLastStatus {
                status: "failed".to_string(),
                detail: "private profile failed".to_string(),
                resolved_profile: Some("private".to_string()),
                ..ConnectedToolLastStatus::default()
            },
        );

        let work = connected_tool_last_status(
            &statuses,
            SetupTarget::CodexUser,
            &ToolProfileRoute::Explicit("work".to_string()),
            "private",
        )
        .expect("work-scoped history");
        assert_eq!(work.detail, "work profile verified");

        let private = connected_tool_last_status(
            &statuses,
            SetupTarget::CodexUser,
            &ToolProfileRoute::DaemonDefault,
            "private",
        )
        .expect("daemon-default history follows active profile");
        assert_eq!(private.detail, "private profile failed");
    }

    #[test]
    fn connected_tool_last_status_only_uses_matching_legacy_status() {
        let mut statuses = std::collections::BTreeMap::new();
        statuses.insert(
            SetupTarget::CodexUser.key().to_string(),
            ConnectedToolLastStatus {
                status: "verified".to_string(),
                detail: "legacy work status".to_string(),
                resolved_profile: Some("work".to_string()),
                ..ConnectedToolLastStatus::default()
            },
        );

        assert!(
            connected_tool_last_status(
                &statuses,
                SetupTarget::CodexUser,
                &ToolProfileRoute::DaemonDefault,
                "private",
            )
            .is_none()
        );
        assert!(
            connected_tool_last_status(
                &statuses,
                SetupTarget::CodexUser,
                &ToolProfileRoute::DaemonDefault,
                "work",
            )
            .is_some()
        );
    }

    #[test]
    fn setup_wizard_completion_requires_daemon_library_mcp_and_tool() {
        let library = LibrarySnapshot {
            db_path: PathBuf::from("/tmp/solo/solo.db"),
            exists: true,
            size_bytes: Some(1),
            last_error: None,
        };
        let tools = ToolSnapshot {
            rows: vec![ToolConfigRow {
                target: SetupTarget::CodexUser,
                path: Some(PathBuf::from("/home/ada/.codex/config.toml")),
                state: ToolConfigState::Verified,
                transport: ToolTransport::Http,
                profile_route: ToolProfileRoute::DaemonDefault,
                detail: "ok".to_string(),
                last_status: None,
            }],
        };
        let ready_probe = successful_mcp_probe("default");

        assert!(setup_wizard_is_complete(
            Some(&SupervisorState::Running),
            DaemonHealth::Healthy,
            &library,
            "default",
            &tools,
            &ready_probe,
            true,
            true,
        ));
        assert!(!setup_wizard_is_complete(
            Some(&SupervisorState::Locked),
            DaemonHealth::Starting,
            &library,
            "default",
            &tools,
            &ready_probe,
            true,
            true,
        ));
        assert!(!setup_wizard_is_complete(
            Some(&SupervisorState::Running),
            DaemonHealth::Healthy,
            &library,
            "work",
            &tools,
            &ready_probe,
            true,
            true,
        ));

        let no_tools = ToolSnapshot { rows: Vec::new() };
        assert!(!setup_wizard_is_complete(
            Some(&SupervisorState::Running),
            DaemonHealth::Healthy,
            &library,
            "default",
            &no_tools,
            &ready_probe,
            true,
            true,
        ));
        assert!(!setup_wizard_is_complete(
            Some(&SupervisorState::Running),
            DaemonHealth::Healthy,
            &library,
            "default",
            &tools,
            &McpProbeState::Idle,
            true,
            true,
        ));
        assert!(!setup_wizard_is_complete(
            Some(&SupervisorState::Running),
            DaemonHealth::Healthy,
            &library,
            "default",
            &tools,
            &successful_mcp_probe("work"),
            true,
            true,
        ));
        assert!(!setup_wizard_is_complete(
            Some(&SupervisorState::Running),
            DaemonHealth::Healthy,
            &library,
            "default",
            &tools,
            &ready_probe,
            false,
            true,
        ));
        assert!(!setup_wizard_is_complete(
            Some(&SupervisorState::Running),
            DaemonHealth::Healthy,
            &library,
            "default",
            &tools,
            &ready_probe,
            true,
            false,
        ));
    }

    #[test]
    fn setup_wizard_import_ready_accepts_committed_or_visible_documents() {
        let import_commit = ImportCommitState::Succeeded {
            report: native_import_report(1),
            completed_at: std::time::UNIX_EPOCH,
        };
        assert!(setup_wizard_import_ready(
            &import_commit,
            &DocumentListState::Idle,
            &ProjectDocsImportState::Idle,
        ));
        assert!(
            setup_wizard_import_detail(
                &import_commit,
                &DocumentListState::Idle,
                &ProjectDocsImportState::Idle,
            )
            .contains("Imported 1")
        );

        let document_list = DocumentListState::Loaded {
            documents: vec![DocumentSummary {
                doc_id: "doc-1".to_string(),
                title: Some("Notes".to_string()),
                source: None,
                mime_type: None,
                ingested_at_ms: None,
                chunk_count: 1,
                status: "active".to_string(),
            }],
            completed_at: std::time::UNIX_EPOCH,
        };
        assert!(setup_wizard_import_ready(
            &ImportCommitState::Idle,
            &document_list,
            &ProjectDocsImportState::Idle,
        ));
        assert!(!setup_wizard_import_ready(
            &ImportCommitState::Idle,
            &DocumentListState::Idle,
            &ProjectDocsImportState::Idle,
        ));
    }

    #[test]
    fn setup_wizard_review_ready_is_library_scoped() {
        let mut settings = Settings::default();
        settings.memory_reviews.insert(
            "mem-1".to_string(),
            MemoryReviewStatus {
                state: "approved".to_string(),
                reviewed_at_ms: Some(1),
                note: None,
            },
        );
        let idle_recent = MemoryRecentState::Idle;
        assert!(setup_wizard_review_ready(&settings, &idle_recent));
        assert!(setup_wizard_review_detail(&settings, &idle_recent).contains("1 cached"));

        let daemon_recent = MemoryRecentState::Loaded {
            memories: vec![RecentMemory {
                memory_id: "mem-daemon".to_string(),
                label: "Daemon reviewed".to_string(),
                preview: "Daemon reviewed".to_string(),
                ts_ms: Some(1),
                source_type: Some("mcp_agent".to_string()),
                salience: Some(0.8),
                status: Some("active".to_string()),
                review_state: Some("dismissed".to_string()),
                reviewed_at_ms: Some(2),
                review_note: None,
            }],
            completed_at: std::time::SystemTime::now(),
        };
        let empty_settings = Settings::default();
        assert!(setup_wizard_review_ready(&empty_settings, &daemon_recent));
        assert!(setup_wizard_review_detail(&empty_settings, &daemon_recent).contains("visible"));
    }

    #[test]
    fn workspace_access_scope_gates_global_and_project_targets() {
        assert!(workspace_access_scope_allows_target(
            WorkspaceAccessScope::GlobalOnly,
            SetupTarget::CodexUser
        ));
        assert!(!workspace_access_scope_allows_target(
            WorkspaceAccessScope::GlobalOnly,
            SetupTarget::CodexProject
        ));
        assert!(workspace_access_scope_allows_target(
            WorkspaceAccessScope::ProjectOnly,
            SetupTarget::CodexProject
        ));
        assert!(!workspace_access_scope_allows_target(
            WorkspaceAccessScope::ProjectOnly,
            SetupTarget::ClaudeDesktop
        ));
        assert!(workspace_access_scope_allows_target(
            WorkspaceAccessScope::GlobalAndProject,
            SetupTarget::Cursor
        ));
        assert!(workspace_access_scope_allows_target(
            WorkspaceAccessScope::GlobalAndProject,
            SetupTarget::CodexProject
        ));
    }

    #[test]
    fn workspace_scope_project_status_requires_selected_root() {
        let missing = ProjectMemorySnapshot {
            root: None,
            config_path: None,
            state: ProjectMemoryState::NotSelected,
            config: None,
            detail: "not selected".to_string(),
        };
        let (text, tone, detail) =
            workspace_scope_project_status(WorkspaceAccessScope::GlobalAndProject, &missing);
        assert_eq!(text, "Needs project root");
        assert_eq!(tone, StateTone::Warn);
        assert!(detail.contains("Select a project root"));
        assert!(!workspace_access_target_ready(
            WorkspaceAccessScope::GlobalAndProject,
            SetupTarget::CodexProject,
            &missing
        ));
        assert!(workspace_access_target_ready(
            WorkspaceAccessScope::GlobalAndProject,
            SetupTarget::CodexUser,
            &missing
        ));

        let ready = ProjectMemorySnapshot {
            root: Some(PathBuf::from("/work/solo")),
            config_path: Some(PathBuf::from("/work/solo/.solo/project.toml")),
            state: ProjectMemoryState::Ready,
            config: Some(ProjectMemoryConfig {
                name: "Solo".to_string(),
                project_id: "solo".to_string(),
                tags: vec!["agent".to_string()],
            }),
            detail: "configured".to_string(),
        };
        let (text, tone, detail) =
            workspace_scope_project_status(WorkspaceAccessScope::GlobalAndProject, &ready);
        assert_eq!(text, "configured");
        assert_eq!(tone, StateTone::Good);
        assert!(detail.contains("solo"));
        assert!(workspace_access_target_ready(
            WorkspaceAccessScope::GlobalAndProject,
            SetupTarget::CodexProject,
            &ready
        ));
        assert!(!workspace_access_target_ready(
            WorkspaceAccessScope::GlobalOnly,
            SetupTarget::CodexProject,
            &ready
        ));
    }

    #[test]
    fn workspace_file_access_config_edit_sets_and_clears_project_root() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let project = tempfile::tempdir().expect("project dir");
        let config_path = data_dir.path().join("solo.config.toml");
        write_minimal_solo_config(&config_path);

        let backup = set_workspace_file_access_allowed_roots(
            &config_path,
            Some(vec![project.path().to_path_buf()]),
        )
        .expect("set allowed roots");
        assert!(backup.is_file());

        let snapshot = collect_workspace_file_access_snapshot(data_dir.path());
        let canonical_project =
            std::fs::canonicalize(project.path()).expect("canonical project dir");
        assert_eq!(snapshot.state, WorkspaceFileAccessState::Restricted);
        assert_eq!(
            snapshot.allowed_roots,
            vec![display_user_path(&canonical_project)]
        );

        let backup = set_workspace_file_access_allowed_roots(&config_path, None)
            .expect("clear allowed roots");
        assert!(backup.is_file());

        let snapshot = collect_workspace_file_access_snapshot(data_dir.path());
        assert_eq!(snapshot.state, WorkspaceFileAccessState::Unrestricted);
        assert!(snapshot.allowed_roots.is_empty());
    }

    #[test]
    fn workspace_file_access_project_status_detects_selected_root() {
        let project = tempfile::tempdir().expect("project dir");
        let project_root = std::fs::canonicalize(project.path()).expect("canonical project");
        let access = WorkspaceFileAccessSnapshot {
            config_path: PathBuf::from("/solo/solo.config.toml"),
            state: WorkspaceFileAccessState::Restricted,
            allowed_roots: vec![display_user_path(&project_root)],
            env_override: None,
            detail: "restricted".to_string(),
        };
        let project = ProjectMemorySnapshot {
            root: Some(project_root),
            config_path: None,
            state: ProjectMemoryState::Ready,
            config: None,
            detail: "configured".to_string(),
        };

        let (text, tone, detail) = workspace_file_access_project_status(&access, &project);

        assert_eq!(text, "Allowed");
        assert_eq!(tone, StateTone::Good);
        assert!(detail.contains("allowed_roots"));
    }

    #[test]
    fn workspace_file_access_runtime_status_reports_restart_required() {
        let access = WorkspaceFileAccessSnapshot {
            config_path: PathBuf::from("/solo/solo.config.toml"),
            state: WorkspaceFileAccessState::Restricted,
            allowed_roots: vec![r"C:\work\solo".to_string()],
            env_override: None,
            detail: "restricted".to_string(),
        };

        let (text, tone, detail) = workspace_file_access_runtime_status(&access, true);

        assert_eq!(text, "Restart required");
        assert_eq!(tone, StateTone::Warn);
        assert!(detail.contains("restart Solo"));
    }

    #[test]
    fn workspace_file_access_runtime_status_reports_env_override() {
        let access = WorkspaceFileAccessSnapshot {
            config_path: PathBuf::from("/solo/solo.config.toml"),
            state: WorkspaceFileAccessState::Restricted,
            allowed_roots: vec![r"C:\work\solo".to_string()],
            env_override: Some(r"C:\override".to_string()),
            detail: "restricted".to_string(),
        };

        let (text, tone, detail) = workspace_file_access_runtime_status(&access, false);

        assert_eq!(text, "Env override");
        assert_eq!(tone, StateTone::Warn);
        assert!(detail.contains(WORKSPACE_FILE_ROOTS_ENV));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_file_access_config_edit_preserves_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = tempfile::tempdir().expect("data dir");
        let project = tempfile::tempdir().expect("project dir");
        let config_path = data_dir.path().join("solo.config.toml");
        write_minimal_solo_config(&config_path);
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600))
            .expect("set config permissions");

        set_workspace_file_access_allowed_roots(
            &config_path,
            Some(vec![project.path().to_path_buf()]),
        )
        .expect("set allowed roots");

        let mode = std::fs::metadata(&config_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn display_user_path_strips_windows_verbatim_prefixes() {
        assert_eq!(
            display_user_path(Path::new(r"\\?\C:\work\solo")),
            r"C:\work\solo"
        );
        assert_eq!(
            display_user_path(Path::new(r"\\?\UNC\server\share\solo")),
            r"\\server\share\solo"
        );
    }

    #[test]
    fn setup_wizard_step_state_marks_first_incomplete_as_active() {
        assert_eq!(
            setup_wizard_step_state(true, true),
            SetupWizardStepState::Complete
        );
        assert_eq!(
            setup_wizard_step_state(false, true),
            SetupWizardStepState::Active
        );
        assert_eq!(
            setup_wizard_step_state(false, false),
            SetupWizardStepState::Waiting
        );
    }

    #[test]
    fn tool_config_detection_uses_native_paths_for_supported_desktops() {
        let claude_windows = detect_tool_config_path_for_os(
            SetupTarget::ClaudeDesktop,
            "windows",
            &env_lookup(&[("APPDATA", r"C:\Users\Ada\AppData\Roaming")]),
            None,
        );
        let path = claude_windows.path.expect("windows claude path");
        assert!(path.ends_with(Path::new("Claude").join("claude_desktop_config.json")));
        assert!(path.display().to_string().contains("AppData"));

        let claude_macos = detect_tool_config_path_for_os(
            SetupTarget::ClaudeDesktop,
            "macos",
            &env_lookup(&[("HOME", "/Users/ada")]),
            None,
        );
        assert_eq!(
            claude_macos.path,
            Some(
                PathBuf::from("/Users/ada")
                    .join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            )
        );

        let claude_linux = detect_tool_config_path_for_os(
            SetupTarget::ClaudeDesktop,
            "linux",
            &env_lookup(&[("HOME", "/home/ada")]),
            None,
        );
        assert_eq!(
            claude_linux.path,
            Some(
                PathBuf::from("/home/ada")
                    .join(".config")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            )
        );
    }

    #[test]
    fn setup_detection_allows_solo_cli_from_path() {
        let temp_dir =
            std::env::temp_dir().join(format!("solo-tray-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");
        let command_name = solo_command_name();
        let command_path = temp_dir.join(command_name);
        std::fs::write(&command_path, b"").expect("write fake solo command");

        assert!(command_exists_in_paths(
            command_name,
            vec![temp_dir.clone()]
        ));
        assert!(!command_exists_in_paths(
            command_name,
            vec![temp_dir.join("missing")]
        ));

        let _ = std::fs::remove_file(command_path);
        let _ = std::fs::remove_dir(temp_dir);
    }

    #[test]
    fn tool_config_detection_uses_home_paths_for_cursor_and_codex() {
        let cursor = detect_tool_config_path_for_os(
            SetupTarget::Cursor,
            "linux",
            &env_lookup(&[("HOME", "/home/ada")]),
            None,
        );
        assert_eq!(
            cursor.path,
            Some(PathBuf::from("/home/ada").join(".cursor").join("mcp.json"))
        );

        let codex = detect_tool_config_path_for_os(
            SetupTarget::CodexUser,
            "macos",
            &env_lookup(&[("HOME", "/Users/ada")]),
            None,
        );
        assert_eq!(
            codex.path,
            Some(
                PathBuf::from("/Users/ada")
                    .join(".codex")
                    .join("config.toml")
            )
        );

        let project_root = PathBuf::from("/work/solo");
        let codex_project = detect_tool_config_path_for_os(
            SetupTarget::CodexProject,
            "linux",
            &env_lookup(&[]),
            Some(&project_root),
        );
        assert_eq!(
            codex_project.path,
            Some(project_root.join(".codex").join("config.toml"))
        );
    }

    #[test]
    fn tool_config_detection_reports_missing_native_env() {
        let cursor =
            detect_tool_config_path_for_os(SetupTarget::Cursor, "linux", &env_lookup(&[]), None);
        assert_eq!(cursor.path, None);
        assert_eq!(cursor.note, Some("HOME is not set".to_string()));

        let codex = detect_tool_config_path_for_os(
            SetupTarget::CodexUser,
            "windows",
            &env_lookup(&[]),
            None,
        );
        assert_eq!(codex.path, None);
        assert_eq!(codex.note, Some("USERPROFILE is not set".to_string()));

        let codex_project = detect_tool_config_path_for_os(
            SetupTarget::CodexProject,
            "linux",
            &env_lookup(&[]),
            None,
        );
        assert_eq!(codex_project.path, None);
        assert_eq!(
            codex_project.note,
            Some("select a project root in Projects".to_string())
        );
    }

    #[test]
    fn json_tool_config_detects_http_bridge_and_secret_leak() {
        let config = serde_json::json!({
            "mcpServers": {
                "solo": {
                    "command": "npx",
                    "args": ["mcp-remote", "http://127.0.0.1:17821/mcp", "--transport", "http-only"]
                }
            }
        });
        let (state, transport, route, detail) = inspect_json_tool_value(&config);
        assert_eq!(state, ToolConfigState::Verified);
        assert_eq!(transport, ToolTransport::HttpBridge);
        assert_eq!(route, ToolProfileRoute::DaemonDefault);
        assert!(detail.contains("configured"));

        let profiled = serde_json::json!({
            "mcpServers": {
                "solo": {
                    "command": "npx",
                    "args": [
                        "mcp-remote",
                        "http://127.0.0.1:17821/mcp",
                        "--transport",
                        "http-only",
                        "--header",
                        "X-Solo-Tenant:work"
                    ]
                }
            }
        });
        let (_, _, route, detail) = inspect_json_tool_value(&profiled);
        assert_eq!(route, ToolProfileRoute::DaemonDefault);
        assert_eq!(detail, "`mcpServers.solo` is configured");

        let leaked = serde_json::json!({
            "mcpServers": {
                "solo": {
                    "command": "solo",
                    "args": ["mcp-stdio"],
                    "env": { "SOLO_PASSPHRASE": "secret" }
                }
            }
        });
        let (state, _, _, detail) = inspect_json_tool_value(&leaked);
        assert_eq!(state, ToolConfigState::NeedsRepair);
        assert!(detail.contains("SOLO_PASSPHRASE"));

        let leaked_token = serde_json::json!({
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
        let (state, _, _, detail) = inspect_json_tool_value(&leaked_token);
        assert_eq!(state, ToolConfigState::NeedsRepair);
        assert!(detail.contains("Authorization bearer token"));
        assert!(!detail.contains("secret-token"));
    }

    #[test]
    fn toml_tool_config_detects_codex_http_and_stdio() {
        let http: toml::Value = toml::from_str(
            r#"
            [mcp_servers.solo]
            url = "http://127.0.0.1:17821/mcp"
            "#,
        )
        .unwrap();
        let (state, transport, route, detail) = inspect_toml_tool_value(&http);
        assert_eq!(state, ToolConfigState::Verified);
        assert_eq!(transport, ToolTransport::Http);
        assert_eq!(route, ToolProfileRoute::DaemonDefault);
        assert!(detail.contains("HTTP"));

        let http_with_profile: toml::Value = toml::from_str(
            r#"
            [mcp_servers.solo]
            url = "http://127.0.0.1:17821/mcp"

            [mcp_servers.solo.http_headers]
            X-Solo-Tenant = "work"
            "#,
        )
        .unwrap();
        let (_, _, route, detail) = inspect_toml_tool_value(&http_with_profile);
        assert_eq!(route, ToolProfileRoute::DaemonDefault);
        assert_eq!(detail, "`mcp_servers.solo` uses HTTP");

        let stdio: toml::Value = toml::from_str(
            r#"
            [mcp_servers.solo]
            command = "solo"
            args = ["mcp-stdio", "--tenant", "work"]
            "#,
        )
        .unwrap();
        let (state, transport, route, detail) = inspect_toml_tool_value(&stdio);
        assert_eq!(state, ToolConfigState::Verified);
        assert_eq!(transport, ToolTransport::Stdio);
        assert_eq!(route, ToolProfileRoute::DaemonDefault);
        assert!(detail.contains("stdio"));

        let leaked_token: toml::Value = toml::from_str(
            r#"
            [mcp_servers.solo]
            url = "http://127.0.0.1:17821/mcp"

            [mcp_servers.solo.http_headers]
            Authorization = "Bearer secret-token"
            "#,
        )
        .unwrap();
        let (state, _, _, detail) = inspect_toml_tool_value(&leaked_token);
        assert_eq!(state, ToolConfigState::NeedsRepair);
        assert!(detail.contains("Authorization bearer token"));
        assert!(!detail.contains("secret-token"));
    }

    #[test]
    fn setup_target_apply_args_write_real_configs() {
        let mcp_url = "http://127.0.0.1:17821/mcp";

        assert_eq!(
            arg_strings(SetupTarget::ClaudeDesktop.apply_args(mcp_url, None)),
            vec![
                "setup-client",
                "claude-desktop",
                "--transport",
                "http",
                "--url",
                mcp_url,
                "--apply",
            ]
        );
        assert_eq!(
            arg_strings(SetupTarget::Cursor.apply_args(mcp_url, None)),
            vec![
                "setup-client",
                "cursor",
                "--transport",
                "http",
                "--url",
                mcp_url,
                "--apply",
            ]
        );
        assert_eq!(
            arg_strings(SetupTarget::CodexUser.apply_args(mcp_url, None)),
            vec![
                "setup-client",
                "codex",
                "--scope",
                "user",
                "--transport",
                "http",
                "--url",
                mcp_url,
                "--apply",
            ]
        );
        assert_eq!(
            arg_strings(
                SetupTarget::CodexProject.apply_args(mcp_url, Some(Path::new("/work/solo")),)
            ),
            vec![
                "setup-client",
                "codex",
                "--scope",
                "project",
                "--transport",
                "http",
                "--url",
                mcp_url,
                "--apply",
                "--project-dir",
                "/work/solo",
            ]
        );
    }

    #[test]
    fn setup_target_verify_args_are_read_only() {
        assert_eq!(
            arg_strings(SetupTarget::ClaudeDesktop.verify_args(None)),
            vec!["setup-client", "verify", "claude-desktop"]
        );
        assert_eq!(
            arg_strings(SetupTarget::Cursor.verify_args(None)),
            vec!["setup-client", "verify", "cursor"]
        );
        assert_eq!(
            arg_strings(SetupTarget::CodexUser.verify_args(None)),
            vec!["setup-client", "verify", "codex", "--scope", "user"]
        );
        assert_eq!(
            arg_strings(SetupTarget::CodexProject.verify_args(Some(Path::new("/work/solo")))),
            vec![
                "setup-client",
                "verify",
                "codex",
                "--scope",
                "project",
                "--project-dir",
                "/work/solo",
            ]
        );
    }

    #[test]
    fn setup_target_doctor_args_are_structured_and_read_only() {
        let mcp_url = "http://127.0.0.1:17821/mcp";

        assert_eq!(
            arg_strings(SetupTarget::ClaudeDesktop.doctor_args(mcp_url, None)),
            vec![
                "setup-client",
                "doctor",
                "claude-desktop",
                "--url",
                mcp_url,
                "--format",
                "json",
            ]
        );
        assert_eq!(
            arg_strings(SetupTarget::Cursor.doctor_args(mcp_url, None)),
            vec![
                "setup-client",
                "doctor",
                "cursor",
                "--url",
                mcp_url,
                "--format",
                "json",
            ]
        );
        assert_eq!(
            arg_strings(SetupTarget::CodexUser.doctor_args(mcp_url, None)),
            vec![
                "setup-client",
                "doctor",
                "codex",
                "--scope",
                "user",
                "--url",
                mcp_url,
                "--format",
                "json",
            ]
        );
        assert_eq!(
            arg_strings(
                SetupTarget::CodexProject.doctor_args(mcp_url, Some(Path::new("/work/solo")))
            ),
            vec![
                "setup-client",
                "doctor",
                "codex",
                "--scope",
                "project",
                "--url",
                mcp_url,
                "--format",
                "json",
                "--project-dir",
                "/work/solo",
            ]
        );
    }

    #[test]
    fn post_setup_verification_rejects_legacy_explicit_profile_route() {
        let verified_work = ToolVerification {
            state: ToolConfigState::Verified,
            transport: ToolTransport::Http,
            profile_route: ToolProfileRoute::Explicit("work".to_string()),
            detail: "`mcp_servers.solo` uses HTTP".to_string(),
            config_path: Some("/home/ada/.codex/config.toml".to_string()),
        };

        let mismatch = validate_tool_verification(
            SetupTarget::CodexUser,
            &ExpectedToolProfileRoute::DaemonDefault,
            &verified_work,
        )
        .unwrap_err();
        assert!(mismatch.contains("route mismatch"));
        assert!(mismatch.contains("profile `work`"));
    }

    #[test]
    fn post_setup_verification_rejects_unverified_live_config() {
        let needs_repair = ToolVerification {
            state: ToolConfigState::NeedsRepair,
            transport: ToolTransport::Unknown,
            profile_route: ToolProfileRoute::Unknown,
            detail: "malformed TOML".to_string(),
            config_path: Some("/home/ada/.codex/config.toml".to_string()),
        };

        let error = validate_tool_verification(
            SetupTarget::CodexUser,
            &ExpectedToolProfileRoute::Any,
            &needs_repair,
        )
        .unwrap_err();

        assert!(error.contains("live config is Needs repair"));
        assert!(error.contains("malformed TOML"));
    }

    #[test]
    fn setup_success_message_includes_config_route_and_path() {
        let verification = ToolVerification {
            state: ToolConfigState::Verified,
            transport: ToolTransport::HttpBridge,
            profile_route: ToolProfileRoute::DaemonDefault,
            detail: "`mcpServers.solo` is configured".to_string(),
            config_path: Some("/home/ada/.cursor/mcp.json".to_string()),
        };

        let message =
            setup_action_success_message(SetupActionVerb::Apply, &verification, "apply ok");

        assert!(message.contains("setup applied and verified"));
        assert!(message.contains("HTTP bridge"));
        assert!(message.contains("daemon default"));
        assert!(message.contains("/home/ada/.cursor/mcp.json"));
    }

    #[test]
    fn setup_doctor_report_parses_endpoint_and_client_rows() {
        let body = serde_json::json!({
            "profile_route": "Explicit profile: work",
            "mcp_endpoint": {
                "url": "http://127.0.0.1:17821/mcp",
                "status": "auth_required",
                "detail": "endpoint is reachable but requires authorization",
                "http_status": 401,
                "tools": {
                    "tool_count": 20,
                    "missing_required_tools": [],
                },
            },
            "clients": [
                {
                    "client": "codex",
                    "display_name": "Codex",
                    "config_path": "/home/ada/.codex/config.toml",
                    "config_status": "ok",
                    "solo_entry": "installed",
                    "detail": "`mcp_servers.solo` is configured",
                }
            ],
        });

        let report = parse_setup_doctor_report(&serde_json::to_string(&body).unwrap()).unwrap();

        assert_eq!(
            report.profile_route.as_deref(),
            Some("Explicit profile: work")
        );
        assert_eq!(report.endpoint.status, "auth_required");
        assert_eq!(report.endpoint.http_status, Some(401));
        assert_eq!(report.endpoint.tools.as_ref().unwrap().tool_count, 20);
        assert_eq!(report.clients[0].client, "codex");
        assert_eq!(report.clients[0].solo_entry, "installed");
        assert_eq!(
            setup_doctor_endpoint_tone(&report.endpoint.status),
            StateTone::Good
        );
        assert_eq!(
            setup_doctor_client_tone(&report.clients[0]),
            StateTone::Good
        );
        let (tools_text, tools_tone, tools_detail) =
            setup_doctor_tools_status(report.endpoint.tools.as_ref().unwrap());
        assert_eq!(tools_text, "20 listed");
        assert_eq!(tools_tone, StateTone::Good);
        assert!(tools_detail.contains("Critical memory tools"));
        assert!(setup_doctor_client_summary(&report).contains("Codex config ok"));
    }

    #[test]
    fn tool_last_status_reports_route_profile_and_path_detail() {
        let status = ConnectedToolLastStatus {
            status: "applied_verified".to_string(),
            detail: "setup applied and verified".to_string(),
            config_path: Some("/home/ada/.codex/config.toml".to_string()),
            config_state: Some("Verified".to_string()),
            transport: Some("HTTP".to_string()),
            profile_route: Some("profile `work`".to_string()),
            resolved_profile: Some("work".to_string()),
            updated_at_ms: Some(0),
        };

        let label = tool_last_status_label(Some(&status));
        assert!(label.contains("applied verified: work"));

        let detail = tool_last_status_detail(Some(&status));
        assert!(detail.contains("Config: Verified"));
        assert!(detail.contains("Transport: HTTP"));
        assert!(detail.contains("Route: profile `work`"));
        assert!(detail.contains("Resolved profile: work"));
        assert!(detail.contains("Path: /home/ada/.codex/config.toml"));
        assert!(detail.contains("setup applied and verified"));
    }

    #[test]
    fn tool_verification_detail_rows_explain_config_route_and_last_action() {
        let row = ToolConfigRow {
            target: SetupTarget::CodexUser,
            path: Some(PathBuf::from("/home/ada/.codex/config.toml")),
            state: ToolConfigState::Verified,
            transport: ToolTransport::Http,
            profile_route: ToolProfileRoute::Explicit("work".to_string()),
            detail: "`mcp_servers.solo` uses HTTP".to_string(),
            last_status: Some(ConnectedToolLastStatus {
                status: "verified".to_string(),
                detail: "setup verified".to_string(),
                config_path: Some("/home/ada/.codex/config.toml".to_string()),
                config_state: Some("Verified".to_string()),
                transport: Some("HTTP".to_string()),
                profile_route: Some("profile `work`".to_string()),
                resolved_profile: Some("work".to_string()),
                updated_at_ms: Some(0),
            }),
        };

        let rows = tool_verification_detail_rows(&row, "default");

        assert!(rows.iter().any(|(label, value)| {
            *label == "Config" && value.contains("Verified") && value.contains("uses HTTP")
        }));
        assert!(rows.iter().any(|(label, value)| {
            *label == "Profile route" && value.contains("Community library")
        }));
        assert!(rows.iter().any(|(label, value)| {
            *label == "Last action detail" && value.contains("Resolved profile: work")
        }));
        assert!(
            rows.iter()
                .any(|(label, value)| { *label == "Resolved profile" && value == "work" })
        );
    }

    #[test]
    fn connected_tool_status_separates_daemon_probe_from_client_load() {
        let row = ToolConfigRow {
            target: SetupTarget::CodexUser,
            path: Some(PathBuf::from("/home/ada/.codex/config.toml")),
            state: ToolConfigState::Verified,
            transport: ToolTransport::Http,
            profile_route: ToolProfileRoute::DaemonDefault,
            detail: "`mcp_servers.solo` uses HTTP".to_string(),
            last_status: None,
        };

        let (daemon_text, daemon_tone, daemon_detail) = tool_daemon_mcp_status(
            &row,
            DaemonHealth::Healthy,
            &successful_mcp_probe("default"),
            "default",
        );
        assert_eq!(daemon_text, "Tray probe OK");
        assert_eq!(daemon_tone, StateTone::Good);
        assert!(daemon_detail.contains("initialize and tools/list"));

        let (client_text, client_tone, client_detail) =
            tool_client_load_status(&row, &ClientCheckState::Idle);
        assert_eq!(client_text, "Manual smoke");
        assert_eq!(client_tone, StateTone::Warn);
        assert!(client_detail.contains("codex mcp list"));

        let (client_text, client_tone, client_detail) = tool_client_load_status(
            &row,
            &ClientCheckState::Succeeded {
                target: SetupTarget::CodexUser,
                summary: "codex mcp list listed `solo`".to_string(),
                completed_at: std::time::SystemTime::UNIX_EPOCH,
            },
        );
        assert_eq!(client_text, "Client loaded");
        assert_eq!(client_tone, StateTone::Good);
        assert!(client_detail.contains("loaded Solo"));
    }

    #[test]
    fn copyable_setup_commands_never_include_tenant_routing() {
        let commands = setup_client_commands(
            "http://127.0.0.1:17821/v1/status",
            Path::new(r"C:\Users\Example\.solo"),
        );

        assert!(
            commands
                .iter()
                .filter(|(label, _)| {
                    !matches!(*label, "List clients" | "Doctor" | "Claude Code HTTP")
                })
                .all(|(_, command)| command.contains("--dry-run"))
        );
        assert!(commands.iter().any(|(label, command)| {
            *label == "Doctor"
                && command == "solo setup-client doctor --url http://127.0.0.1:17821/mcp"
        }));
        assert!(commands.iter().any(|(label, command)| {
            *label == "Claude Code HTTP"
                && command
                    == "claude mcp add --transport http --scope user solo http://127.0.0.1:17821/mcp"
        }));
        assert!(commands.iter().any(|(label, command)| {
            *label == "Codex HTTP (user)"
                && command.contains("--scope user")
                && command.contains("http://127.0.0.1:17821/mcp")
        }));
        assert!(commands.iter().all(|(_, command)| {
            !command.contains("--tenant") && !command.contains("X-Solo-Tenant")
        }));
    }

    #[test]
    fn setup_urls_are_derived_from_status_url() {
        assert_eq!(
            mcp_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/mcp"
        );
        assert_eq!(
            backup_url_from_status_url("http://localhost:9999/v1/status"),
            "http://localhost:9999/backup"
        );
        assert_eq!(
            mcp_url_from_status_url("http://localhost:9999/status"),
            "http://127.0.0.1:17821/mcp"
        );
    }

    #[test]
    fn library_snapshot_uses_only_root_solo_database() {
        let temp = tempfile::tempdir().expect("create temp data dir");
        let tenants = temp.path().join("tenants");
        std::fs::create_dir_all(&tenants).expect("create tenants dir");
        std::fs::write(tenants.join("work.db"), b"work").expect("write work db");
        std::fs::write(temp.path().join("solo.db"), b"community").expect("write library db");

        let snapshot = collect_library_snapshot(temp.path());

        assert!(snapshot.last_error.is_none());
        assert!(snapshot.exists);
        assert_eq!(snapshot.db_path, temp.path().join("solo.db"));
        assert_eq!(snapshot.size_bytes, Some(b"community".len() as u64));
    }

    #[test]
    fn process_output_summary_is_actionable_and_bounded() {
        assert_eq!(
            summarize_process_output(b"", b""),
            "completed with no output"
        );
        assert_eq!(
            summarize_process_output(b" wrote config\n\n", b" made backup "),
            "wrote config | made backup"
        );

        let long_stdout = "x".repeat(300);
        let summary = summarize_process_output(long_stdout.as_bytes(), b"");
        assert!(summary.ends_with("..."));
        assert!(summary.chars().count() <= 243);

        let display = display_process_output(
            b"project ingest --dry-run\ncandidate: README.md\n",
            b"warning",
        );
        assert!(display.contains("candidate: README.md"));
        assert!(display.contains("stderr:\nwarning"));

        let long_display = display_process_output("x".repeat(4_100).as_bytes(), b"");
        assert!(long_display.ends_with("\n..."));
        assert!(long_display.chars().count() <= 4_004);
    }

    #[test]
    fn solo_command_capture_times_out_hung_child() {
        let (bin, args) = slow_shell_command_for_tests();
        let err =
            run_solo_command_capture_with_timeout(&bin, args, std::time::Duration::from_millis(75))
                .expect_err("slow child should time out");

        assert!(err.contains("timed out"));
    }

    fn slow_shell_command_for_tests() -> (PathBuf, Vec<std::ffi::OsString>) {
        #[cfg(windows)]
        {
            (
                PathBuf::from("powershell.exe"),
                vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    "Start-Sleep -Seconds 5".into(),
                ],
            )
        }
        #[cfg(not(windows))]
        {
            (PathBuf::from("sh"), vec!["-c".into(), "sleep 5".into()])
        }
    }
}
