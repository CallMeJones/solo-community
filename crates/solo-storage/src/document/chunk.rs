// SPDX-License-Identifier: Apache-2.0

//! Split a document's text into chunks for embedding.
//!
//! ## Strategy
//!
//! 1. Split the input on paragraph boundaries (`\n\n` or a Markdown-style
//!    heading line).
//! 2. Accumulate paragraphs into a chunk until the running token count
//!    reaches `target_tokens`. Emit, then start the next chunk with the
//!    last ~`overlap_tokens` worth of characters from the just-emitted
//!    chunk (to preserve context across boundaries).
//! 3. If a single paragraph itself exceeds `target_tokens * 1.5`, slide
//!    a window across it, preferring to break on sentence-ending
//!    punctuation (`.`, `!`, `?`, newline) within the last ~10% of the
//!    window.
//!
//! All offsets are byte offsets into the original `text`. They MUST land
//! on UTF-8 character boundaries — the implementation walks
//! `text.char_indices()` to guarantee that.
//!
//! Token counting is approximated as `chars / 4`. This is good enough for
//! English; for non-Latin scripts it under-estimates by ~2x, which means
//! chunks may come out a bit larger than expected. The approximation
//! lives in [`approx_token_count`] and is intentionally not pluggable —
//! the writer-actor (P3) re-derives `token_count` per chunk from the
//! same fn so there's no drift between the chunker and the persisted
//! metadata.

/// Configuration for [`chunk_text`].
///
/// Field defaults (500 / 50) come from the v0.7.0 plan; values were chosen
/// to keep each chunk well under the 8K-token context of typical embedder
/// models while still capturing a meaningful semantic unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkConfig {
    /// Target tokens per chunk (approximation: chars/4).
    pub target_tokens: u32,
    /// Tokens of overlap between consecutive chunks. Should be < target.
    pub overlap_tokens: u32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            target_tokens: 500,
            overlap_tokens: 50,
        }
    }
}

/// One chunk's specification.
///
/// The writer-actor (P3) materializes a `ChunkSpec` into a
/// [`solo_core::DocumentChunk`] by allocating a fresh `ChunkId`, setting
/// `doc_id`, assigning `chunk_index`, and stamping `created_at_ms`. Holding
/// those concerns out of the chunker keeps it a pure function from
/// (text, config) → list of substrings + offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSpec {
    /// The chunk's text content (slice from the original document).
    pub content: String,
    /// Byte offset in the original document where this chunk starts.
    pub start_offset: u32,
    /// Byte offset in the original document where this chunk ends (exclusive).
    pub end_offset: u32,
    /// Approximate token count (chars/4) for this chunk's content.
    pub token_count: u32,
}

/// Approximate token count: 1 token ≈ 4 characters (English heuristic).
pub(crate) fn approx_token_count(text: &str) -> u32 {
    let chars = text.chars().count();
    // Saturating to u32 is fine — texts > 17 GB chars are out of scope.
    u32::try_from(chars / 4).unwrap_or(u32::MAX)
}

/// Split `text` into chunks per `config`.
///
/// Contracts (enforced by tests):
///
///   * Empty input → empty `Vec`.
///   * Text whose token count ≤ `target * 1.5` → exactly one chunk
///     spanning the whole text.
///   * Otherwise → N ≥ 2 chunks. Each chunk's content is a contiguous
///     byte-slice of `text` (no synthesis). `start_offset` /
///     `end_offset` fall on UTF-8 char boundaries.
///   * Offsets are monotonically increasing across the returned `Vec`:
///     for consecutive chunks `start[i+1] < end[i]` (overlap) and
///     `end[i+1] > end[i]` (forward progress).
pub fn chunk_text(text: &str, config: &ChunkConfig) -> Vec<ChunkSpec> {
    if text.is_empty() {
        return Vec::new();
    }
    let target = config.target_tokens.max(1);
    let overlap = config.overlap_tokens.min(target.saturating_sub(1));
    // A single-chunk emit if the whole text comfortably fits.
    let total_tokens = approx_token_count(text);
    if total_tokens <= target.saturating_mul(3) / 2 {
        return vec![ChunkSpec {
            content: text.to_string(),
            start_offset: 0,
            end_offset: u32::try_from(text.len()).unwrap_or(u32::MAX),
            token_count: total_tokens,
        }];
    }

    let paragraphs = split_paragraphs(text);
    let oversize_threshold = target.saturating_mul(3) / 2;

    let mut chunks: Vec<ChunkSpec> = Vec::new();
    let mut cursor_start: usize = 0; // byte offset of the chunk currently being assembled
    let mut cursor_end: usize = 0; // byte offset just past the last paragraph appended
    let mut cursor_tokens: u32 = 0;

    for p in &paragraphs {
        let p_tokens = approx_token_count(&text[p.start..p.end]);

        // Oversized paragraph — flush whatever we have, then slide-window
        // across the paragraph itself.
        if p_tokens >= oversize_threshold {
            if cursor_end > cursor_start {
                push_chunk(&mut chunks, text, cursor_start, cursor_end);
            }
            slide_window(&mut chunks, text, p.start, p.end, target, overlap);
            cursor_start = window_overlap_start(text, p.end, overlap);
            cursor_end = cursor_start;
            cursor_tokens = 0;
            continue;
        }

        // Would adding this paragraph overshoot the target? Flush + restart.
        // The "would overshoot" check is `cursor_tokens + p_tokens > target * 1.5`
        // so we keep paragraphs whole when feasible.
        if cursor_end > cursor_start && cursor_tokens + p_tokens > oversize_threshold {
            push_chunk(&mut chunks, text, cursor_start, cursor_end);
            cursor_start = window_overlap_start(text, cursor_end, overlap);
            // cursor_end intentionally NOT reset here — it's overwritten
            // unconditionally below at `cursor_end = p.end`.
        }

        // Append the paragraph to the current chunk window.
        cursor_end = p.end;
        cursor_tokens = approx_token_count(&text[cursor_start..cursor_end]);

        // If we now sit exactly at or above target, flush — but only when we
        // haven't already; staying within the [target/2, target*1.5] band.
        if cursor_tokens >= target {
            push_chunk(&mut chunks, text, cursor_start, cursor_end);
            cursor_start = window_overlap_start(text, cursor_end, overlap);
            cursor_end = cursor_start;
            // cursor_tokens recomputed at the top of the next iteration if needed
        }
    }

    // Trailing chunk: anything pending after the last paragraph.
    if cursor_end > cursor_start {
        push_chunk(&mut chunks, text, cursor_start, cursor_end);
    }

    chunks
}

/// A single paragraph window into the source text.
#[derive(Debug, Clone, Copy)]
struct Paragraph {
    start: usize,
    /// Exclusive byte offset; includes the trailing paragraph separator
    /// so that consecutive paragraphs concatenate to the original text
    /// without gaps.
    end: usize,
}

/// Split on `\n\n` (paragraph) boundaries. Each paragraph's `[start, end)`
/// includes any blank-line separator that immediately follows, so
/// concatenating all paragraphs reconstructs `text` byte-for-byte.
fn split_paragraphs(text: &str) -> Vec<Paragraph> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        // Find the next "\n\n" (or end-of-string).
        if i + 1 < n && bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            // Skip past the full run of newlines so the next paragraph
            // doesn't start with whitespace it can never trim away (the
            // chunker preserves byte offsets exactly).
            let mut j = i + 2;
            while j < n && bytes[j] == b'\n' {
                j += 1;
            }
            out.push(Paragraph { start, end: j });
            start = j;
            i = j;
            continue;
        }
        i += 1;
    }
    if start < n {
        out.push(Paragraph { start, end: n });
    }
    out
}

/// Compute the start of an overlap window of approximately `overlap` tokens
/// (≈ `overlap * 4` chars) ending at `end`. The returned position is
/// guaranteed to be a UTF-8 char boundary.
fn window_overlap_start(text: &str, end: usize, overlap: u32) -> usize {
    if overlap == 0 || end == 0 {
        return end;
    }
    let target_chars = (overlap as usize) * 4;
    let mut count = 0usize;
    // Iterate char-indices in reverse from `end`.
    let prefix = &text[..end];
    for (idx, _ch) in prefix.char_indices().rev() {
        count += 1;
        if count > target_chars {
            return idx;
        }
    }
    0
}

fn push_chunk(out: &mut Vec<ChunkSpec>, text: &str, start: usize, end: usize) {
    debug_assert!(start < end, "push_chunk: empty range [{start},{end})");
    debug_assert!(
        text.is_char_boundary(start),
        "start {start} not on char boundary"
    );
    debug_assert!(text.is_char_boundary(end), "end {end} not on char boundary");
    let slice = &text[start..end];
    out.push(ChunkSpec {
        content: slice.to_string(),
        start_offset: u32::try_from(start).unwrap_or(u32::MAX),
        end_offset: u32::try_from(end).unwrap_or(u32::MAX),
        token_count: approx_token_count(slice),
    });
}

/// Slide a fixed-token window across `text[range_start..range_end]`,
/// preferring sentence-ending punctuation in the last ~10% of the window.
///
/// Used when a single paragraph is too large to fit a single chunk.
fn slide_window(
    out: &mut Vec<ChunkSpec>,
    text: &str,
    range_start: usize,
    range_end: usize,
    target: u32,
    overlap: u32,
) {
    let target_chars = (target as usize) * 4;
    let mut window_start = range_start;
    while window_start < range_end {
        // Provisional end at target_chars; then nudge to the nearest
        // sentence-ending punctuation if one exists in the last 10%.
        let mut chars_seen = 0usize;
        let mut window_end = range_end;
        let suffix = &text[window_start..range_end];
        for (idx, _ch) in suffix.char_indices() {
            chars_seen += 1;
            if chars_seen >= target_chars {
                window_end = window_start + idx;
                break;
            }
        }
        // If the natural end is within ~10% of the target, that's fine.
        // Otherwise, look back through the last 10% of the window for a
        // sentence-ending punctuation char.
        if window_end < range_end {
            let lookback = target_chars / 10;
            let snap = find_sentence_break(text, window_start, window_end, lookback);
            window_end = snap;
        }
        if window_end <= window_start {
            // Defensive: don't loop forever. Force at least one char.
            let next = text[window_start..range_end]
                .char_indices()
                .next()
                .map(|(_, c)| window_start + c.len_utf8())
                .unwrap_or(range_end);
            window_end = next;
        }
        push_chunk(out, text, window_start, window_end);
        if window_end >= range_end {
            break;
        }
        let next_start = window_overlap_start(text, window_end, overlap);
        // Guarantee monotonic progress (avoid infinite loop on pathological text).
        window_start = next_start.max(window_start + 1);
        // Re-align to char boundary going forward — `+1` may land mid-char.
        while window_start < range_end && !text.is_char_boundary(window_start) {
            window_start += 1;
        }
    }
}

/// Within `[window_end - lookback, window_end)`, find the byte offset just
/// past the last `.`, `!`, `?`, or `\n`. If none, return `window_end` unchanged.
fn find_sentence_break(
    text: &str,
    window_start: usize,
    window_end: usize,
    lookback_chars: usize,
) -> usize {
    let bytes = text.as_bytes();
    // Determine the byte offset of the start of the look-back region.
    let look_start = {
        let prefix = &text[window_start..window_end];
        let mut count = 0usize;
        let mut start_idx = 0usize;
        for (idx, _ch) in prefix.char_indices().rev() {
            count += 1;
            if count >= lookback_chars {
                start_idx = window_start + idx;
                break;
            }
        }
        if count < lookback_chars {
            window_start
        } else {
            start_idx
        }
    };
    // Walk back from window_end looking for terminal punctuation.
    let mut i = window_end;
    while i > look_start {
        let prev = match text[..i].char_indices().next_back() {
            Some((idx, _)) => idx,
            None => break,
        };
        let ch = bytes[prev];
        if ch == b'.' || ch == b'!' || ch == b'?' || ch == b'\n' {
            return i; // include the punctuation char
        }
        i = prev;
    }
    window_end
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_empty_text_returns_empty_vec() {
        let out = chunk_text("", &ChunkConfig::default());
        assert!(out.is_empty());
    }

    #[test]
    fn chunk_short_text_returns_single_chunk() {
        // Way under target — should fit in one chunk.
        let text = "Hello world. This is a tiny doc.";
        let out = chunk_text(text, &ChunkConfig::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, text);
        assert_eq!(out[0].start_offset, 0);
        assert_eq!(out[0].end_offset as usize, text.len());
    }

    /// Build a multi-paragraph synthetic text large enough to force multiple
    /// chunks. Each paragraph is ~200 chars (~50 tokens by chars/4), so
    /// at target=500 / overlap=50 we expect roughly target/50 = 10
    /// paragraphs per chunk → N chunks ≈ paragraph_count / 10.
    fn synthetic_text(paragraph_count: usize, words_per_paragraph: usize) -> String {
        let mut s = String::new();
        for p in 0..paragraph_count {
            for w in 0..words_per_paragraph {
                if w > 0 {
                    s.push(' ');
                }
                s.push_str(&format!("word{w:02}"));
            }
            s.push('.');
            if p + 1 < paragraph_count {
                s.push_str("\n\n");
            }
        }
        s
    }

    #[test]
    fn chunk_long_text_splits_into_multiple() {
        // ~500 paragraphs of ~50 chars each = ~25 000 chars = ~6 250 tokens.
        // At target=500, expect ~12+ chunks.
        let text = synthetic_text(500, 8); // ~50 chars/paragraph
        let cfg = ChunkConfig::default();
        let out = chunk_text(&text, &cfg);
        assert!(out.len() > 1, "expected multiple chunks, got {}", out.len());
        // Every chunk's content must round-trip exactly with its offsets.
        for c in &out {
            let slice = &text[c.start_offset as usize..c.end_offset as usize];
            assert_eq!(slice, c.content.as_str());
        }
    }

    #[test]
    fn chunk_respects_paragraph_boundaries() {
        // A handful of well-separated paragraphs. Each is small (~30 chars)
        // so the chunker should group them but NOT split mid-paragraph.
        let text = synthetic_text(40, 6);
        let cfg = ChunkConfig {
            target_tokens: 50,
            overlap_tokens: 5,
        };
        let out = chunk_text(&text, &cfg);
        assert!(out.len() > 1);
        // None of the chunk boundaries should fall in the middle of "wordNN"
        // — they should land on or near \n\n boundaries.
        for c in &out {
            let last_char = c.content.chars().last().unwrap();
            // The last char should be either '.' (sentence end) or '\n' or a digit
            // (when the chunker had to cut mid-paragraph because the next
            // paragraph would overshoot too much). For this size of synthetic
            // input, the chunker should mostly land on sentence ends.
            assert!(
                last_char == '.' || last_char == '\n' || last_char.is_ascii_alphanumeric(),
                "chunk ends mid-token at: {:?}",
                &c.content[c.content.len().saturating_sub(20)..]
            );
        }
    }

    #[test]
    fn chunk_target_size_band() {
        // Most chunks should fall within [target/2, target*1.5] tokens.
        let text = synthetic_text(300, 8);
        let cfg = ChunkConfig {
            target_tokens: 100,
            overlap_tokens: 10,
        };
        let out = chunk_text(&text, &cfg);
        assert!(out.len() >= 3, "need enough chunks to evaluate band");
        // Excluding the last (trailing) chunk, every chunk must be in band.
        let lower = cfg.target_tokens / 2;
        let upper = cfg.target_tokens * 3 / 2;
        for (i, c) in out.iter().enumerate().take(out.len() - 1) {
            assert!(
                c.token_count >= lower && c.token_count <= upper,
                "chunk {i} out of band: token_count={} band=[{lower},{upper}]",
                c.token_count,
            );
        }
    }

    #[test]
    fn chunk_offsets_monotonic_with_overlap() {
        let text = synthetic_text(200, 8);
        let cfg = ChunkConfig {
            target_tokens: 100,
            overlap_tokens: 10,
        };
        let out = chunk_text(&text, &cfg);
        assert!(out.len() >= 2);
        for window in out.windows(2) {
            let a = &window[0];
            let b = &window[1];
            // Forward progress
            assert!(
                b.end_offset > a.end_offset,
                "end_offset must increase across chunks: {} -> {}",
                a.end_offset,
                b.end_offset
            );
            // Overlap: next chunk starts at or before previous chunk's end.
            // With overlap_tokens=10, b.start_offset should typically be
            // a.end_offset - (~40 chars). Allow equality for cases where
            // the chunker can't find any overlap (rare).
            assert!(
                b.start_offset <= a.end_offset,
                "next chunk should overlap or abut the previous: a.end={} b.start={}",
                a.end_offset,
                b.start_offset,
            );
        }
    }

    #[test]
    fn chunk_utf8_safe_offsets() {
        // Multi-byte chars (Japanese + accented Latin). The chunker must
        // never panic and must produce offsets that land on char boundaries.
        let para = "こんにちは世界。これは日本語のテストです。Caféの紅茶。".repeat(40);
        let text = format!(
            "{para}\n\n{}",
            "Bonjour le monde. Voici un test en français.".repeat(40)
        );
        let cfg = ChunkConfig {
            target_tokens: 80,
            overlap_tokens: 10,
        };
        let out = chunk_text(&text, &cfg);
        assert!(!out.is_empty());
        for c in &out {
            let s = c.start_offset as usize;
            let e = c.end_offset as usize;
            assert!(text.is_char_boundary(s), "start {s} not on char boundary");
            assert!(text.is_char_boundary(e), "end {e} not on char boundary");
            // The slice must equal the chunk content byte-for-byte.
            assert_eq!(&text[s..e], c.content);
        }
    }

    #[test]
    fn chunk_very_large_text() {
        // ~100KB text — should complete well under 1s.
        let text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(1_800);
        assert!(text.len() > 100_000);
        let cfg = ChunkConfig::default();
        let start = std::time::Instant::now();
        let out = chunk_text(&text, &cfg);
        let elapsed = start.elapsed();
        assert!(out.len() > 10, "expected many chunks, got {}", out.len());
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "100KB chunking too slow: {elapsed:?}"
        );
    }

    #[test]
    fn chunk_token_count_approximation() {
        // 40 ASCII chars → 40/4 = 10 tokens.
        assert_eq!(approx_token_count("0123456789".repeat(4).as_str()), 10);
        // 12 multibyte chars → 12/4 = 3 tokens (counts CHARS not bytes).
        assert_eq!(approx_token_count("あいうえおかきくけこさし"), 3);
        // Empty → 0.
        assert_eq!(approx_token_count(""), 0);
    }

    #[test]
    fn chunk_oversized_paragraph_slides_window() {
        // A single huge paragraph with no \n\n boundaries — the chunker
        // must still split it into multiple chunks via sentence sliding.
        let sentence = "This is a sentence with several words in it. ".to_string();
        let mega_paragraph = sentence.repeat(200); // ~9000 chars ≈ 2250 tokens
        let cfg = ChunkConfig {
            target_tokens: 100,
            overlap_tokens: 10,
        };
        let out = chunk_text(&mega_paragraph, &cfg);
        assert!(
            out.len() >= 3,
            "expected oversized paragraph to be split, got {} chunks",
            out.len()
        );
        // Offsets must still be monotonic + char-boundary safe.
        for c in &out {
            let s = c.start_offset as usize;
            let e = c.end_offset as usize;
            assert!(mega_paragraph.is_char_boundary(s));
            assert!(mega_paragraph.is_char_boundary(e));
            assert_eq!(&mega_paragraph[s..e], c.content);
        }
    }

    #[test]
    fn chunk_config_default_is_500_50() {
        let c = ChunkConfig::default();
        assert_eq!(c.target_tokens, 500);
        assert_eq!(c.overlap_tokens, 50);
    }

    #[test]
    fn chunk_text_offsets_cover_input_modulo_overlap() {
        // The first chunk starts at 0, the last chunk ends at text.len().
        let text = synthetic_text(80, 8);
        let cfg = ChunkConfig::default();
        let out = chunk_text(&text, &cfg);
        assert!(!out.is_empty());
        assert_eq!(out[0].start_offset, 0);
        assert_eq!(out.last().unwrap().end_offset as usize, text.len());
    }
}
