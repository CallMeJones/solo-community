// SPDX-License-Identifier: Apache-2.0

//! Managed client hook installation. Prepare every file before changing any,
//! preserve other hooks, back up changes, and undo partial multi-file writes.

use super::*;
use crate::commands::agent_hook::{HookClient, HookEvent, validate_url};
use base64::{Engine, engine::general_purpose::STANDARD};

const MARKER: &str = "solo-setup-v1";

#[derive(Debug, Args)]
pub struct HookSetupArgs {
    /// Configure MCP only and remove hooks previously installed by Solo.
    #[arg(long)]
    pub no_hooks: bool,
    /// Environment variable name for hook authentication; never the token itself.
    #[arg(long, default_value = "SOLO_BEARER_TOKEN")]
    pub hook_token_env: String,
}

fn hook_client(client: Client) -> Option<HookClient> {
    match client {
        Client::ClaudeCode => Some(HookClient::ClaudeCode),
        Client::Codex => Some(HookClient::Codex),
        Client::Cursor => Some(HookClient::Cursor),
        Client::Copilot => Some(HookClient::Copilot),
        Client::Gemini => Some(HookClient::Gemini),
        Client::ClaudeDesktop => None,
    }
}

fn hook_path<F>(
    client: Client,
    mcp_path: &Path,
    os: OperatingSystem,
    env_var: &F,
) -> Result<Option<PathBuf>>
where
    F: Fn(&str) -> Option<OsString>,
{
    Ok(match client {
        Client::ClaudeDesktop => None,
        Client::Codex | Client::Cursor => Some(mcp_path.with_file_name("hooks.json")),
        Client::Gemini => Some(mcp_path.to_path_buf()),
        Client::ClaudeCode => Some(
            env_var("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| home_dir(os, env_var).map(|p| p.join(".claude")))
                .context("could not detect Claude settings directory")?
                .join("settings.json"),
        ),
        Client::Copilot => Some(
            home_dir(os, env_var)
                .context("could not detect Copilot hook directory")?
                .join(".copilot/hooks/solo-memory.json"),
        ),
    })
}

pub(super) fn setup<F>(
    client: Client,
    mcp_path: &Path,
    transport: Transport,
    url: &str,
    data_dir: Option<&Path>,
    options: &HookSetupArgs,
    apply: bool,
    os: OperatingSystem,
    env_var: &F,
) -> Result<()>
where
    F: Fn(&str) -> Option<OsString>,
{
    if transport == Transport::Http {
        validate_url(url)?;
    }
    if options.hook_token_env.is_empty()
        || !options
            .hook_token_env
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        || options
            .hook_token_env
            .starts_with(|c: char| c.is_ascii_digit())
    {
        bail!("--hook-token-env must be an environment variable name, not a credential");
    }
    let exe = env::current_exe().context("locate Solo executable")?;
    let enabled = !options.no_hooks && transport == Transport::Http;
    let changes = plan(
        client, mcp_path, transport, url, data_dir, options, &exe, os, env_var,
    )?;
    println!("Client      : {}", client.display_name());
    println!("Mode        : {}", if apply { "apply" } else { "dry-run" });
    println!("Transport   : {}", transport.label());
    println!("Memory hooks: {}", coverage(client, enabled));
    if hook_client(client).is_some() && enabled {
        println!("Hook runner : {}", exe.display());
        println!(
            "Hook auth   : environment variable {} (value is never saved)",
            options.hook_token_env
        );
    }
    for change in &changes {
        println!(
            "{}: {}",
            if change.before.as_deref() == Some(change.after.as_slice()) {
                "Unchanged"
            } else if apply {
                "Update"
            } else {
                "Would update"
            },
            change.path.display()
        );
    }
    if apply {
        apply_changes(&changes)?;
        println!("Setup complete. Existing changed files were backed up.");
    } else {
        println!(
            "No files were written. Use --apply to install the MCP connection and memory hooks."
        );
    }
    println!("{}", activation_note(client));
    if transport == Transport::Stdio {
        println!(
            "Automatic recall requires a running HTTP daemon. Stdio config is preserved; Solo-managed HTTP hooks are removed."
        );
    }
    Ok(())
}

fn coverage(client: Client, enabled: bool) -> &'static str {
    if client == Client::ClaudeDesktop {
        "not supported by ordinary Claude Desktop chat; MCP tools remain available"
    } else if !enabled {
        "disabled; removes only Solo-managed hooks"
    } else {
        match client {
            Client::Cursor => "session start (asynchronous); use MCP for topic-specific recall",
            Client::Copilot => "session start in VS Code Copilot and Local agent sessions",
            _ => "session start and before each user prompt",
        }
    }
}

fn activation_note(client: Client) -> &'static str {
    match client {
        Client::Codex => {
            "Restart Codex and review/trust the Solo hooks in /hooks. Setup never bypasses client trust."
        }
        Client::ClaudeCode => {
            "Restart Claude Code. Review any client trust request; disabled hooks and built-in memory settings are preserved."
        }
        Client::Gemini => {
            "Restart Gemini CLI. Review any hook trust request; hooksConfig.enabled is preserved."
        }
        Client::Cursor => {
            "Restart Cursor. Session-start context is asynchronous; it is not guaranteed to precede the first answer."
        }
        Client::Copilot => {
            "Restart VS Code and use a Copilot or Local agent session with hooks enabled. Review any client trust request."
        }
        Client::ClaudeDesktop => {
            "Restart Claude Desktop and enable Solo. Automatic context hooks are not available in ordinary chat."
        }
    }
}

#[derive(Debug)]
struct Change {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            bail!("config must be a regular file: {}", path.display())
        }
        Ok(_) => Ok(Some(
            fs::read(path).with_context(|| format!("read {}", path.display()))?,
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspect {}", path.display())),
    }
}

fn parse_json(bytes: Option<&[u8]>) -> Result<Value> {
    let json = match bytes {
        Some(bytes) => {
            serde_json::from_slice(bytes).context("invalid existing JSON; no files changed")?
        }
        None => json!({}),
    };
    if !json.is_object() {
        bail!("existing config root is not a JSON object; no files changed");
    }
    Ok(json)
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn plan<F>(
    client: Client,
    mcp_path: &Path,
    transport: Transport,
    url: &str,
    data_dir: Option<&Path>,
    options: &HookSetupArgs,
    exe: &Path,
    os: OperatingSystem,
    env_var: &F,
) -> Result<Vec<Change>>
where
    F: Fn(&str) -> Option<OsString>,
{
    let before = read_optional(mcp_path)?;
    let after = if client == Client::Codex {
        let existing = before
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()?
            .map(str::parse::<DocumentMut>)
            .transpose()?;
        if let Some(doc) = &existing {
            ensure_mergeable_codex_config(doc)?;
        }
        // Retain user-owned server options (auth references, approval policy,
        // timeouts). Only connection fields belong to this installer.
        let extras = existing
            .as_ref()
            .and_then(|d| d.get("mcp_servers"))
            .and_then(|i| i.get("solo"))
            .and_then(Item::as_table)
            .cloned();
        let mut doc = preview_codex_config(existing, transport, url, data_dir);
        if let Some(extras) = extras {
            let table = doc["mcp_servers"]["solo"]
                .as_table_mut()
                .context("Solo table")?;
            for (key, value) in extras.iter() {
                if !["url", "command", "args"].contains(&key) {
                    table.insert(key, value.clone());
                }
            }
        }
        doc.to_string().into_bytes()
    } else {
        let existing = parse_json(before.as_deref())?;
        ensure_mergeable_config(&existing, client.config_style())?;
        let key = client.config_style().root_key();
        let extras = existing
            .get(key)
            .and_then(|v| v.get("solo"))
            .and_then(Value::as_object)
            .cloned();
        let mut doc = preview_config(
            Some(existing),
            client.config_style(),
            transport,
            url,
            data_dir,
        );
        if let Some(extras) = extras {
            let entry = doc[key]["solo"]
                .as_object_mut()
                .context("Solo server entry")?;
            for (key, value) in extras {
                if !["type", "url", "httpUrl", "command", "args"].contains(&key.as_str()) {
                    entry.insert(key, value);
                }
            }
        }
        json_bytes(&doc)?
    };
    let mut changes = vec![Change {
        path: mcp_path.to_path_buf(),
        before,
        after,
    }];
    if let (Some(hook_client), Some(path)) = (
        hook_client(client),
        hook_path(client, mcp_path, os, env_var)?,
    ) {
        let enabled = !options.no_hooks && transport == Transport::Http;
        let same_file = path == mcp_path;
        let before = if same_file {
            changes[0].before.clone()
        } else {
            read_optional(&path)?
        };
        // Disabling a never-installed integration should not create an empty hook file.
        if enabled || before.is_some() {
            let mut doc = parse_json(if same_file {
                Some(&changes[0].after)
            } else {
                before.as_deref()
            })?;
            merge_hooks(
                &mut doc,
                client,
                hook_client,
                exe,
                url,
                &options.hook_token_env,
                os,
                enabled,
            )?;
            let after = json_bytes(&doc)?;
            if same_file {
                changes[0].after = after;
            } else {
                changes.push(Change {
                    path,
                    before,
                    after,
                });
            }
        }
    }
    Ok(changes)
}

fn merge_hooks(
    doc: &mut Value,
    client: Client,
    hook_client: HookClient,
    exe: &Path,
    url: &str,
    token_env: &str,
    os: OperatingSystem,
    enabled: bool,
) -> Result<()> {
    if !enabled && doc.get("hooks").is_none() {
        return Ok(());
    }
    if enabled && matches!(client, Client::Cursor | Client::Copilot) {
        if doc.get("version").is_some_and(|v| v != &json!(1)) {
            bail!(
                "unsupported {} hooks version; no files changed",
                client.display_name()
            );
        }
        doc["version"] = json!(1);
    }
    let root = doc.as_object_mut().context("hook config root")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing hooks must be an object; no files changed")?;
    // Remove only handlers carrying our versioned marker. Preserve siblings,
    // matchers and all unrelated hook events, including user-disabled settings.
    for entries in hooks.values_mut() {
        let entries = entries
            .as_array_mut()
            .context("existing hook event must be an array")?;
        for entry in entries.iter() {
            if entry.get("hooks").is_some_and(|h| !h.is_array()) {
                bail!("existing hook group must contain an array");
            }
        }
        entries.retain_mut(|entry| {
            if owned_handler(entry, client) {
                return false;
            }
            if let Some(handlers) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = handlers.len();
                handlers.retain(|handler| !owned_handler(handler, client));
                // An unrelated empty group belongs to the user too.
                return before == handlers.len() || !handlers.is_empty();
            }
            true
        });
    }
    if enabled {
        for event in [HookEvent::SessionStart, HookEvent::Prompt] {
            let Some(name) = hook_client.event_name(event) else {
                continue;
            };
            let command = hook_command(exe, client, event, url, token_env, os);
            let handler = if client == Client::Cursor {
                json!({"command": command})
            } else {
                json!({"type": "command", "command": command, "timeout": if client == Client::Gemini { 10000 } else { 10 }})
            };
            let entry = if matches!(client, Client::Cursor | Client::Copilot) {
                handler
            } else {
                json!({"hooks": [handler]})
            };
            hooks
                .entry(name)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .context("hook event must be an array")?
                .push(entry);
        }
    }
    Ok(())
}

fn hook_command(
    exe: &Path,
    client: Client,
    event: HookEvent,
    url: &str,
    token_env: &str,
    os: OperatingSystem,
) -> String {
    let event = match event {
        HookEvent::SessionStart => "session-start",
        HookEvent::Prompt => "prompt",
    };
    let exe = exe
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();
    let parts = [
        exe.as_str(),
        "agent-hook",
        "--managed-by",
        MARKER,
        "--client",
        client.cli_name(),
        "--event",
        event,
        "--url",
        url,
        "--token-env",
        token_env,
    ];
    if os == OperatingSystem::Windows {
        // Encoded PowerShell is safe from both cmd.exe and Git Bash expansion;
        // URLs, Unicode paths, apostrophes and shell metacharacters stay literal.
        let script = format!(
            "& {}",
            parts
                .iter()
                .map(|p| format!("'{}'", p.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        format!(
            "powershell.exe -NoProfile -NonInteractive -EncodedCommand {}",
            STANDARD.encode(bytes)
        )
    } else {
        parts
            .iter()
            .map(|p| format!("'{}'", p.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn command_text(command: &str) -> Option<String> {
    if let Some(encoded) =
        command.strip_prefix("powershell.exe -NoProfile -NonInteractive -EncodedCommand ")
    {
        let bytes = STANDARD.decode(encoded).ok()?;
        if bytes.len() % 2 != 0 {
            return None;
        }
        let words: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        String::from_utf16(&words).ok()
    } else {
        Some(command.to_string())
    }
}

fn owned_handler(handler: &Value, client: Client) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .and_then(command_text)
        .is_some_and(|text| {
            text.contains(&format!(
                "'agent-hook' '--managed-by' '{MARKER}' '--client' '{}'",
                client.cli_name()
            ))
        })
}

fn apply_changes(changes: &[Change]) -> Result<()> {
    apply_changes_with_writer(changes, write_bytes_atomic)
}

fn apply_changes_with_writer(
    changes: &[Change],
    mut write: impl FnMut(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    let changed: Vec<&Change> = changes
        .iter()
        .filter(|c| c.before.as_deref() != Some(c.after.as_slice()))
        .collect();
    // Detect concurrent edits and prepare backups before touching any file.
    for change in &changed {
        if read_optional(&change.path)? != change.before {
            bail!(
                "config changed during setup; retry: {}",
                change.path.display()
            );
        }
        fs::create_dir_all(config_parent(&change.path)?)?;
        if change.before.is_some() {
            create_backup(&change.path)?;
        }
    }
    for (written, change) in changed.iter().enumerate() {
        let result = (|| {
            // Another client may save settings while backups are being made.
            if read_optional(&change.path)? != change.before {
                bail!("config changed during setup: {}", change.path.display());
            }
            write(&change.path, &change.after)
        })();
        if let Err(error) = result {
            let mut restore_errors = Vec::new();
            for prior in changed[..written].iter().rev() {
                let result = (|| {
                    // Do not roll back a newer edit made after our replacement.
                    if read_optional(&prior.path)?.as_deref() != Some(prior.after.as_slice()) {
                        bail!("config changed after setup wrote it");
                    }
                    match &prior.before {
                        Some(bytes) => write(&prior.path, bytes),
                        None => fs::remove_file(&prior.path).map_err(anyhow::Error::from),
                    }
                })();
                if result.is_err() {
                    restore_errors.push(prior.path.display().to_string());
                }
            }
            if !restore_errors.is_empty() {
                bail!(
                    "setup failed: {error}; rollback incomplete; review current files before restoring backups: {}",
                    restore_errors.join(", ")
                );
            }
            return Err(error).context("setup failed; earlier changes were restored");
        }
    }
    Ok(())
}

pub(super) fn status<F>(
    client: Client,
    mcp_path: Option<&Path>,
    os: OperatingSystem,
    env_var: &F,
) -> Value
where
    F: Fn(&str) -> Option<OsString>,
{
    let inspect = || -> Result<Value> {
        if hook_client(client).is_none() {
            return Ok(json!({"status":"unsupported","detail":coverage(client, false)}));
        }
        let path = hook_path(
            client,
            mcp_path.context("MCP path unavailable")?,
            os,
            env_var,
        )?
        .context("hook path unavailable")?;
        let Some(bytes) = read_optional(&path)? else {
            return Ok(
                json!({"status":"missing","path":path,"detail":"Run setup-client for this client to install memory hooks."}),
            );
        };
        let doc = parse_json(Some(&bytes))?;
        let hooks = doc
            .get("hooks")
            .and_then(Value::as_object)
            .context("hooks missing or invalid")?;
        let adapter = hook_client(client).context("adapter")?;
        let installed = [HookEvent::SessionStart, HookEvent::Prompt]
            .iter()
            .filter_map(|&event| adapter.event_name(event))
            .all(|event| {
                hooks
                    .get(event)
                    .and_then(Value::as_array)
                    .is_some_and(|entries| {
                        entries.iter().any(|entry| {
                            owned_handler(entry, client)
                                || entry
                                    .get("hooks")
                                    .and_then(Value::as_array)
                                    .is_some_and(|hs| hs.iter().any(|h| owned_handler(h, client)))
                        })
                    })
            });
        let disabled = doc.get("disableAllHooks") == Some(&json!(true))
            || doc.pointer("/hooksConfig/enabled") == Some(&json!(false));
        Ok(json!({
            "status": if !installed { "missing" } else if disabled { "disabled_by_client" } else { "installed" },
            "path":path, "coverage":coverage(client,true),
            "activation":activation_note(client),
            "runtime_verified":false
        }))
    };
    inspect().unwrap_or_else(|_| json!({"status":"invalid","detail":"Cannot read a valid hook configuration; rerun setup after correcting the file."}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(disabled: bool) -> HookSetupArgs {
        HookSetupArgs {
            no_hooks: disabled,
            hook_token_env: "SOLO_BEARER_TOKEN".to_string(),
        }
    }

    #[test]
    fn all_adapters_install_preserve_and_remove_only_owned_hooks() {
        for os in [OperatingSystem::Windows, OperatingSystem::Linux] {
            for client in [
                Client::ClaudeCode,
                Client::Codex,
                Client::Cursor,
                Client::Copilot,
                Client::Gemini,
            ] {
                let mut doc = json!({"setting":"keep","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"user-custom-hook"}]}]}});
                let adapter = hook_client(client).unwrap();
                merge_hooks(
                    &mut doc,
                    client,
                    adapter,
                    Path::new("/path with space/solo"),
                    "http://localhost/mcp",
                    "TOKEN",
                    os,
                    true,
                )
                .unwrap();
                let first = doc.clone();
                merge_hooks(
                    &mut doc,
                    client,
                    adapter,
                    Path::new("/path with space/solo"),
                    "http://localhost/mcp",
                    "TOKEN",
                    os,
                    true,
                )
                .unwrap();
                assert_eq!(doc, first, "idempotent {client:?} {os:?}");
                assert_eq!(doc["setting"], "keep");
                merge_hooks(
                    &mut doc,
                    client,
                    adapter,
                    Path::new("/new/solo"),
                    "http://localhost:2/mcp",
                    "TOKEN",
                    os,
                    false,
                )
                .unwrap();
                assert_eq!(
                    doc["hooks"]["SessionStart"][0]["hooks"][0]["command"],
                    "user-custom-hook"
                );
                assert!(!doc.to_string().contains(MARKER));
                let count: usize = doc["hooks"]
                    .as_object()
                    .unwrap()
                    .values()
                    .map(|v| v.as_array().unwrap().len())
                    .sum();
                assert_eq!(count, 1);
            }
        }
    }

    #[test]
    fn copilot_setup_supports_agent_host_and_preserves_local_handlers() {
        let mut doc = json!({"hooks":{"SessionStart":[{"type":"command","command":"user-hook"}]}});
        merge_hooks(
            &mut doc,
            Client::Copilot,
            HookClient::Copilot,
            Path::new("/usr/bin/solo"),
            DEFAULT_MCP_URL,
            "TOKEN",
            OperatingSystem::Linux,
            true,
        )
        .unwrap();
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["hooks"]["SessionStart"][0]["command"], "user-hook");
        let handler = &doc["hooks"]["SessionStart"][1];
        assert_eq!(handler["type"], "command");
        assert_eq!(handler["timeout"], 10);
        assert!(owned_handler(handler, Client::Copilot));
        doc["version"] = json!(2);
        let before = doc.clone();
        assert!(
            merge_hooks(
                &mut doc,
                Client::Copilot,
                HookClient::Copilot,
                Path::new("/usr/bin/solo"),
                DEFAULT_MCP_URL,
                "TOKEN",
                OperatingSystem::Linux,
                true
            )
            .is_err()
        );
        assert_eq!(doc, before);
    }

    #[test]
    fn setup_is_atomic_on_invalid_hooks_and_handles_shared_gemini_file() {
        let temp = tempfile::tempdir().unwrap();
        let lookup = |key: &str| match key {
            "HOME" | "USERPROFILE" => Some(temp.path().as_os_str().to_owned()),
            _ => None,
        };
        let mcp = temp.path().join(".gemini/settings.json");
        fs::create_dir_all(mcp.parent().unwrap()).unwrap();
        fs::write(&mcp, r#"{"theme":"custom","mcpServers":{"other":{"command":"keep"}},"hooksConfig":{"enabled":false}}"#).unwrap();
        let plans = plan(
            Client::Gemini,
            &mcp,
            Transport::Http,
            DEFAULT_MCP_URL,
            None,
            &options(false),
            Path::new("/usr/bin/solo"),
            OperatingSystem::Linux,
            &lookup,
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        apply_changes(&plans).unwrap();
        let doc: Value = serde_json::from_slice(&fs::read(&mcp).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["solo"]["httpUrl"], DEFAULT_MCP_URL);
        assert_eq!(doc["mcpServers"]["other"]["command"], "keep");
        assert_eq!(doc["hooksConfig"]["enabled"], false);
        assert_eq!(
            status(Client::Gemini, Some(&mcp), OperatingSystem::Linux, &lookup)["status"],
            "disabled_by_client"
        );
        let second = plan(
            Client::Gemini,
            &mcp,
            Transport::Http,
            DEFAULT_MCP_URL,
            None,
            &options(false),
            Path::new("/usr/bin/solo"),
            OperatingSystem::Linux,
            &lookup,
        )
        .unwrap();
        assert_eq!(second[0].before.as_ref().unwrap(), &second[0].after);
        let codex = temp.path().join(".codex/config.toml");
        fs::create_dir_all(codex.parent().unwrap()).unwrap();
        fs::write(codex.with_file_name("hooks.json"), "{broken").unwrap();
        assert!(
            plan(
                Client::Codex,
                &codex,
                Transport::Http,
                DEFAULT_MCP_URL,
                None,
                &options(false),
                Path::new("/usr/bin/solo"),
                OperatingSystem::Linux,
                &lookup
            )
            .is_err()
        );
        assert!(!codex.exists());
    }

    #[test]
    fn shell_metacharacters_are_literal_and_marker_is_recognizable() {
        let command = hook_command(
            Path::new("C:\\Some O'Brien\\$danger\\solo.exe"),
            Client::ClaudeCode,
            HookEvent::Prompt,
            "https://example.com/mcp?a=$x&b=1",
            "TOKEN",
            OperatingSystem::Windows,
        );
        let text = command_text(&command).unwrap();
        assert!(text.starts_with("& 'C:\\Some O''Brien\\$danger\\solo.exe'"));
        assert!(text.contains("'https://example.com/mcp?a=$x&b=1'"));
        assert!(!command.contains('$'));
        assert!(owned_handler(
            &json!({"command":command}),
            Client::ClaudeCode
        ));
        assert!(!owned_handler(
            &json!({"command":"echo solo-setup-v1"}),
            Client::ClaudeCode
        ));
        assert!(!owned_handler(&json!({"command":command}), Client::Codex));
    }

    #[test]
    fn failed_second_write_restores_the_first_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.json");
        fs::write(&path, b"original").unwrap();
        let changes = [
            Change {
                path: path.clone(),
                before: Some(b"original".to_vec()),
                after: b"updated".to_vec(),
            },
            Change {
                path: temp.path().join("hooks.json"),
                before: None,
                after: b"hooks".to_vec(),
            },
        ];
        let mut calls = 0;
        let result = apply_changes_with_writer(&changes, |path, bytes| {
            calls += 1;
            if calls == 2 {
                bail!("simulated disk failure");
            }
            write_bytes_atomic(path, bytes)
        });
        assert!(result.is_err());
        assert_eq!(calls, 3);
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert!(!changes[1].path.exists());
    }

    #[test]
    fn empty_user_groups_and_disabled_settings_survive_installation() {
        let mut doc = json!({"disableAllHooks":true,"hooks":{"SessionStart":[{"matcher":"custom","hooks":[]}]}});
        for enabled in [true, false] {
            merge_hooks(
                &mut doc,
                Client::ClaudeCode,
                HookClient::ClaudeCode,
                Path::new("/usr/bin/solo"),
                DEFAULT_MCP_URL,
                "TOKEN",
                OperatingSystem::Linux,
                enabled,
            )
            .unwrap();
            assert_eq!(doc["disableAllHooks"], true);
            assert_eq!(
                doc["hooks"]["SessionStart"][0],
                json!({"matcher":"custom","hooks":[]})
            );
        }
    }

    #[test]
    fn concurrent_edits_are_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(&path, b"original").unwrap();
        let changes = [Change {
            path: path.clone(),
            before: Some(b"original".to_vec()),
            after: b"new".to_vec(),
        }];
        fs::write(&path, b"concurrent edit").unwrap();
        assert!(apply_changes(&changes).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"concurrent edit");
    }

    #[test]
    fn edits_between_writes_survive_failure_and_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("mcp.json");
        let second = temp.path().join("hooks.json");
        for path in [&first, &second] {
            fs::write(path, b"original").unwrap();
        }
        let changes: Vec<_> = [&first, &second]
            .into_iter()
            .map(|path| Change {
                path: path.clone(),
                before: Some(b"original".to_vec()),
                after: b"setup".to_vec(),
            })
            .collect();
        let result = apply_changes_with_writer(&changes, |path, bytes| {
            write_bytes_atomic(path, bytes)?;
            // Simulate the assistant saving both files after our first write.
            fs::write(&first, b"new user setting")?;
            fs::write(&second, b"new user hook")?;
            Ok(())
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("rollback incomplete")
        );
        assert_eq!(fs::read(first).unwrap(), b"new user setting");
        assert_eq!(fs::read(second).unwrap(), b"new user hook");
    }

    #[test]
    fn stdio_does_not_install_a_hook_for_an_unrelated_default_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let changes = plan(
            Client::Codex,
            &path,
            Transport::Stdio,
            DEFAULT_MCP_URL,
            Some(Path::new("/private/library")),
            &options(false),
            Path::new("/usr/bin/solo"),
            OperatingSystem::Linux,
            &|_| None,
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert!(!String::from_utf8_lossy(&changes[0].after).contains("agent-hook"));
    }
}
