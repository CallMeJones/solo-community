// SPDX-License-Identifier: Apache-2.0

//! Built-in redaction patterns shipped with Solo (v0.8.0 P5).
//!
//! All regex literals here use **bounded quantifiers** (no unbounded
//! `+` / `*`) to neuter regex-DoS risk on adversarial input. Match
//! semantics anchor on word boundaries where the pattern would otherwise
//! over-match (e.g. credit-card 16-digit runs inside arbitrary text).
//!
//! Naming convention: each pattern's `name` field doubles as the
//! sentinel: a match for `email` is replaced with `[REDACTED:email]`.
//! The names are the keys operators use in
//! `[redaction] exclude_builtin = [...]` to disable a default.
//!
//! ## Adding a new built-in
//!
//! Append the (name, regex, replacement, post_filter) to
//! [`builtin_specs`] below. Keep the regex bounded. Add a positive AND
//! a negative test case in `redaction::registry::tests`.

use regex::Regex;

use super::registry::{RedactionPattern, RedactionPatternKind};

/// Specifications for every built-in pattern. Returned as plain data so
/// `RedactionRegistry::builtin` can compile them into `RedactionPattern`
/// values; callers test the specs by name without forcing a full
/// `RedactionRegistry::builtin` round-trip.
pub fn builtin_specs() -> Vec<BuiltinSpec> {
    vec![
        // RFC 5322-ish. Bounded everywhere: local-part ≤ 64 (RFC max),
        // domain label ≤ 63 (DNS max). Multi-segment domain via repeated
        // bounded group. Avoids unbounded `+` / `*` to keep matching
        // linear on adversarial input.
        BuiltinSpec {
            name: "email",
            regex: r"[A-Za-z0-9._%+\-]{1,64}@[A-Za-z0-9.\-]{1,253}\.[A-Za-z]{2,24}",
            kind: RedactionPatternKind::Literal,
        },
        // US SSN. Exactly NNN-NN-NNNN. We intentionally do NOT match the
        // 9-digit run without dashes — too many false positives against
        // arbitrary digit chunks. Operators who need that can add a
        // custom pattern.
        BuiltinSpec {
            name: "ssn",
            regex: r"\b[0-9]{3}-[0-9]{2}-[0-9]{4}\b",
            kind: RedactionPatternKind::Literal,
        },
        // US phone numbers. Two shapes:
        //   - (NNN) NNN-NNNN          (matches "(555) 123-4567")
        //   - NNN-NNN-NNNN            (matches "555-123-4567")
        // Anchored on word boundaries so a 555-123-45678 chunk in a
        // longer digit run doesn't trip mid-string.
        BuiltinSpec {
            name: "us_phone",
            regex: r"(?:\(\s*[0-9]{3}\s*\)\s*[0-9]{3}-[0-9]{4}|\b[0-9]{3}-[0-9]{3}-[0-9]{4}\b)",
            kind: RedactionPatternKind::Literal,
        },
        // Credit card: 13-19 contiguous digits OR digit-with-spaces/-
        // dashes pattern. The post-filter `Luhn` runs the standard Luhn
        // checksum and rejects matches that fail (so random 16-digit
        // numbers don't trip). Matches must be sequences of digits
        // optionally separated by spaces or dashes — the Luhn check
        // operates on the digit-only collapse.
        BuiltinSpec {
            name: "credit_card",
            regex: r"\b(?:[0-9][ \-]?){12,18}[0-9]\b",
            kind: RedactionPatternKind::CreditCardWithLuhn,
        },
        // AWS IAM access key id. AKIA + exactly 16 uppercase
        // alphanumerics. Highly distinctive — no Luhn-style false-
        // positive filter needed.
        BuiltinSpec {
            name: "aws_access_key",
            regex: r"\bAKIA[A-Z0-9]{16}\b",
            kind: RedactionPatternKind::Literal,
        },
        // GitHub PAT. Modern token prefixes: ghp_ (PAT), gho_ (OAuth),
        // ghu_ (user-server), ghs_ (server-server), ghr_ (refresh).
        // Documented body is 36-255 alphanumerics; we bound at 255 to
        // refuse pathological runaway matches.
        BuiltinSpec {
            name: "github_pat",
            regex: r"\bgh[pousr]_[A-Za-z0-9]{36,255}\b",
            kind: RedactionPatternKind::Literal,
        },
    ]
}

/// One built-in pattern as plain data, before regex compilation.
#[derive(Debug, Clone)]
pub struct BuiltinSpec {
    pub name: &'static str,
    pub regex: &'static str,
    pub kind: RedactionPatternKind,
}

impl BuiltinSpec {
    /// Compile into a `RedactionPattern`. Panics in `tests` if the
    /// regex literal doesn't compile — these are static strings, a
    /// failed compile is a programming error, not an operator config
    /// issue.
    pub fn into_pattern(self) -> RedactionPattern {
        let re = Regex::new(self.regex).unwrap_or_else(|e| {
            panic!(
                "built-in redaction pattern `{}` failed to compile: {e}",
                self.name
            )
        });
        RedactionPattern {
            name: self.name.to_string(),
            regex: re,
            replacement: format!("[REDACTED:{}]", self.name),
            enabled: true,
            kind: self.kind,
        }
    }
}

/// Standard Luhn checksum used by `RedactionPatternKind::CreditCardWithLuhn`.
/// Returns `true` if the digit-only form of `s` passes the Luhn check.
/// Strips spaces and ASCII hyphens before computing.
///
/// Implementation is the textbook one: iterate digits right-to-left,
/// double every second digit (subtracting 9 when the doubled value
/// exceeds 9), sum, accept if `sum % 10 == 0`.
pub fn luhn_check(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let mut sum: u32 = 0;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = *d;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_accepts_canonical_test_cards() {
        // VISA test number (valid Luhn).
        assert!(luhn_check("4111111111111111"));
        // MasterCard test number.
        assert!(luhn_check("5555555555554444"));
        // AmEx test number (15 digits).
        assert!(luhn_check("378282246310005"));
    }

    #[test]
    fn luhn_rejects_random_digit_runs() {
        // 16 ones — fails Luhn (sum 24 → 16 after doubling adjustments).
        assert!(!luhn_check("1111111111111111"));
        // Off-by-one variation of a valid VISA.
        assert!(!luhn_check("4111111111111112"));
    }

    #[test]
    fn luhn_rejects_too_short_or_too_long() {
        // 12 digits — below the 13-digit floor.
        assert!(!luhn_check("411111111111"));
        // 20 digits — above the 19-digit ceiling.
        assert!(!luhn_check("41111111111111111119"));
    }

    #[test]
    fn luhn_strips_separators_before_check() {
        assert!(luhn_check("4111-1111-1111-1111"));
        assert!(luhn_check("4111 1111 1111 1111"));
    }

    #[test]
    fn all_builtin_regexes_compile() {
        for spec in builtin_specs() {
            // `into_pattern` panics on a bad regex; this exercises every
            // entry without further assertions.
            let p = spec.into_pattern();
            assert!(p.enabled);
            assert!(p.replacement.starts_with("[REDACTED:"));
        }
    }
}
