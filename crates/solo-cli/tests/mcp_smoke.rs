// SPDX-License-Identifier: Apache-2.0

//! MCP-stdio smoke tests. Spawn `solo mcp-stdio` as a real subprocess
//! and drive it via raw JSON-RPC over stdin/stdout — the same wire
//! protocol that production MCP clients (Claude Desktop, Cursor)
//! speak.
//!
//! Why raw JSON-RPC and not rmcp's client side: rmcp itself is well-
//! tested upstream; the value of this harness is asserting that
//! Solo's stdio transport handles the actual line-delimited-JSON
//! framing correctly end-to-end, which is what an external client
//! sees. Bonus: no extra dev-dep (just `serde_json`, already in the
//! workspace).
//!
//! ## What's covered
//!
//!   - `mcp_stdio_lists_canonical_tools` — `initialize` →
//!     `notifications/initialized` → `tools/list` round-trip;
//!     verifies the canonical tools are exposed by name
//!     (four episode tools + three derived-layer tools added in
//!     v0.4.0: themes, facts_about, contradictions + one added in
//!     v0.5.0: inspect_cluster + five document tools added in
//!     v0.7.0: ingest_document, search_docs, inspect_document,
//!     list_documents, forget_document + one added in v0.9.2:
//!     remember_batch + memory_context + update/inbox/review/entities/resolve
//!     + five upload/staged-ingest tools).
//!
//!   - `mcp_stdio_remember_batch_round_trip` — exercises the v0.9.2
//!     `memory_remember_batch` tool with 5 items carrying varied
//!     `source_type` + `salience` values; asserts the reply is an
//!     ordered array of 5 distinct memory_ids and that each item is
//!     recallable through `memory_recall`.
//!
//!   - `mcp_stdio_remember_then_recall_round_trip` — exercises
//!     `tools/call` for `memory_remember` followed by `memory_recall`
//!     with a unique content string; asserts the recall result text
//!     contains the remembered content.
//!
//!   - `mcp_stdio_ingest_then_search_doc_round_trip` — ingests a
//!     small markdown fixture written to a tempfile, then issues
//!     `memory_search_docs` for content known to live in the chunk.
//!     Verifies the full ingest → embed → HNSW → search read path
//!     through the real MCP wire (the daemon spawns a fully-wired
//!     writer with an embedder, so unlike the in-process Harness
//!     tests this exercises the actual P3 + P4 + P5 stack
//!     end-to-end).
//!
//! ## What's NOT covered
//!
//! - Server-initiated requests (rmcp 0.1.5 doesn't send any unsolicited
//!   to clients in our config; if that changes, the read loop already
//!   skips non-matching ids).
//! - SSE/HTTP transports — covered by the http handler tests.
//! - Tool argument schema validation beyond the happy path.
//!
//! ## Cost
//!
//! Each test does a fresh `solo init` (Argon2id key derivation, ~1-3
//! sec) plus an MCP-stdio spawn (another key derivation). File total
//! runs ~6-8 sec on Windows.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PASSPHRASE: &str = "test-passphrase-for-mcp-smoke-tests";
const PROTOCOL_VERSION: &str = "2024-11-05";

fn solo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_solo"))
}

fn force_stub_embedder(data_dir: &Path) {
    let path = data_dir.join("solo.config.toml");
    let existing = std::fs::read_to_string(&path).expect("read solo.config.toml");
    let salt_hex = existing
        .lines()
        .find_map(|line| line.trim().strip_prefix("salt_hex = "))
        .expect("extract salt_hex from solo.config.toml");
    std::fs::write(
        &path,
        format!(
            "schema_version = 1\n\
             salt_hex = {salt_hex}\n\n\
             [embedder]\n\
             name = \"stub\"\n\
             version = \"v1\"\n\
             dim = 32\n\
             dtype = \"f32\"\n\n\
             [llm]\n\
             mode = \"none\"\n"
        ),
    )
    .expect("write stub solo.config.toml");
}

/// Run `solo init` synchronously against `data_dir`, panicking with
/// stderr on non-zero exit so the test author sees what broke.
fn solo_init(data_dir: &Path) {
    let out = Command::new(solo_bin())
        .env("SOLO_DATA_DIR", data_dir)
        .env("SOLO_PASSPHRASE", PASSPHRASE)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(
        out.status.success(),
        "solo init failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    force_stub_embedder(data_dir);
}

/// Minimal JSON-RPC client over `solo mcp-stdio`'s stdin/stdout.
/// Newline-delimited messages — the MCP stdio framing.
struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    /// Spawn `solo mcp-stdio` with stdin/stdout piped. Caller owns the
    /// returned `McpClient`; dropping it closes stdin (signaling rmcp
    /// to exit cleanly) and waits up to 10s before force-killing.
    fn spawn(data_dir: &Path) -> Self {
        let mut child = Command::new(solo_bin())
            .env("SOLO_DATA_DIR", data_dir)
            .env("SOLO_PASSPHRASE", PASSPHRASE)
            .arg("mcp-stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Mute stderr — rmcp + tracing are noisy and would clutter
            // test output. Re-enable to `Stdio::inherit()` to debug.
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn solo mcp-stdio");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
        }
    }

    /// Send a JSON-RPC request and block until the matching response
    /// arrives. Server-initiated messages (notifications, requests
    /// with a different `id`) are silently consumed in the meantime.
    /// Panics on transport / parse / server-error so test failures
    /// surface with the actual response body.
    fn send(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req).expect("serialize request");
        let stdin = self
            .stdin
            .as_mut()
            .expect("stdin already taken (post-shutdown send?)");
        writeln!(stdin, "{line}").expect("write request");
        stdin.flush().expect("flush request");

        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read response");
            if n == 0 {
                panic!("EOF on mcp-stdio stdout while waiting for id={id}");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // Stdio frames messages with `\n`; stray empty lines
                // are tolerable. Real protocol responses are non-empty.
                continue;
            }
            let resp: Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
                panic!(
                    "parse JSON response (id={id} method={method}): {e}\n\
                     line was: {trimmed:?}"
                )
            });
            // Match by id; skip notifications + unrelated.
            if resp.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = resp.get("error") {
                    panic!("MCP error response for {method}: {err}");
                }
                return resp;
            }
        }
    }

    /// Send a JSON-RPC notification (no id, no reply expected).
    fn notify(&mut self, method: &str, params: Value) {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req).expect("serialize notification");
        let stdin = self.stdin.as_mut().expect("stdin already taken");
        writeln!(stdin, "{line}").expect("write notification");
        stdin.flush().expect("flush notification");
    }

    /// Standard initialize → notifications/initialized handshake.
    fn handshake(&mut self) {
        let resp = self.send(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "solo-test", "version": "0.0.0" },
            }),
        );
        assert!(
            resp.get("result").is_some(),
            "initialize lacked result: {resp}"
        );
        self.notify("notifications/initialized", json!({}));
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Close stdin first — rmcp's serve loop reads stdin lines and
        // exits on EOF, then the daemon's `ctx.shutdown()` runs (drains
        // writer, saves snapshot). Give it 10s, then force-kill so
        // tempdir cleanup doesn't deadlock waiting for a stuck child.
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => return,
            }
        }
    }
}

/// Cheap process-local random suffix so concurrent test runs don't
/// collide on remembered content (matches `process_lifecycle.rs`).
fn rand_suffix() -> u32 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ std::process::id()
}

#[test]
fn mcp_stdio_lists_canonical_tools() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    let resp = client.send("tools/list", json!({}));
    let tools = resp
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("tools/list missing /result/tools array: {resp}"));

    let mut names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    names.sort();

    assert_eq!(
        names,
        vec![
            "document_upload_abort".to_string(),
            "document_upload_chunk_base64".to_string(),
            "document_upload_commit".to_string(),
            "document_upload_prepare".to_string(),
            "document_upload_status".to_string(),
            "memory_attach".to_string(),
            "memory_context".to_string(),
            "memory_contradiction_resolve".to_string(),
            "memory_contradictions".to_string(),
            "memory_entities".to_string(),
            "memory_explain_provenance".to_string(),
            "memory_facts_about".to_string(),
            "memory_forget".to_string(),
            // Forget lifecycle tools sort lexicographically after `memory_forget`.
            "memory_forget_asset".to_string(),
            "memory_forget_document".to_string(),
            "memory_graph_paths".to_string(),
            "memory_import_documents".to_string(),
            "memory_inbox".to_string(),
            "memory_ingest_document".to_string(),
            "memory_ingest_staged_document".to_string(),
            "memory_inspect".to_string(),
            "memory_inspect_asset".to_string(),
            // v0.5.0 Priority 3 — `c` < `r` lexicographically.
            "memory_inspect_cluster".to_string(),
            "memory_inspect_document".to_string(),
            "memory_link_document_asset".to_string(),
            "memory_list_assets".to_string(),
            "memory_list_document_assets".to_string(),
            "memory_list_documents".to_string(),
            "memory_list_memory_attachments".to_string(),
            "memory_prepare_asset_download".to_string(),
            "memory_prepare_document_source_download".to_string(),
            "memory_recall".to_string(),
            "memory_remember".to_string(),
            // v0.9.2 — batched-remember.
            "memory_remember_batch".to_string(),
            "memory_request_entity_split".to_string(),
            "memory_review".to_string(),
            "memory_search_docs".to_string(),
            "memory_themes".to_string(),
            "memory_update".to_string(),
        ],
        "tools/list returned unexpected name set"
    );

    // Each tool must carry schemas and annotations over JSON-RPC
    // `tools/list`, not just in the in-process API.
    for t in tools {
        assert!(
            t.get("description")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()),
            "tool missing description: {t}"
        );
        assert!(
            t.get("inputSchema").is_some(),
            "tool missing inputSchema: {t}"
        );
        assert!(
            t.get("outputSchema")
                .and_then(|schema| schema.get("type"))
                .and_then(|v| v.as_str())
                == Some("object"),
            "tool missing root-object outputSchema: {t}"
        );
        assert!(
            t.get("annotations")
                .and_then(|annotations| annotations.get("openWorldHint"))
                .and_then(|v| v.as_bool())
                == Some(false),
            "tool missing closed-world annotation: {t}"
        );
    }
}

#[test]
fn mcp_stdio_remember_then_recall_round_trip() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    let unique = format!("mcp-roundtrip-{:x}", rand_suffix());

    // remember
    let resp = client.send(
        "tools/call",
        json!({
            "name": "memory_remember",
            "arguments": { "content": unique.clone() },
        }),
    );
    let remember_text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("remember response missing /result/content/0/text: {resp}"));
    // The handler returns a confirmation containing the new MemoryId.
    // Don't pin to a specific phrasing; just assert it's non-empty.
    assert!(
        !remember_text.is_empty(),
        "remember returned empty text: {resp}"
    );

    // recall — query with the unique content; the stub embedder hashes
    // both the stored content and the query, so a literal match recalls
    // identically. The recall handler returns text that includes each
    // hit's content.
    let resp = client.send(
        "tools/call",
        json!({
            "name": "memory_recall",
            "arguments": { "query": unique.clone(), "limit": 5 },
        }),
    );
    let recall_text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("recall response missing /result/content/0/text: {resp}"));
    assert!(
        recall_text.contains(&unique),
        "recall result didn't contain the remembered content `{unique}`; got: {recall_text}"
    );
}

#[test]
fn mcp_stdio_ingest_then_search_doc_round_trip() {
    // End-to-end coverage for the v0.7.0 P5+P6 document tools through
    // the real MCP wire (subprocess + JSON-RPC).
    //
    // Pipeline exercised: file on disk →
    //   memory_ingest_document (parse + chunk + embed + persist) →
    //   memory_search_docs (embed query → HNSW → SQL → JSON hits).
    //
    // The stub embedder is deterministic over content prefixes (hashes
    // the input bytes), so an exact-substring search will find chunks
    // containing that substring at high cosine-distance rank.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    // Write a small markdown fixture next to the data dir. Path must
    // be readable by the subprocess; same parent gives us a clean
    // cleanup at test end via `tmp.close()` (implicit on drop).
    let unique = format!("doc-needle-{:x}", rand_suffix());
    let fixture_path = tmp.path().join("notes.md");
    let fixture_body = format!(
        "# Personal notes\n\nThis is a small markdown file used to \
         exercise the v0.7.0 document ingest path.\n\nThe needle phrase \
         for the test is `{unique}` — searching for it via \
         memory_search_docs should surface the chunk this paragraph \
         lives in.\n\nA second paragraph adds more bulk so the chunker \
         has at least a couple sentences of context to work with.\n"
    );
    std::fs::write(&fixture_path, &fixture_body).expect("write fixture");

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    // Ingest. The handler returns the IngestReport as pretty JSON; we
    // parse it back and assert the doc_id / chunks_persisted fields.
    let resp = client.send(
        "tools/call",
        json!({
            "name": "memory_ingest_document",
            "arguments": { "path": fixture_path.to_string_lossy() },
        }),
    );
    let ingest_text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("ingest response missing /result/content/0/text: {resp}"));
    let report: Value = serde_json::from_str(ingest_text)
        .unwrap_or_else(|e| panic!("ingest response not JSON: {e}; body was: {ingest_text}"));
    let doc_id = report
        .get("doc_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("ingest response missing doc_id: {ingest_text}"));
    assert_eq!(
        doc_id.len(),
        36,
        "expected UUID-shaped doc_id, got: {doc_id}"
    );
    let chunks_persisted = report
        .get("chunks_persisted")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("ingest response missing chunks_persisted: {ingest_text}"));
    assert!(
        chunks_persisted >= 1,
        "expected at least 1 chunk, got {chunks_persisted}: {ingest_text}"
    );

    // Search for the needle phrase. The stub embedder makes literal
    // substring matches highly recallable; we just need one hit
    // containing the needle.
    let resp = client.send(
        "tools/call",
        json!({
            "name": "memory_search_docs",
            "arguments": { "query": unique.clone(), "limit": 5 },
        }),
    );
    let search_text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("search response missing /result/content/0/text: {resp}"));
    let hits: Value = serde_json::from_str(search_text)
        .unwrap_or_else(|e| panic!("search response not JSON: {e}; body was: {search_text}"));
    let arr = hits
        .get("hits")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("search response missing hits array: {search_text}"));
    assert!(
        !arr.is_empty(),
        "search returned no hits for needle `{unique}`: {search_text}"
    );
    let found_needle = arr.iter().any(|h| {
        h.get("content")
            .and_then(|c| c.as_str())
            .is_some_and(|s| s.contains(&unique))
    });
    assert!(
        found_needle,
        "no hit contained the needle `{unique}`: {search_text}"
    );
}

#[test]
fn mcp_stdio_remember_batch_round_trip() {
    // v0.9.2 end-to-end coverage for `memory_remember_batch`:
    //
    //   1. Spawn a fresh `solo mcp-stdio` subprocess.
    //   2. Send a 5-item batch with mixed `source_type` + `salience`
    //      values; assert the reply is a JSON array of 5 strings
    //      (memory_ids in input order).
    //   3. For each item, query `memory_recall` against the unique
    //      needle and confirm a hit is returned. This proves the batch
    //      tx committed AND the post-commit `hnsw.add` per item ran —
    //      the canonical end-to-end batch invariant.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    let prefix = format!("batch-needle-{:x}", rand_suffix());
    // 5 items — each gets a unique suffix so we can search for it
    // individually after the batch commit.
    let needles: Vec<String> = (0..5).map(|i| format!("{prefix}-{i}")).collect();
    let items_json: Vec<Value> = needles
        .iter()
        .enumerate()
        .map(|(i, content)| {
            // Mix source_type + salience values so the test pins both
            // optional fields' round-trip behaviour, not just `content`.
            json!({
                "content": content,
                "source_type": if i % 2 == 0 { "user_preference" } else { "agent_response" },
                "salience": 0.1 + (i as f64) * 0.2,
            })
        })
        .collect();

    let resp = client.send(
        "tools/call",
        json!({
            "name": "memory_remember_batch",
            "arguments": { "items": items_json },
        }),
    );
    let batch_text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("batch response missing /result/content/0/text: {resp}"));

    // Reply must be a JSON array of 5 distinct strings in input order.
    let ids: Value = serde_json::from_str(batch_text)
        .unwrap_or_else(|e| panic!("batch reply not JSON: {e}; body was: {batch_text}"));
    let ids_arr = ids
        .as_array()
        .unwrap_or_else(|| panic!("batch reply not an array: {batch_text}"));
    assert_eq!(ids_arr.len(), 5, "expected 5 memory_ids, got: {batch_text}");
    let mut id_strs: Vec<&str> = ids_arr
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("batch reply id not a string: {batch_text}"))
        })
        .collect();
    // Each id must look UUID-shaped + be distinct.
    for id in &id_strs {
        assert_eq!(id.len(), 36, "expected UUID-shaped memory_id, got: {id}");
    }
    id_strs.sort();
    id_strs.dedup();
    assert_eq!(
        id_strs.len(),
        5,
        "memory_ids must be distinct: {batch_text}"
    );

    // Verify each needle is recallable.
    for needle in &needles {
        let resp = client.send(
            "tools/call",
            json!({
                "name": "memory_recall",
                "arguments": { "query": needle, "limit": 5 },
            }),
        );
        let recall_text = resp
            .pointer("/result/content/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("recall missing /result/content/0/text: {resp}"));
        assert!(
            recall_text.contains(needle),
            "recall for `{needle}` didn't return the batched item; got: {recall_text}"
        );
    }
}

#[test]
fn mcp_stdio_remember_batch_rejects_empty_items() {
    // v0.9.2 — empty `items` is a client-side error; the handler must
    // return invalid_params BEFORE the writer-actor is even contacted.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 999,
        "method": "tools/call",
        "params": {
            "name": "memory_remember_batch",
            "arguments": { "items": [] },
        },
    });
    let line = serde_json::to_string(&req).expect("serialize request");
    let stdin = client.stdin.as_mut().expect("stdin");
    writeln!(stdin, "{line}").expect("write request");
    stdin.flush().expect("flush request");

    loop {
        let mut buf = String::new();
        let n = client.stdout.read_line(&mut buf).expect("read response");
        if n == 0 {
            panic!("EOF before empty-batch error response");
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resp: Value = serde_json::from_str(trimmed).expect("parse JSON");
        if resp.get("id").and_then(|v| v.as_i64()) != Some(999) {
            continue;
        }
        // MCP error responses carry `error.code` + `error.message`.
        // rmcp's invalid_params is -32602.
        let err = resp
            .get("error")
            .unwrap_or_else(|| panic!("expected error response for empty batch: {resp}"));
        let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or_default();
        assert_eq!(
            code, -32602,
            "expected invalid_params code -32602, got: {err}"
        );
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            message.contains("must not be empty"),
            "error message should mention empty-items rejection; got: {message}"
        );
        break;
    }
}
