// SPDX-License-Identifier: Apache-2.0

//! Bearer-token validation. Forward port of v0.7.x's
//! `ValidateRequestHeaderLayer::custom(BearerToken::new(token))` flow,
//! re-shaped to emit an [`AuthenticatedPrincipal`] so downstream layers
//! (such as the audit log) can treat bearer + OIDC identically.

use super::{AuthError, AuthenticatedPrincipal};
/// Stateless validator. Compare the `Authorization: Bearer <token>`
/// header against the expected token.
///
/// Dev-log 0152 M11: the token comparison is constant-time over the
/// length of `expected_token`. Length differences ARE observable — but
/// length-leak alone doesn't recover the token (32-byte recommended
/// minimum entropy). The byte-comparison accumulator pattern below
/// avoids adding a `subtle` crate dependency for this single use.
#[derive(Debug, Clone)]
pub struct BearerValidator {
    expected_token: String,
}

impl BearerValidator {
    pub fn new(expected_token: String) -> Self {
        Self { expected_token }
    }

    /// Validate the value of an Authorization header. `None` is treated
    /// as a missing header (same as malformed).
    pub fn validate(&self, header: Option<&str>) -> Result<AuthenticatedPrincipal, AuthError> {
        let header = header.ok_or(AuthError::MissingAuthHeader)?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or(AuthError::MalformedAuthHeader)?;
        if !constant_time_eq(token.as_bytes(), self.expected_token.as_bytes()) {
            return Err(AuthError::InvalidBearer);
        }
        Ok(AuthenticatedPrincipal::bearer())
    }
}

/// Constant-time byte-slice equality (dev-log 0152 M11, dev-log 0154
/// post-fix tightening). Runs in time proportional to `expected.len()`
/// regardless of how many bytes match. Length mismatch returns false
/// immediately — length-leak is acceptable for ≥32-byte tokens; the
/// secret bytes are protected.
///
/// The `std::hint::black_box` calls prevent the optimiser from
/// unrolling the loop into data-dependent branches or short-circuiting
/// the accumulator. We don't pull in the `subtle` crate (the only other
/// constant-time primitive Solo needs is this one) but black-boxing
/// the inputs + the accumulator achieves the same effect at the IR
/// level. A future hardening could swap in `subtle::ConstantTimeEq` if
/// other comparison sites appear.
fn constant_time_eq(actual: &[u8], expected: &[u8]) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (a, e) in actual.iter().zip(expected.iter()) {
        // black_box on both operands prevents the optimiser from
        // reordering or short-circuiting based on early matches.
        let a = std::hint::black_box(*a);
        let e = std::hint::black_box(*e);
        diff |= a ^ e;
    }
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validator() -> BearerValidator {
        BearerValidator::new("s3cr3t".to_string())
    }

    #[test]
    fn accepts_correct_token() {
        let v = validator();
        let p = v.validate(Some("Bearer s3cr3t")).expect("accept");
        assert_eq!(p.subject, "bearer");
        assert!(p.scopes.is_empty());
        assert!(p.claims.is_null());
    }

    #[test]
    fn rejects_missing_header() {
        let v = validator();
        let err = v.validate(None).unwrap_err();
        assert!(matches!(err, AuthError::MissingAuthHeader), "got {err:?}");
    }

    #[test]
    fn rejects_malformed_header_no_bearer_prefix() {
        let v = validator();
        // No "Bearer " prefix — looks like a Basic auth header.
        let err = v.validate(Some("Basic s3cr3t")).unwrap_err();
        assert!(matches!(err, AuthError::MalformedAuthHeader), "got {err:?}");
        // No prefix at all.
        let err = v.validate(Some("s3cr3t")).unwrap_err();
        assert!(matches!(err, AuthError::MalformedAuthHeader), "got {err:?}");
    }

    #[test]
    fn rejects_wrong_token() {
        let v = validator();
        let err = v.validate(Some("Bearer wrong-token")).unwrap_err();
        assert!(matches!(err, AuthError::InvalidBearer), "got {err:?}");
    }
}
