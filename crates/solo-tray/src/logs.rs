// SPDX-License-Identifier: Apache-2.0

//! Bounded ring buffer for captured daemon stderr.
//!
//! The supervisor writes lines into here from its stderr-reader task;
//! the egui window reads them out on every paint. Lock contention is
//! negligible — both sides hold the mutex for microseconds.

use std::collections::VecDeque;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

const REDACTED: &str = "[redacted]";
const MAX_LINE_LEN: usize = 4096;
const TRUNCATED_MARKER: &str = " ...[truncated]";

/// One captured log line. We keep the level extraction cheap (no full
/// tracing parsing) — just a prefix match on the canonical level
/// strings. Unknown lines default to `Info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Infer a log line's level from its prefix. Matches the
    /// `tracing-subscriber` default format `LEVEL msg…` (level may be
    /// padded; we strip leading whitespace before matching).
    ///
    /// Assumes the caller has already stripped ANSI escapes — see
    /// [`strip_ansi`] and [`RingBuffer::push_line`].
    pub fn infer(line: &str) -> Self {
        let trimmed = line.trim_start();
        // Match longest-first so "ERROR" doesn't fall through to "ERR".
        if trimmed.starts_with("ERROR") {
            Self::Error
        } else if trimmed.starts_with("WARN") {
            Self::Warn
        } else if trimmed.starts_with("INFO") {
            Self::Info
        } else if trimmed.starts_with("DEBUG") {
            Self::Debug
        } else if trimmed.starts_with("TRACE") {
            Self::Trace
        } else {
            Self::Info
        }
    }

    /// Numeric ordering for filter comparisons. Higher = more severe.
    pub fn severity(self) -> u8 {
        match self {
            Self::Trace => 0,
            Self::Debug => 1,
            Self::Info => 2,
            Self::Warn => 3,
            Self::Error => 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: Level,
    pub text: String,
}

/// Fixed-capacity ring buffer of captured log lines. When full, oldest
/// lines are dropped on insert.
#[derive(Debug)]
pub struct RingBuffer {
    lines: VecDeque<LogLine>,
    capacity: usize,
    /// Total lines ever appended. Useful for the window status bar
    /// ("retained N of M total").
    pub seen: u64,
    /// Total lines dropped from the front because the buffer was full.
    pub dropped: u64,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity),
            capacity,
            seen: 0,
            dropped: 0,
        }
    }

    pub fn push_line(&mut self, raw: impl Into<String>) {
        let raw = raw.into();
        // Strip ANSI escape sequences (terminal colour codes) before
        // storage. The Solo daemon's `tracing-subscriber` defaults to
        // ANSI-coloured stderr; those escapes (`\x1B[33m`, `\x1B[0m`,
        // etc.) render as garbage glyphs in egui and add ~50 chars
        // per log line. Cumulatively that made the log viewer hit
        // multi-second per-frame render times on Windows, tripping
        // the OS "Not Responding" watchdog. Strip once on insertion
        // — much cheaper than stripping on every paint.
        let mut text = normalize_log_line(&raw);
        // Defensive cap. A runaway daemon log line (e.g. a huge
        // serialized struct) shouldn't be able to lock up text
        // shaping for the entire viewer. 4 KiB is comfortably wider
        // than any tracing line we generate.
        if text.len() > MAX_LINE_LEN {
            // Truncate on a char boundary so we don't slice mid-UTF8.
            let mut cut = MAX_LINE_LEN;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str(TRUNCATED_MARKER);
        }
        let level = Level::infer(&text);
        self.seen += 1;
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(LogLine { level, text });
    }

    /// Iterate over lines whose severity >= `min_level`. Cheap copy of
    /// the references; intended for per-frame egui rendering.
    pub fn iter_filtered(&self, min_level: Level) -> impl Iterator<Item = &LogLine> {
        self.lines
            .iter()
            .filter(move |l| l.level.severity() >= min_level.severity())
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        // Don't reset `seen` / `dropped` — those are lifetime totals.
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }
}

/// Resolve `<data_dir>/tray.log` using the same data-dir fallback as
/// the tray menu and settings UI.
pub fn tray_log_path() -> PathBuf {
    crate::tray::resolve_data_dir().join("tray.log")
}

/// Read the last `max_lines` from a log file, applying the same
/// operator-visible normalization as the daemon stderr buffer.
pub fn read_tail_lines(path: &Path, max_lines: usize) -> io::Result<Vec<String>> {
    if max_lines == 0 {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path)?;
    let reader = io::BufReader::new(file);
    let mut tail = VecDeque::with_capacity(max_lines);
    for line in reader.lines() {
        let mut text = normalize_log_line(&line?);
        if text.len() > MAX_LINE_LEN {
            let mut cut = MAX_LINE_LEN;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str(TRUNCATED_MARKER);
        }
        if tail.len() == max_lines {
            tail.pop_front();
        }
        tail.push_back(text);
    }
    Ok(tail.into_iter().collect())
}

/// Remove sensitive values from operator-visible log text.
pub fn sanitize_log_text(input: &str) -> String {
    let mut text = redact_bearer_tokens(input);
    for key in [
        "solo_passphrase",
        "passphrase",
        "password",
        "bearer_token",
        "access_token",
        "refresh_token",
        "api_key",
        "apikey",
        "token",
        "secret",
    ] {
        text = redact_assignment_values(&text, key);
    }
    for flag in [
        "--bearer-token",
        "--access-token",
        "--refresh-token",
        "--api-key",
        "--token",
        "--passphrase",
        "--password",
        "--secret",
    ] {
        text = redact_flag_values(&text, flag);
    }
    text
}

fn normalize_log_line(raw: &str) -> String {
    sanitize_log_text(&strip_ansi(raw))
}

fn redact_bearer_tokens(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("bearer") {
        let start = cursor + relative;
        let marker_end = start + "bearer".len();
        if !is_key_boundary_before(input, start) {
            out.push_str(&input[cursor..marker_end]);
            cursor = marker_end;
            continue;
        }
        let Some((after_marker, ch)) = next_char_at(input, marker_end) else {
            break;
        };
        if !ch.is_whitespace() {
            out.push_str(&input[cursor..after_marker]);
            cursor = after_marker;
            continue;
        }
        let value_start = skip_horizontal_whitespace(input, after_marker);
        let value_end = secret_value_end(input, value_start);
        if value_end == value_start {
            out.push_str(&input[cursor..value_start]);
            cursor = value_start;
            continue;
        }
        out.push_str(&input[cursor..value_start]);
        out.push_str(REDACTED);
        cursor = value_end;
    }
    out.push_str(&input[cursor..]);
    out
}

fn redact_assignment_values(input: &str, key: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(&key) {
        let start = cursor + relative;
        let key_end = start + key.len();
        if !is_key_boundary_before(input, start) {
            out.push_str(&input[cursor..key_end]);
            cursor = key_end;
            continue;
        }
        let mut sep_index = skip_horizontal_whitespace(input, key_end);
        if let Some((after_quote, '"' | '\'')) = next_char_at(input, sep_index) {
            sep_index = skip_horizontal_whitespace(input, after_quote);
        }
        let Some((after_sep, sep)) = next_char_at(input, sep_index) else {
            break;
        };
        if sep != '=' && sep != ':' {
            out.push_str(&input[cursor..after_sep]);
            cursor = after_sep;
            continue;
        }
        let (value_start, value_end) = secret_value_range(input, after_sep);
        if value_end == value_start {
            out.push_str(&input[cursor..value_start]);
            cursor = value_start;
            continue;
        }
        out.push_str(&input[cursor..value_start]);
        out.push_str(REDACTED);
        cursor = value_end;
    }
    out.push_str(&input[cursor..]);
    out
}

fn redact_flag_values(input: &str, flag: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let flag = flag.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(&flag) {
        let start = cursor + relative;
        let flag_end = start + flag.len();
        if !is_key_boundary_before(input, start) {
            out.push_str(&input[cursor..flag_end]);
            cursor = flag_end;
            continue;
        }
        let Some((after_marker, ch)) = next_char_at(input, flag_end) else {
            break;
        };
        let raw_value_start = if ch == '=' || ch.is_whitespace() {
            skip_horizontal_whitespace(input, after_marker)
        } else {
            out.push_str(&input[cursor..after_marker]);
            cursor = after_marker;
            continue;
        };
        let (value_start, value_end) = secret_value_range(input, raw_value_start);
        if value_end == value_start {
            out.push_str(&input[cursor..value_start]);
            cursor = value_start;
            continue;
        }
        out.push_str(&input[cursor..value_start]);
        out.push_str(REDACTED);
        cursor = value_end;
    }
    out.push_str(&input[cursor..]);
    out
}

fn next_char_at(input: &str, index: usize) -> Option<(usize, char)> {
    input[index..]
        .chars()
        .next()
        .map(|ch| (index + ch.len_utf8(), ch))
}

fn skip_horizontal_whitespace(input: &str, index: usize) -> usize {
    let mut cursor = index;
    while let Some((next, ch)) = next_char_at(input, cursor) {
        if ch == ' ' || ch == '\t' {
            cursor = next;
        } else {
            break;
        }
    }
    cursor
}

fn is_key_boundary_before(input: &str, index: usize) -> bool {
    match input[..index].chars().next_back() {
        Some(ch) => !matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-'),
        None => true,
    }
}

fn secret_value_range(input: &str, start: usize) -> (usize, usize) {
    let value_start = skip_horizontal_whitespace(input, start);
    if let Some((quoted_start, quote @ ('"' | '\''))) = next_char_at(input, value_start) {
        let value_end = input[quoted_start..]
            .find(quote)
            .map(|offset| quoted_start + offset)
            .unwrap_or(input.len());
        return (quoted_start, value_end);
    }
    (value_start, secret_value_end(input, value_start))
}

fn secret_value_end(input: &str, start: usize) -> usize {
    for (offset, ch) in input[start..].char_indices() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '&' | '#'
                    | ','
                    | ';'
                    | '"'
                    | '\''
                    | '`'
                    | '<'
                    | '>'
                    | '|'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
            )
        {
            return start + offset;
        }
    }
    input.len()
}

/// Strip ANSI escape sequences from `s`. Handles the common subset
/// (`CSI` sequences like `\x1B[33m`, `\x1B[1;36m`, `\x1B[0K`); does
/// NOT handle the full ANSI grammar (DCS, OSC, etc.) which the
/// tracing-subscriber doesn't emit.
///
/// Works at the byte level for the ESC/CSI scan, then reassembles
/// via `from_utf8_lossy` so multi-byte UTF-8 sequences in the
/// surrounding text (e.g. operator names with non-ASCII chars) round-
/// trip correctly. ANSI escape bytes themselves are pure ASCII so
/// the byte-level scan is safe.
fn strip_ansi(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B {
            // ESC: skip the sequence and continue.
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // CSI: skip to the final byte (0x40..=0x7E inclusive).
                i += 2;
                while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // skip the final byte itself
                }
            } else {
                // Bare ESC — drop the single byte and keep going.
                i += 1;
            }
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_infer_matches_known_prefixes() {
        assert_eq!(Level::infer("ERROR something went wrong"), Level::Error);
        assert_eq!(Level::infer("  WARN  stale lockfile"), Level::Warn);
        assert_eq!(Level::infer("INFO ready"), Level::Info);
        assert_eq!(Level::infer("DEBUG verbose"), Level::Debug);
        assert_eq!(Level::infer("TRACE noisy"), Level::Trace);
        assert_eq!(Level::infer("random output"), Level::Info);
    }

    #[test]
    fn ring_buffer_drops_oldest_when_full() {
        let mut rb = RingBuffer::new(3);
        rb.push_line("a");
        rb.push_line("b");
        rb.push_line("c");
        rb.push_line("d");
        assert_eq!(rb.len(), 3);
        assert_eq!(rb.dropped, 1);
        assert_eq!(rb.seen, 4);
        let texts: Vec<&str> = rb.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["b", "c", "d"]);
    }

    #[test]
    fn iter_filtered_respects_severity() {
        let mut rb = RingBuffer::new(10);
        rb.push_line("INFO hello");
        rb.push_line("ERROR bad");
        rb.push_line("DEBUG detail");
        let warn_plus: Vec<&str> = rb
            .iter_filtered(Level::Warn)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(warn_plus, vec!["ERROR bad"]);
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        // Typical tracing-subscriber stderr line: colourised level
        // prefix + dim/reset around timestamps and field names.
        let raw = "\x1B[32mINFO\x1B[0m starting\x1B[2m field=\x1B[0mvalue";
        let stripped = strip_ansi(raw);
        assert_eq!(stripped, "INFO starting field=value");
    }

    #[test]
    fn strip_ansi_preserves_multibyte_utf8() {
        // Operator names sometimes contain non-ASCII chars; we must
        // not corrupt UTF-8 multi-byte sequences during the strip.
        let raw = "\x1B[33mWARN\x1B[0m user=naïveté ☃";
        assert_eq!(strip_ansi(raw), "WARN user=naïveté ☃");
    }

    #[test]
    fn level_inferred_from_stripped_text() {
        let mut rb = RingBuffer::new(4);
        rb.push_line("\x1B[31mERROR\x1B[0m oh no");
        rb.push_line("\x1B[33mWARN\x1B[0m something");
        assert_eq!(rb.lines[0].level, Level::Error);
        assert_eq!(rb.lines[0].text, "ERROR oh no");
        assert_eq!(rb.lines[1].level, Level::Warn);
        assert_eq!(rb.lines[1].text, "WARN something");
    }

    #[test]
    fn sanitize_log_text_redacts_common_secret_shapes() {
        let raw = concat!(
            "Authorization: Bearer secret-token ",
            "SOLO_PASSPHRASE=swordfish ",
            "url=http://127.0.0.1/?token=query-secret&ok=1 ",
            r#"json={"api_key":"json-secret","refresh_token":"refresh-secret"} "#,
            "--bearer-token cli-secret --passphrase=\"quoted-secret\""
        );
        let text = sanitize_log_text(raw);
        for secret in [
            "secret-token",
            "swordfish",
            "query-secret",
            "json-secret",
            "refresh-secret",
            "cli-secret",
            "quoted-secret",
        ] {
            assert!(!text.contains(secret), "{secret} leaked in {text}");
        }
        assert!(text.contains(REDACTED));
        assert!(text.contains("ok=1"));
    }

    #[test]
    fn ring_buffer_stores_sanitized_text() {
        let mut rb = RingBuffer::new(4);
        rb.push_line("INFO api_key=super-secret ready");
        assert_eq!(rb.lines[0].text, "INFO api_key=[redacted] ready");
    }

    #[test]
    fn read_tail_lines_bounds_and_sanitizes_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tray.log");
        std::fs::write(&path, "INFO first\nWARN token=secret-token\nERROR third\n")
            .expect("write tray log");

        let lines = read_tail_lines(&path, 2).expect("tail lines");
        assert_eq!(
            lines,
            vec![
                "WARN token=[redacted]".to_string(),
                "ERROR third".to_string()
            ]
        );
    }

    #[test]
    fn push_line_truncates_very_long_input() {
        let mut rb = RingBuffer::new(4);
        let huge = "x".repeat(10_000);
        rb.push_line(huge);
        assert!(rb.lines[0].text.len() <= 4096 + TRUNCATED_MARKER.len());
        assert!(rb.lines[0].text.ends_with(TRUNCATED_MARKER.trim_start()));
    }
}
