// SPDX-License-Identifier: Apache-2.0

//! Schema-aware importers for external app exports.
//!
//! The CLI and daemon both use this module so ChatGPT, Claude, and
//! bookmarks exports are parsed into the same stable Markdown records before
//! they enter the normal document-ingest path.

use anyhow::{Context, Result, bail};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const ESTIMATED_BYTES_PER_TOKEN: u64 = 4;
const MATERIALIZED_IMPORT_EXTENSION: &str = "md";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaImportSource {
    ChatGpt,
    Claude,
    Bookmarks,
}

impl SchemaImportSource {
    pub fn command_name(self) -> &'static str {
        match self {
            Self::ChatGpt => "chatgpt",
            Self::Claude => "claude",
            Self::Bookmarks => "bookmarks",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ChatGpt => "ChatGPT",
            Self::Claude => "Claude",
            Self::Bookmarks => "Bookmarks",
        }
    }

    pub fn materialized_dir(self) -> &'static str {
        match self {
            Self::ChatGpt => "chatgpt",
            Self::Claude => "claude",
            Self::Bookmarks => "bookmarks",
        }
    }

    pub fn no_records_message(self) -> &'static str {
        match self {
            Self::ChatGpt => "no ChatGPT conversations were importable",
            Self::Claude => "no Claude conversations were importable",
            Self::Bookmarks => "no bookmarks were importable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaImportScan {
    pub records_scanned: u64,
    pub filtered_records: u64,
    pub skipped_records: u64,
    pub records: Vec<ImportRecord>,
}

impl SchemaImportScan {
    fn new() -> Self {
        Self {
            records_scanned: 0,
            filtered_records: 0,
            skipped_records: 0,
            records: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRecord {
    pub source_id: String,
    pub title: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptMessage {
    role: String,
    text: String,
    created_at: Option<String>,
    order: usize,
}

pub fn parse_schema_import(
    path: &Path,
    source: SchemaImportSource,
    filters: &[String],
) -> Result<SchemaImportScan> {
    match source {
        SchemaImportSource::ChatGpt => parse_chatgpt_import(path, filters),
        SchemaImportSource::Claude => parse_claude_import(path, filters),
        SchemaImportSource::Bookmarks => parse_bookmarks_import(path),
    }
}

pub fn estimate_schema_chunks(records: &[ImportRecord], chunk_token_target: u32) -> u64 {
    records
        .iter()
        .map(|record| estimate_chunks(record.body.len() as u64, chunk_token_target))
        .sum()
}

pub fn materialized_schema_record_path(dir: &Path, record: &ImportRecord) -> PathBuf {
    let filename = format!(
        "{}-{}.{}",
        slugify_filename(&record.title),
        &stable_hash(&record.source_id)[..12],
        MATERIALIZED_IMPORT_EXTENSION
    );
    dir.join(filename)
}

pub fn materialize_schema_record(dir: &Path, record: &ImportRecord) -> Result<PathBuf> {
    let path = materialized_schema_record_path(dir, record);
    std::fs::write(&path, &record.body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn parse_chatgpt_import(path: &Path, filters: &[String]) -> Result<SchemaImportScan> {
    let value = read_json_export(path, "conversations.json")?;
    let conversations = conversation_array(&value)
        .context("ChatGPT export must be an array or contain a conversations array")?;
    let mut scan = SchemaImportScan::new();

    for conversation in conversations {
        scan.records_scanned += 1;
        let id = string_field(conversation, &["id", "uuid"])
            .map(str::to_string)
            .unwrap_or_else(|| stable_hash(&conversation.to_string()));
        let title = string_field(conversation, &["title", "name"])
            .map(clean_title)
            .unwrap_or_else(|| "Untitled ChatGPT conversation".to_string());
        if !matches_schema_filters(&id, &title, filters) {
            scan.filtered_records += 1;
            continue;
        }
        let messages = extract_chatgpt_messages(conversation);
        if messages.is_empty() {
            scan.skipped_records += 1;
            continue;
        }
        let record = transcript_record(
            SchemaImportSource::ChatGpt,
            id,
            title,
            string_field(conversation, &["create_time", "created_at"]).map(str::to_string),
            string_field(conversation, &["update_time", "updated_at"]).map(str::to_string),
            messages,
        );
        scan.records.push(record);
    }
    Ok(scan)
}

fn parse_claude_import(path: &Path, filters: &[String]) -> Result<SchemaImportScan> {
    let value = read_json_export(path, "conversations.json")?;
    let conversations = conversation_array(&value)
        .context("Claude export must be an array or contain a conversations array")?;
    let mut scan = SchemaImportScan::new();

    for conversation in conversations {
        scan.records_scanned += 1;
        let id = string_field(conversation, &["uuid", "id"])
            .map(str::to_string)
            .unwrap_or_else(|| stable_hash(&conversation.to_string()));
        let title = string_field(conversation, &["name", "title"])
            .map(clean_title)
            .unwrap_or_else(|| "Untitled Claude conversation".to_string());
        if !matches_schema_filters(&id, &title, filters) {
            scan.filtered_records += 1;
            continue;
        }
        let messages = extract_claude_messages(conversation);
        if messages.is_empty() {
            scan.skipped_records += 1;
            continue;
        }
        let record = transcript_record(
            SchemaImportSource::Claude,
            id,
            title,
            string_field(conversation, &["created_at", "create_time"]).map(str::to_string),
            string_field(conversation, &["updated_at", "update_time"]).map(str::to_string),
            messages,
        );
        scan.records.push(record);
    }
    Ok(scan)
}

fn parse_bookmarks_import(path: &Path) -> Result<SchemaImportScan> {
    if path.is_dir() {
        bail!("bookmarks import expects a browser bookmarks export file");
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let records = if path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
    {
        let value: serde_json::Value = serde_json::from_str(strip_json_bom(&raw))
            .with_context(|| format!("parse {}", path.display()))?;
        parse_bookmarks_json(&value)
    } else {
        parse_bookmarks_html(&raw)?
    };

    let mut scan = SchemaImportScan::new();
    scan.records_scanned = records.len() as u64;
    for bookmark in records {
        if bookmark.url.trim().is_empty() {
            scan.skipped_records += 1;
            continue;
        }
        scan.records.push(bookmark_record(bookmark));
    }
    Ok(scan)
}

fn read_json_export(path: &Path, default_file_name: &str) -> Result<serde_json::Value> {
    let file = if path.is_dir() {
        let candidate = path.join(default_file_name);
        if candidate.is_file() {
            candidate
        } else {
            bail!(
                "{} export directory must contain {}",
                path.display(),
                default_file_name
            );
        }
    } else {
        path.to_path_buf()
    };
    let raw = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    serde_json::from_str(strip_json_bom(&raw))
        .with_context(|| format!("parse JSON {}", file.display()))
}

fn strip_json_bom(raw: &str) -> &str {
    raw.strip_prefix('\u{feff}').unwrap_or(raw)
}

fn conversation_array(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    value.as_array().or_else(|| {
        value
            .get("conversations")
            .and_then(serde_json::Value::as_array)
    })
}

fn extract_chatgpt_messages(conversation: &serde_json::Value) -> Vec<TranscriptMessage> {
    if let Some(mapping) = conversation
        .get("mapping")
        .and_then(serde_json::Value::as_object)
    {
        let mut messages = Vec::new();
        for (order, node) in mapping.values().enumerate() {
            let Some(message) = node.get("message") else {
                continue;
            };
            if message.is_null() {
                continue;
            }
            let role = message
                .pointer("/author/role")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let text = message_text(message.get("content").unwrap_or(&serde_json::Value::Null));
            if text.trim().is_empty() {
                continue;
            }
            messages.push(TranscriptMessage {
                role: normalize_role(role),
                text,
                created_at: value_timestamp(message.get("create_time")),
                order,
            });
        }
        messages.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.order.cmp(&b.order))
        });
        return messages;
    }

    extract_generic_messages(conversation)
}

fn extract_claude_messages(conversation: &serde_json::Value) -> Vec<TranscriptMessage> {
    let messages = conversation
        .get("chat_messages")
        .or_else(|| conversation.get("messages"))
        .and_then(serde_json::Value::as_array);
    let Some(messages) = messages else {
        return Vec::new();
    };

    messages
        .iter()
        .enumerate()
        .filter_map(|(order, message)| {
            let role = string_field(message, &["sender", "role", "author"]).unwrap_or("unknown");
            let text = string_field(message, &["text", "content"])
                .map(str::to_string)
                .unwrap_or_else(|| message_text(message.get("content").unwrap_or(message)));
            if text.trim().is_empty() {
                return None;
            }
            Some(TranscriptMessage {
                role: normalize_role(role),
                text,
                created_at: string_field(message, &["created_at", "create_time"])
                    .map(str::to_string)
                    .or_else(|| value_timestamp(message.get("created_at"))),
                order,
            })
        })
        .collect()
}

fn extract_generic_messages(conversation: &serde_json::Value) -> Vec<TranscriptMessage> {
    let Some(messages) = conversation
        .get("messages")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    messages
        .iter()
        .enumerate()
        .filter_map(|(order, message)| {
            let role = string_field(message, &["role", "sender", "author"]).unwrap_or("unknown");
            let text = string_field(message, &["text", "content"])
                .map(str::to_string)
                .unwrap_or_else(|| message_text(message.get("content").unwrap_or(message)));
            if text.trim().is_empty() {
                return None;
            }
            Some(TranscriptMessage {
                role: normalize_role(role),
                text,
                created_at: string_field(message, &["created_at", "create_time"])
                    .map(str::to_string)
                    .or_else(|| value_timestamp(message.get("create_time"))),
                order,
            })
        })
        .collect()
}

fn transcript_record(
    source: SchemaImportSource,
    id: String,
    title: String,
    created_at: Option<String>,
    updated_at: Option<String>,
    messages: Vec<TranscriptMessage>,
) -> ImportRecord {
    let source_id = format!("{}:{id}", source.command_name());
    let mut body = String::new();
    body.push_str("# ");
    body.push_str(&title);
    body.push_str("\n\n");
    push_metadata(&mut body, "Source", source.display_name());
    push_metadata(&mut body, "Source ID", &source_id);
    if let Some(created_at) = created_at.as_deref() {
        push_metadata(&mut body, "Created", created_at);
    }
    if let Some(updated_at) = updated_at.as_deref() {
        push_metadata(&mut body, "Updated", updated_at);
    }
    body.push_str("\n## Transcript\n\n");
    for message in messages {
        body.push_str("### ");
        body.push_str(&message.role);
        if let Some(created_at) = message.created_at.as_deref() {
            body.push_str(" (");
            body.push_str(created_at);
            body.push(')');
        }
        body.push_str("\n\n");
        body.push_str(message.text.trim());
        body.push_str("\n\n");
    }

    ImportRecord {
        source_id,
        title,
        created_at,
        updated_at,
        body,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BookmarkItem {
    title: String,
    url: String,
    folder: Option<String>,
    added_at: Option<String>,
}

fn bookmark_record(bookmark: BookmarkItem) -> ImportRecord {
    let title = clean_title(&bookmark.title);
    let source_id = format!("bookmark:{}", stable_hash(&bookmark.url));
    let mut body = String::new();
    body.push_str("# ");
    body.push_str(&title);
    body.push_str("\n\n");
    push_metadata(&mut body, "Source", "Browser bookmarks");
    push_metadata(&mut body, "Source ID", &source_id);
    push_metadata(&mut body, "URL", &bookmark.url);
    if let Some(folder) = bookmark.folder.as_deref() {
        push_metadata(&mut body, "Folder", folder);
    }
    if let Some(added_at) = bookmark.added_at.as_deref() {
        push_metadata(&mut body, "Added", added_at);
    }
    body.push_str("\nBookmark imported as metadata only; Solo did not crawl the page.\n");

    ImportRecord {
        source_id,
        title,
        created_at: bookmark.added_at,
        updated_at: None,
        body,
    }
}

fn parse_bookmarks_json(value: &serde_json::Value) -> Vec<BookmarkItem> {
    let mut out = Vec::new();
    collect_bookmarks_json(value, &mut Vec::new(), &mut out);
    out
}

fn collect_bookmarks_json(
    value: &serde_json::Value,
    folders: &mut Vec<String>,
    out: &mut Vec<BookmarkItem>,
) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_bookmarks_json(item, folders, out);
            }
        }
        serde_json::Value::Object(map) => {
            let title = string_field(value, &["title", "name"])
                .map(clean_title)
                .unwrap_or_else(|| "Untitled bookmark".to_string());
            let url = string_field(value, &["url", "uri"]).map(str::to_string);
            if let Some(url) = url.as_deref() {
                out.push(BookmarkItem {
                    title: title.clone(),
                    url: url.to_string(),
                    folder: (!folders.is_empty()).then(|| folders.join("/")),
                    added_at: string_field(value, &["date_added", "dateAdded", "add_date"])
                        .map(str::to_string),
                });
            }

            let mut pushed = false;
            let has_children = map
                .get("children")
                .and_then(serde_json::Value::as_array)
                .is_some();
            if has_children && !title.trim().is_empty() && url.is_none() {
                folders.push(title);
                pushed = true;
            }

            if let Some(children) = map.get("children") {
                collect_bookmarks_json(children, folders, out);
            }
            if let Some(roots) = map.get("roots") {
                collect_bookmarks_json(roots, folders, out);
            }
            if !map.contains_key("children") && !map.contains_key("roots") && url.is_none() {
                for child in map.values() {
                    collect_bookmarks_json(child, folders, out);
                }
            }
            if pushed {
                folders.pop();
            }
        }
        _ => {}
    }
}

fn parse_bookmarks_html(raw: &str) -> Result<Vec<BookmarkItem>> {
    let anchor_re = Regex::new(r#"(?is)<a\s+([^>]*)>(.*?)</a>"#).expect("valid anchor regex");
    let href_re = Regex::new(r#"(?is)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)
        .expect("valid href regex");
    let add_date_re = Regex::new(r#"(?is)\badd_date\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)
        .expect("valid add_date regex");
    let tag_re = Regex::new(r#"(?is)<[^>]+>"#).expect("valid tag regex");

    let mut out = Vec::new();
    for caps in anchor_re.captures_iter(raw) {
        let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let href = href_re
            .captures(attrs)
            .and_then(|caps| capture_first(&caps))
            .unwrap_or_default();
        if href.trim().is_empty() {
            continue;
        }
        let raw_title = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let title = html_unescape(tag_re.replace_all(raw_title, "").trim());
        let added_at = add_date_re
            .captures(attrs)
            .and_then(|caps| capture_first(&caps));
        out.push(BookmarkItem {
            title: if title.is_empty() {
                href.clone()
            } else {
                title
            },
            url: html_unescape(&href),
            folder: None,
            added_at,
        });
    }
    Ok(out)
}

fn capture_first(caps: &regex::Captures<'_>) -> Option<String> {
    (1..caps.len())
        .filter_map(|idx| caps.get(idx))
        .map(|m| html_unescape(m.as_str()))
        .next()
}

fn message_text(value: &serde_json::Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if let Some(parts) = value.get("parts").and_then(serde_json::Value::as_array) {
        return parts
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
        return text.to_string();
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .filter_map(|item| {
                item.as_str().map(str::to_string).or_else(|| {
                    item.get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn string_field<'a>(value: &'a serde_json::Value, names: &[&str]) -> Option<&'a str> {
    for name in names {
        if let Some(s) = value.get(*name).and_then(serde_json::Value::as_str) {
            return Some(s);
        }
    }
    None
}

fn value_timestamp(value: Option<&serde_json::Value>) -> Option<String> {
    match value {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn normalize_role(role: &str) -> String {
    match role {
        "human" => "user".to_string(),
        "assistant" => "assistant".to_string(),
        "system" => "system".to_string(),
        "tool" => "tool".to_string(),
        other => clean_title(other),
    }
}

fn matches_schema_filters(id: &str, title: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let id = id.to_ascii_lowercase();
    let title = title.to_ascii_lowercase();
    filters.iter().any(|filter| {
        let filter = filter.trim().to_ascii_lowercase();
        !filter.is_empty() && (id == filter || title.contains(&filter))
    })
}

fn estimate_chunks(bytes: u64, chunk_token_target: u32) -> u64 {
    if bytes == 0 {
        return 0;
    }
    let target_bytes = u64::from(chunk_token_target.max(1)) * ESTIMATED_BYTES_PER_TOKEN;
    bytes.div_ceil(target_bytes)
}

fn push_metadata(body: &mut String, key: &str, value: &str) {
    body.push_str("- ");
    body.push_str(key);
    body.push_str(": ");
    body.push_str(value.trim());
    body.push('\n');
}

fn clean_title(title: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        "Untitled".to_string()
    } else {
        title
    }
}

fn slugify_filename(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "import".to_string()
    } else {
        out
    }
}

fn stable_hash(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_text(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write fixture");
    }

    #[test]
    fn parse_chatgpt_conversations_json_extracts_transcripts() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("conversations.json");
        let fixture = serde_json::json!([
            {
                "id": "conv-1",
                "title": "Release plan",
                "create_time": 1710000000,
                "mapping": {
                    "a": {
                        "message": {
                            "author": { "role": "user" },
                            "create_time": 1710000001,
                            "content": { "parts": ["What ships next?"] }
                        }
                    },
                    "b": {
                        "message": {
                            "author": { "role": "assistant" },
                            "create_time": 1710000002,
                            "content": { "parts": ["Schema-aware importers."] }
                        }
                    }
                }
            },
            {
                "id": "empty",
                "title": "Empty",
                "mapping": {}
            }
        ]);
        write_text(&export, &serde_json::to_string(&fixture).unwrap());

        let scan = parse_schema_import(&export, SchemaImportSource::ChatGpt, &[]).unwrap();

        assert_eq!(scan.records_scanned, 2);
        assert_eq!(scan.skipped_records, 1);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].source_id, "chatgpt:conv-1");
        assert!(scan.records[0].body.contains("## Transcript"));
        assert!(scan.records[0].body.contains("What ships next?"));
        assert!(scan.records[0].body.contains("Schema-aware importers."));
    }

    #[test]
    fn parse_chatgpt_conversations_accepts_utf8_bom() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("conversations.json");
        write_text(
            &export,
            "\u{feff}[{\"id\":\"conv-1\",\"title\":\"BOM\",\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}]}]",
        );

        let scan = parse_schema_import(&export, SchemaImportSource::ChatGpt, &[]).unwrap();

        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].title, "BOM");
    }

    #[test]
    fn parse_chatgpt_conversations_honors_title_filter() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("conversations.json");
        let fixture = serde_json::json!([
            {
                "id": "conv-1",
                "title": "Release plan",
                "messages": [{ "role": "user", "content": "hello" }]
            },
            {
                "id": "conv-2",
                "title": "Dinner",
                "messages": [{ "role": "user", "content": "pizza" }]
            }
        ]);
        write_text(&export, &serde_json::to_string(&fixture).unwrap());

        let scan = parse_schema_import(
            &export,
            SchemaImportSource::ChatGpt,
            &["release".to_string()],
        )
        .unwrap();

        assert_eq!(scan.records_scanned, 2);
        assert_eq!(scan.filtered_records, 1);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].title, "Release plan");
    }

    #[test]
    fn parse_claude_conversations_json_extracts_chat_messages() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("conversations.json");
        let fixture = serde_json::json!({
            "conversations": [
                {
                    "uuid": "claude-1",
                    "name": "Architecture",
                    "created_at": "2026-05-27T10:00:00Z",
                    "chat_messages": [
                        { "sender": "human", "text": "Use one product surface." },
                        { "sender": "assistant", "text": "Tray plus web UI." }
                    ]
                }
            ]
        });
        write_text(&export, &serde_json::to_string(&fixture).unwrap());

        let scan = parse_schema_import(&export, SchemaImportSource::Claude, &[]).unwrap();

        assert_eq!(scan.records_scanned, 1);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].source_id, "claude:claude-1");
        assert!(scan.records[0].body.contains("Use one product surface."));
        assert!(scan.records[0].body.contains("Tray plus web UI."));
    }

    #[test]
    fn parse_bookmarks_html_extracts_links_without_crawling() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("bookmarks.html");
        write_text(
            &export,
            r#"
            <!DOCTYPE NETSCAPE-Bookmark-file-1>
            <DL><p>
              <DT><A HREF="https://solo.dev/docs?token=abc&amp;view=1" ADD_DATE="1710000000">Solo Docs</A>
              <DT><A HREF="https://example.com">Example</A>
            </DL><p>
            "#,
        );

        let scan = parse_schema_import(&export, SchemaImportSource::Bookmarks, &[]).unwrap();

        assert_eq!(scan.records_scanned, 2);
        assert_eq!(scan.records.len(), 2);
        assert!(scan.records[0].body.contains("Source: Browser bookmarks"));
        assert!(scan.records[0].body.contains("Solo did not crawl the page"));
        assert!(
            scan.records[0]
                .body
                .contains("https://solo.dev/docs?token=abc&view=1")
        );
    }

    #[test]
    fn parse_bookmarks_json_walks_nested_browser_trees() {
        let fixture = serde_json::json!({
            "roots": {
                "bookmark_bar": {
                    "name": "Bookmarks Bar",
                    "children": [
                        {
                            "name": "Solo",
                            "url": "https://solo.dev",
                            "date_added": "1710000000"
                        },
                        {
                            "name": "Research",
                            "children": [
                                { "title": "Paper", "uri": "https://example.com/paper" }
                            ]
                        }
                    ]
                }
            }
        });

        let bookmarks = parse_bookmarks_json(&fixture);

        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].title, "Solo");
        assert_eq!(bookmarks[0].folder.as_deref(), Some("Bookmarks Bar"));
        assert_eq!(
            bookmarks[1].folder.as_deref(),
            Some("Bookmarks Bar/Research")
        );
    }

    #[test]
    fn materialized_record_path_is_stable_and_markdown() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let record = ImportRecord {
            source_id: "chatgpt:abc".to_string(),
            title: "Release Plan!".to_string(),
            created_at: None,
            updated_at: None,
            body: "# Release Plan\n".to_string(),
        };

        let first = materialize_schema_record(tmp.path(), &record).unwrap();
        let second = materialize_schema_record(tmp.path(), &record).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("md"));
        assert_eq!(std::fs::read_to_string(first).unwrap(), "# Release Plan\n");
    }
}
