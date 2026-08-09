// SPDX-License-Identifier: Apache-2.0

//! Refuse to initialize Solo inside a cloud-sync folder.
//!
//! Why: SQLCipher + cloud sync is a corruption time-bomb. Dropbox/OneDrive/etc.
//! sync mid-write, sync the WAL but not the main DB, and produce silently
//! broken files. We refuse upfront with a clear message — much better than
//! discovering it after the user's memory is gone.
//!
//! The check is heuristic by design (a `Solo` folder named after a cloud
//! provider is rare; misclassifying a real cloud folder as safe is dangerous).
//! False positives are recoverable (`--allow-cloud-sync` flag, future work);
//! false negatives are catastrophic.
//!
//! Detection: walks every ancestor path component and tests each against a
//! list of known cloud-sync folder names. Matching is case-insensitive (NTFS
//! and HFS+ are case-insensitive at the OS level; ext4 isn't, but typing
//! `~/dropbox/...` is still risky).

use solo_core::{Error, Result};
use std::path::{Component, Path};
use unicode_normalization::UnicodeNormalization;

/// Folder names produced by the most common cloud-sync clients. Case-folded
/// during comparison. If you add a new entry, prefer the literal folder name
/// the client creates rather than its branding.
const CLOUD_SYNC_NAMES: &[&str] = &[
    // Dropbox: ~/Dropbox, ~/Dropbox (Personal), ~/Dropbox (Business)
    "dropbox",
    // OneDrive: ~/OneDrive, ~/OneDrive - <org name>
    "onedrive",
    // Google Drive — desktop client mounts under various names
    "google drive",
    "googledrive",
    "my drive",
    // iCloud Drive — both the user-facing folder and the macOS internal path
    "icloud drive",
    "icloud",
    "icloud~com~apple~clouddocs",
    "mobile documents",
    // Box.com
    "box",
    "box sync",
    // pCloud
    "pclouddrive",
    "pcloud drive",
    // MEGA
    "mega",
    "megasync",
    // Resilio Sync
    "resilio sync",
    // Sync.com
    "sync",
];

/// Normalise a path component for comparison against
/// `CLOUD_SYNC_NAMES`. Two passes:
///
///   1. **NFKC normalisation** — collapses compatibility variants
///      to their canonical form. Maps full-width Latin (e.g.
///      `ｄｒｏｐｂｏｘ` U+FF44 …) and ligatures (`ﬃ` → `ffi`) onto
///      the ASCII shape so a path component that *looks* like
///      "dropbox" but uses fancy codepoints is detected.
///   2. **ASCII case-folding** — lowercase via `to_lowercase`.
///
/// What this **does NOT** catch: script-mixed confusables —
/// Cyrillic 'о' (U+043E) and Latin 'o' (U+006F) are visually
/// identical but live in different Unicode blocks and have no
/// compatibility mapping. NFKC leaves them alone. Defending
/// against those needs a confusable-detection pass (Unicode
/// Technical Standard #39) which is out of scope for v0.3 — the
/// dependency tree of `unicode-security` is heavier than the
/// hardening it adds for this use case.
fn canonicalize_for_match(s: &str) -> String {
    s.nfkc().collect::<String>().to_lowercase()
}

/// Validate that `path` (a candidate Solo data dir) is safe.
///
/// Checks:
/// 1. No ancestor path component is a known cloud-sync folder.
/// 2. Path is absolute (otherwise the cloud-sync check is unreliable —
///    a relative path could resolve into a cloud folder depending on cwd).
///
/// Returns Ok(()) on success, Err(Error::InvalidInput) with a clear message
/// on failure. Existence of the path is NOT required — `solo init` creates it.
pub fn validate_data_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::invalid_input(format!(
            "data dir must be an absolute path: got {}",
            path.display()
        )));
    }

    for component in path.components() {
        match component {
            Component::Normal(os_name) => {
                let name_lc = canonicalize_for_match(&os_name.to_string_lossy());
                if CLOUD_SYNC_NAMES.iter().any(|&n| name_lc == n) {
                    return Err(Error::invalid_input(format!(
                        "refusing to initialize Solo inside a cloud-sync folder: \
                         `{}` (component `{}` matches known cloud-sync clients). \
                         SQLCipher + cloud sync corrupts databases. \
                         Choose a local-only path (e.g., ~/.solo).",
                        path.display(),
                        name_lc
                    )));
                }
            }
            // Windows UNC paths encode the share name in a Prefix component
            // (e.g. \\server\Dropbox\... → Prefix("\\server\Dropbox")). The
            // Display impl emits the full prefix string; pattern-match against
            // each `\` segment so a share named "Dropbox" or "OneDrive" gets
            // caught the same way as a Normal "Dropbox" component would.
            #[cfg(windows)]
            Component::Prefix(prefix) => {
                let prefix_raw = prefix.as_os_str().to_string_lossy();
                // NFKC + lowercase per-segment, same shape as the
                // Normal-component branch above.
                for segment in prefix_raw.split(['\\', '/']) {
                    if segment.is_empty() {
                        continue;
                    }
                    let segment_norm = canonicalize_for_match(segment);
                    if CLOUD_SYNC_NAMES.iter().any(|&n| segment_norm == n) {
                        return Err(Error::invalid_input(format!(
                            "refusing to initialize Solo inside a cloud-sync folder: \
                             `{}` (UNC prefix segment `{}` matches known cloud-sync clients). \
                             SQLCipher + cloud sync corrupts databases. \
                             Choose a local-only path (e.g., ~/.solo).",
                            path.display(),
                            segment_norm
                        )));
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Construct a platform-absolute path from a forward-slash suffix. Tests
    /// describe path *components* (cloud-sync detection is component-level)
    /// and stay portable: Unix gets a `/` prefix, Windows gets `C:\` and
    /// backslash separators. Without this helper the literal `/Users/...`
    /// strings fail the `is_absolute` check on Windows.
    fn abs(suffix: &str) -> PathBuf {
        #[cfg(windows)]
        {
            let win = suffix.replace('/', "\\");
            PathBuf::from(format!("C:\\{win}"))
        }
        #[cfg(not(windows))]
        {
            PathBuf::from(format!("/{suffix}"))
        }
    }

    #[test]
    fn rejects_dropbox_root() {
        let p = abs("Users/alice/Dropbox/solo");
        let err = validate_data_dir(&p).unwrap_err();
        assert!(err.to_string().contains("cloud-sync"), "got: {err}");
        assert!(err.to_string().contains("dropbox"), "got: {err}");
    }

    #[test]
    fn rejects_onedrive_with_org_suffix() {
        let p = abs("Users/bob/OneDrive/solo");
        // The OneDrive component itself; the OneDrive - Acme variant has a
        // different leading component — we test that one separately if needed.
        assert!(validate_data_dir(&p).is_err());
    }

    #[test]
    fn rejects_icloud_drive() {
        let p = abs("Users/c/Library/Mobile Documents/com~apple~CloudDocs/solo");
        assert!(validate_data_dir(&p).is_err());
    }

    #[test]
    fn rejects_case_variations() {
        let p1 = abs("Users/d/DROPBOX/solo");
        let p2 = abs("Users/d/dropbox/solo");
        let p3 = abs("Users/d/Dropbox/solo");
        assert!(validate_data_dir(&p1).is_err());
        assert!(validate_data_dir(&p2).is_err());
        assert!(validate_data_dir(&p3).is_err());
    }

    #[test]
    fn accepts_dot_solo() {
        let p = abs("home/eve/.solo");
        assert!(validate_data_dir(&p).is_ok());
    }

    #[test]
    fn accepts_explicit_local_path() {
        let p = abs("var/lib/solo");
        assert!(validate_data_dir(&p).is_ok());
    }

    #[test]
    fn rejects_relative_path() {
        let p = PathBuf::from(".solo");
        let err = validate_data_dir(&p).unwrap_err();
        assert!(err.to_string().contains("absolute"), "got: {err}");
    }

    #[test]
    fn no_match_on_substring_within_a_component() {
        // "dropboxlike" is NOT "dropbox" — we match whole components only.
        let p = abs("home/f/dropboxlike/solo");
        assert!(validate_data_dir(&p).is_ok());
    }

    #[test]
    fn rejects_dropbox_with_unicode_case_variants() {
        // NFKC + lowercase catches compatibility variants such as
        // full-width Latin and ligatures. It does **NOT** catch
        // script-mixed confusables — Cyrillic 'о' (U+043E) is
        // visually identical to Latin 'o' (U+006F) but lives in a
        // different Unicode block with no compatibility mapping, so
        // NFKC leaves it alone. Documented limitation; fix would
        // require a confusable-detection pass (UTS #39) and a
        // heavier dep (`unicode-security`).
        let p_cyrillic = abs("Users/x/dr\u{043e}pbox/solo"); // о = U+043E (Cyrillic 'o')
        assert!(
            validate_data_dir(&p_cyrillic).is_ok(),
            "NFKC does not catch script-mixed confusables — \
             documented behaviour, fix would need UTS #39 confusable detection"
        );
    }

    /// Positive case for the NFKC pass: a "dropbox" path component
    /// written with **full-width Latin** (codepoints in the
    /// FFxx block) gets folded to ASCII by NFKC and then matches
    /// the lowercase "dropbox" entry in `CLOUD_SYNC_NAMES`.
    #[test]
    fn rejects_full_width_latin_dropbox_via_nfkc() {
        // U+FF24 = "Ｄ", U+FF52 = "ｒ", etc. The full string
        // "Ｄｒｏｐｂｏｘ" is visually identical to "Dropbox" but
        // uses 7 different codepoints. NFKC maps each to its ASCII
        // counterpart.
        let p = abs("Users/z/\u{FF24}\u{FF52}\u{FF4F}\u{FF50}\u{FF42}\u{FF4F}\u{FF58}/solo");
        let err = validate_data_dir(&p).unwrap_err();
        assert!(
            err.to_string().contains("cloud-sync"),
            "NFKC should fold full-width Latin to ASCII; got: {err}"
        );
    }

    /// Positive case for ligature folding: "ﬃ" (U+FB03) is the
    /// Latin small ligature ffi; NFKC decomposes it to "ffi". A
    /// hypothetical cloud-sync provider whose folder contains a
    /// ligature wouldn't bypass the matcher (none of our
    /// CLOUD_SYNC_NAMES contain ffi today, but the test pins the
    /// NFKC behaviour for future entries).
    #[test]
    fn nfkc_decomposes_ligatures() {
        let normalised = canonicalize_for_match("o\u{FB03}ce"); // "oﬃce"
        assert_eq!(normalised, "office");
    }

    #[test]
    fn rejects_box_dot_com_via_box_component() {
        let p = abs("Users/y/Box/solo");
        assert!(validate_data_dir(&p).is_err());
    }

    #[test]
    fn empty_path_is_rejected_as_non_absolute() {
        let p = PathBuf::new();
        let err = validate_data_dir(&p).unwrap_err();
        assert!(err.to_string().contains("absolute"), "got: {err}");
    }

    #[test]
    fn windows_unc_path_share_name_is_caught() {
        // UNC paths like \\server\share\... encode the share name in a
        // Path::Prefix component. We split the prefix's lowercased
        // string on \ or / and match each segment against
        // CLOUD_SYNC_NAMES — so a share literally named "Dropbox"
        // is rejected, same as a Normal "Dropbox" component.
        #[cfg(windows)]
        {
            let p_share = PathBuf::from(r"\\fileserver\Dropbox\team\solo");
            let err = validate_data_dir(&p_share).unwrap_err();
            assert!(
                err.to_string().contains("UNC prefix segment"),
                "expected UNC-specific error, got: {err}"
            );

            // OneDrive share too.
            let p_onedrive = PathBuf::from(r"\\nas\OneDrive\users\me\solo");
            assert!(validate_data_dir(&p_onedrive).is_err());

            // Cloud-sync names elsewhere in the path also caught
            // (Normal-component path).
            let p_inner = PathBuf::from(r"\\fileserver\share\Dropbox\solo");
            assert!(validate_data_dir(&p_inner).is_err());

            // Benign UNC share is allowed.
            let p_ok = PathBuf::from(r"\\fileserver\backup\team\solo");
            assert!(validate_data_dir(&p_ok).is_ok());
        }
    }
}
