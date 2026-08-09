// SPDX-License-Identifier: Apache-2.0

//! Compiled redaction registry — the runtime form used by the writer.
//!
//! v0.8.0 P5. See `super` module docs for the design rationale.

use std::borrow::Cow;

use regex::Regex;
use solo_core::{Error, Result};

use super::builtins;
use crate::config::RedactionConfig;

/// Discriminator for special post-match filters. Most patterns are
/// `Literal` — any regex match is a redaction. Credit-card numbers run
/// a Luhn post-filter so 16-digit non-CCs (UUID literal chunks, etc.)
/// don't trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionPatternKind {
    /// Every regex match is redacted as-is.
    Literal,
    /// Match candidates must additionally pass the Luhn checksum.
    CreditCardWithLuhn,
}

/// One redaction pattern. Built either from a built-in (via
/// [`super::builtins::builtin_specs`]) or from
/// `[[redaction.custom]]` config entries (via [`RedactionRegistry::from_config`]).
#[derive(Debug)]
pub struct RedactionPattern {
    /// Stable identifier for the pattern. Used as the sentinel suffix
    /// (`[REDACTED:<name>]`) and as the audit-row count key.
    pub name: String,
    pub regex: Regex,
    /// Replacement string. Defaults to `[REDACTED:<name>]`; operators
    /// can override per-pattern via the config.
    pub replacement: String,
    pub enabled: bool,
    pub kind: RedactionPatternKind,
}

/// Compiled redaction registry. Cheap to clone the inner state if
/// needed, but the writer-actor holds exactly one of these and threads
/// `&self` into every write.
#[derive(Debug)]
pub struct RedactionRegistry {
    patterns: Vec<RedactionPattern>,
    enabled: bool,
}

/// Per-pattern match count returned from [`RedactionRegistry::redact`].
/// Carries the pattern's `name` and the number of substring matches
/// redacted. The matched substrings themselves are NEVER captured here
/// — the audit-row builder relies on this struct never carrying PII.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionMatch {
    pub pattern_name: String,
    pub count: u32,
}

/// Result of one [`RedactionRegistry::redact`] call. Carries either the
/// original `&str` borrow (when nothing matched) or an owned `String`
/// with the redactions applied. `matches` is empty when no pattern fired.
pub struct RedactionResult<'a> {
    pub text: Cow<'a, str>,
    pub matches: Vec<RedactionMatch>,
}

impl RedactionRegistry {
    /// Returns the built-in registry with every default pattern enabled
    /// AND the registry itself enabled. Used by tests that want the
    /// full default behaviour without going through config.
    pub fn builtin() -> Self {
        let patterns: Vec<RedactionPattern> = builtins::builtin_specs()
            .into_iter()
            .map(|s| s.into_pattern())
            .collect();
        Self {
            patterns,
            enabled: true,
        }
    }

    /// Build a registry from operator config. The registry is enabled
    /// iff `cfg.enabled == true`. Built-ins land first (filtered by
    /// `cfg.exclude_builtin`); custom patterns follow. Custom-pattern
    /// regex literals are compiled here — failures are returned as
    /// `Error::Storage` so `LibraryHandle::open` can surface them
    /// cleanly.
    pub fn from_config(cfg: &RedactionConfig) -> Result<Self> {
        let mut patterns: Vec<RedactionPattern> = Vec::new();
        for spec in builtins::builtin_specs() {
            if cfg.exclude_builtin.iter().any(|n| n == spec.name) {
                continue;
            }
            patterns.push(spec.into_pattern());
        }
        for custom in &cfg.custom {
            let re = Regex::new(&custom.regex).map_err(|e| {
                Error::storage(format!(
                    "redaction.custom[{}].regex compile failed: {e}",
                    custom.name
                ))
            })?;
            let replacement = custom
                .replacement
                .clone()
                .unwrap_or_else(|| format!("[REDACTED:{}]", custom.name));
            patterns.push(RedactionPattern {
                name: custom.name.clone(),
                regex: re,
                replacement,
                enabled: true,
                kind: RedactionPatternKind::Literal,
            });
        }
        Ok(Self {
            patterns,
            enabled: cfg.enabled,
        })
    }

    /// `true` iff the registry is enabled AND has at least one pattern.
    /// The writer checks this on every call so a disabled registry adds
    /// effectively no overhead (one boolean read).
    pub fn is_enabled(&self) -> bool {
        self.enabled && !self.patterns.is_empty()
    }

    /// Run every enabled pattern over `input`. On no-match returns a
    /// `Cow::Borrowed` over the original string; otherwise an owned
    /// `String` with the matched substrings replaced.
    ///
    /// Matches counts are aggregated per pattern_name; the writer
    /// emits one audit row per write summarising all firing patterns.
    pub fn redact<'a>(&self, input: &'a str) -> RedactionResult<'a> {
        if !self.is_enabled() {
            return RedactionResult {
                text: Cow::Borrowed(input),
                matches: Vec::new(),
            };
        }
        let mut current: Cow<'a, str> = Cow::Borrowed(input);
        let mut matches: Vec<RedactionMatch> = Vec::new();
        for pattern in self.patterns.iter().filter(|p| p.enabled) {
            let count = match pattern.kind {
                RedactionPatternKind::Literal => {
                    let n = pattern.regex.find_iter(&current).count();
                    if n == 0 {
                        continue;
                    }
                    let replaced = pattern
                        .regex
                        .replace_all(&current, pattern.replacement.as_str())
                        .into_owned();
                    current = Cow::Owned(replaced);
                    n as u32
                }
                RedactionPatternKind::CreditCardWithLuhn => {
                    // Two-pass: first collect every match's range, then
                    // filter by Luhn, then rebuild the string with only
                    // the surviving matches replaced. We can't use the
                    // simple `replace_all` path because the Luhn filter
                    // is per-match.
                    let candidates: Vec<(usize, usize)> = pattern
                        .regex
                        .find_iter(&current)
                        .map(|m| (m.start(), m.end()))
                        .collect();
                    if candidates.is_empty() {
                        continue;
                    }
                    let owned: String = current.into_owned();
                    let mut rebuilt = String::with_capacity(owned.len());
                    let mut cursor = 0usize;
                    let mut kept = 0u32;
                    for (s, e) in candidates {
                        // Skip overlapping match leftovers; regex_iter
                        // returns non-overlapping matches but the
                        // cursor advances may still be ahead if the
                        // string was modified — defensive guard.
                        if s < cursor {
                            continue;
                        }
                        rebuilt.push_str(&owned[cursor..s]);
                        let candidate = &owned[s..e];
                        if super::builtins::luhn_check(candidate) {
                            rebuilt.push_str(&pattern.replacement);
                            kept += 1;
                        } else {
                            rebuilt.push_str(candidate);
                        }
                        cursor = e;
                    }
                    rebuilt.push_str(&owned[cursor..]);
                    current = Cow::Owned(rebuilt);
                    if kept == 0 {
                        continue;
                    }
                    kept
                }
            };
            matches.push(RedactionMatch {
                pattern_name: pattern.name.clone(),
                count,
            });
        }
        RedactionResult {
            text: current,
            matches,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> RedactionRegistry {
        RedactionRegistry::builtin()
    }

    #[test]
    fn email_pattern_redacts_canonical_address() {
        let out = r().redact("write me at foo@bar.com please");
        assert!(out.text.contains("[REDACTED:email]"));
        assert!(!out.text.contains("foo@bar.com"));
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].pattern_name, "email");
        assert_eq!(out.matches[0].count, 1);
    }

    #[test]
    fn ssn_pattern_redacts_hyphenated_form() {
        let out = r().redact("ssn is 123-45-6789 ok");
        assert!(out.text.contains("[REDACTED:ssn]"));
        assert!(!out.text.contains("123-45-6789"));
    }

    #[test]
    fn us_phone_pattern_redacts_parenthesized_and_dashed() {
        let r = r();
        let a = r.redact("call (555) 123-4567 today");
        assert!(a.text.contains("[REDACTED:us_phone]"), "got `{}`", a.text);
        assert!(!a.text.contains("(555) 123-4567"));
        let b = r.redact("call 555-123-4567 today");
        assert!(b.text.contains("[REDACTED:us_phone]"));
        assert!(!b.text.contains("555-123-4567"));
    }

    #[test]
    fn credit_card_pattern_redacts_valid_but_not_invalid() {
        let r = r();
        // Valid VISA test card.
        let ok = r.redact("card 4111111111111111 charged");
        assert!(
            ok.text.contains("[REDACTED:credit_card]"),
            "got `{}`",
            ok.text
        );
        assert!(!ok.text.contains("4111111111111111"));
        // Invalid digit run of same length — must NOT match.
        let bad = r.redact("digits 1234567890123456 here");
        assert!(
            !bad.text.contains("[REDACTED:credit_card]"),
            "got `{}`",
            bad.text
        );
        assert!(bad.text.contains("1234567890123456"));
    }

    #[test]
    fn aws_access_key_pattern_redacts() {
        let key = ["AK", "IAIOSFODNN7EXAMPLE"].concat();
        let input = format!("set {key} in env");
        let out = r().redact(&input);
        assert!(out.text.contains("[REDACTED:aws_access_key]"));
        assert!(!out.text.contains(&key));
    }

    #[test]
    fn github_access_token_pattern_redacts_synthetic_token() {
        // Synthetic ghp_ + 36 chars.
        let token = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij12"].concat();
        let input = format!("token={token} ok");
        let out = r().redact(&input);
        assert!(
            out.text.contains("[REDACTED:github_pat]"),
            "got `{}`",
            out.text
        );
        assert!(!out.text.contains(&token));
    }

    #[test]
    fn exclude_builtin_disables_named_pattern() {
        let cfg = RedactionConfig {
            enabled: true,
            exclude_builtin: vec!["email".to_string()],
            custom: Vec::new(),
        };
        let reg = RedactionRegistry::from_config(&cfg).unwrap();
        let out = reg.redact("write foo@bar.com here");
        assert!(
            out.text.contains("foo@bar.com"),
            "email must NOT be redacted; got `{}`",
            out.text
        );
        assert!(out.matches.is_empty());
    }

    #[test]
    fn custom_pattern_redacts() {
        let cfg = RedactionConfig {
            enabled: true,
            exclude_builtin: Vec::new(),
            custom: vec![crate::config::CustomRedactionPattern {
                name: "internal_id".to_string(),
                regex: r"INT-[0-9]{6}".to_string(),
                replacement: None,
            }],
        };
        let reg = RedactionRegistry::from_config(&cfg).unwrap();
        let out = reg.redact("ticket INT-123456 filed");
        assert!(out.text.contains("[REDACTED:internal_id]"));
        assert!(!out.text.contains("INT-123456"));
    }

    #[test]
    fn disabled_registry_is_passthrough() {
        let cfg = RedactionConfig {
            enabled: false,
            ..Default::default()
        };
        let reg = RedactionRegistry::from_config(&cfg).unwrap();
        let out = reg.redact("foo@bar.com sees raw text");
        assert!(out.text.contains("foo@bar.com"));
        assert!(out.matches.is_empty());
        assert!(matches!(out.text, Cow::Borrowed(_)));
    }

    #[test]
    fn empty_input_returns_borrowed() {
        let out = r().redact("");
        assert_eq!(out.text, "");
        assert!(out.matches.is_empty());
        assert!(matches!(out.text, Cow::Borrowed(_)));
    }

    #[test]
    fn invalid_custom_regex_errors_cleanly() {
        let cfg = RedactionConfig {
            enabled: true,
            exclude_builtin: Vec::new(),
            custom: vec![crate::config::CustomRedactionPattern {
                name: "broken".to_string(),
                regex: r"[invalid".to_string(),
                replacement: None,
            }],
        };
        let err = RedactionRegistry::from_config(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("broken"),
            "error must name the offender: `{msg}`"
        );
        assert!(msg.contains("redaction.custom"), "got `{msg}`");
    }

    #[test]
    fn audit_match_struct_carries_only_pattern_name_and_count() {
        let out = r().redact("email foo@bar.com and ssn 123-45-6789");
        for m in &out.matches {
            // Verify by exhaustive destructuring — if the struct grew a
            // PII-carrying field, this would fail to compile and force
            // a deliberate decision.
            let RedactionMatch {
                pattern_name,
                count,
            } = m;
            assert!(!pattern_name.contains("foo@bar.com"));
            assert!(!pattern_name.contains("123-45-6789"));
            assert!(*count >= 1);
        }
    }

    #[test]
    fn multiple_patterns_in_one_input_all_record_counts() {
        let token = ["gh", "p_", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"].concat();
        let input = format!("alice@x.com (555) 123-4567 {token}");
        let out = r().redact(&input);
        let names: Vec<&str> = out
            .matches
            .iter()
            .map(|m| m.pattern_name.as_str())
            .collect();
        assert!(names.contains(&"email"));
        assert!(names.contains(&"us_phone"));
        assert!(names.contains(&"github_pat"));
    }
}
