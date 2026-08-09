// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::io::{self, BufRead};
use std::path::Path;

const REDACTED: &str = "[redacted]";
const MAX_LINE_LEN: usize = 4096;
const TRUNCATED_MARKER: &str = " ...[truncated]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TailLine {
    pub(crate) level: &'static str,
    pub(crate) text: String,
}

pub(crate) fn read_tail_lines(path: &Path, limit: usize) -> io::Result<Vec<TailLine>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path)?;
    let reader = io::BufReader::new(file);
    let mut tail = VecDeque::with_capacity(limit);
    for line in reader.lines() {
        let text = normalize_log_line(&line?);
        if tail.len() == limit {
            tail.pop_front();
        }
        tail.push_back(TailLine {
            level: infer_level(&text),
            text,
        });
    }
    Ok(tail.into_iter().collect())
}

fn normalize_log_line(raw: &str) -> String {
    let mut text = sanitize_log_text(&strip_ansi(raw));
    if text.len() > MAX_LINE_LEN {
        let mut cut = MAX_LINE_LEN;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str(TRUNCATED_MARKER);
    }
    text
}

fn infer_level(line: &str) -> &'static str {
    let trimmed = line.trim_start();
    if trimmed.starts_with("ERROR") {
        "error"
    } else if trimmed.starts_with("WARN") {
        "warn"
    } else if trimmed.starts_with("DEBUG") {
        "debug"
    } else if trimmed.starts_with("TRACE") {
        "trace"
    } else {
        "info"
    }
}

fn sanitize_log_text(input: &str) -> String {
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

fn strip_ansi(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
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
    fn read_tail_lines_bounds_levels_and_sanitizes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tray.log");
        std::fs::write(
            &path,
            "INFO first\nWARN token=secret-token\n\x1B[31mERROR\x1B[0m bearer abc123\n",
        )
        .expect("write log");

        let lines = read_tail_lines(&path, 2).expect("tail");

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].level, "warn");
        assert_eq!(lines[0].text, "WARN token=[redacted]");
        assert_eq!(lines[1].level, "error");
        assert_eq!(lines[1].text, "ERROR bearer [redacted]");
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
    fn normalize_log_line_truncates_very_long_input() {
        let text = normalize_log_line(&"x".repeat(10_000));
        assert!(text.len() <= MAX_LINE_LEN + TRUNCATED_MARKER.len());
        assert!(text.ends_with(TRUNCATED_MARKER.trim_start()));
    }
}
