// SPDX-License-Identifier: Apache-2.0

//! `solo init` subcommand.
//!
//! UX:
//!
//! ```text
//! $ solo init
//! Initializing Solo data directory at /home/me/.solo
//! Enter passphrase (will not be echoed):
//! Confirm passphrase:
//! Deriving key (~500ms) ...
//! Your first name (press enter to skip):
//! Created solo.db (schema v1)
//! Created solo.config.toml
//! Done. Run `solo daemon` to start the memory daemon.
//! ```
//!
//! Passphrase resolution order:
//!   1. `SOLO_PASSPHRASE` env var (warns the user because process environments
//!      can be exposed to same-user processes or diagnostic tools).
//!   2. Interactive prompt via rpassword. Asks twice, requires match.
//!
//! `--data-dir` overrides the default of `~/.solo`. `--force` wipes the
//! existing data dir contents (Solo-owned files only) and re-initializes.
//!
//! After the storage layer writes a fresh `solo.config.toml`, the CLI asks
//! the user for their first name (blank = skip). A non-blank answer is
//! trimmed + lowercased and persisted as the first entry of
//! `IdentityConfig.user_aliases` so the v0.5.0 read-path alias resolution
//! (Priority 1 sub-step 1C — `facts_about` SQL expansion) has a seed alias
//! to match against historical `subject_id = "user"` triples without the
//! user having to hand-edit the TOML. See
//! `docs/dev-log/0071-v0.5.x-roadmap.md` Priority 1 sub-step 1D.

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_storage::{InitParams, SoloConfig, default_data_dir, probe_embedder_config_from_env};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

const ENV_PASSPHRASE: &str = "SOLO_PASSPHRASE";

/// Prompt shown to the user when asking for their first name during
/// interactive `solo init`. Centralised so the CLI banner doc-comment +
/// the runtime print + the unit-test assertion all stay in lockstep.
const NAME_PROMPT: &str = "Your first name (press enter to skip): ";

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Data directory to initialize. Defaults to `~/.solo`.
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Wipe Solo-owned files in `--data-dir` and re-initialize.
    /// DESTRUCTIVE: all stored memories will be lost.
    #[arg(long)]
    pub force: bool,
}

pub async fn run(args: InitArgs) -> Result<()> {
    let data_dir = match args.data_dir {
        Some(p) => p,
        None => default_data_dir().context(
            "could not resolve default data dir (no home directory found); \
             pass --data-dir explicitly",
        )?,
    };

    let passphrase = read_passphrase()?;

    println!("Initializing Solo data directory at {}", data_dir.display());

    // Resolve the embedder identity to persist BEFORE the heavy
    // SQLCipher work, so a fail-fast probe error (e.g. Ollama not
    // running) doesn't leave a half-created data dir behind. The
    // probe respects the same env-var precedence as the daemon's
    // runtime `build_embedder` path (v0.5.1 sub-step 6D).
    let embedder = probe_embedder_config_from_env()
        .await
        .context("resolve embedder identity for solo.config.toml")?;

    println!("Deriving key (~500ms with Argon2id) ...");

    let params = InitParams {
        data_dir: data_dir.clone(),
        passphrase,
        force: args.force,
        embedder,
    };

    let outcome = solo_storage::init(params).context("solo init failed")?;

    // Prompt for the user's first name and persist it as a `user_aliases`
    // entry on the freshly-written config. `solo_storage::init` always
    // writes a config with `IdentityConfig::default()` (empty aliases) for
    // a first-init flow, so the `apply` helper handles both the prompt
    // call and the atomic config rewrite. We pass a stdin-backed reader so
    // tests can swap in a canned closure.
    apply_user_alias_prompt(&outcome.config_path, read_first_name_from_stdin)
        .context("prompt for first name + persist alias")?;

    println!();
    if outcome.upgraded_from_v071 {
        println!("✓ Upgraded v0.7.1 layout in-place");
        println!(
            "  {}  (moved from <data_dir>/solo.db)",
            outcome.db_path.display()
        );
    } else {
        println!("✓ Created {}", outcome.db_path.display());
    }
    println!("✓ Wrote   {}", outcome.config_path.display());
    println!("  Schema  v{}", outcome.schema_version);
    println!();
    println!("Done. Run `solo daemon` to start the memory daemon.");

    Ok(())
}

/// Read a line from stdin and return it as the user's first-name input.
/// Production caller for `apply_user_alias_prompt`. Tests inject a canned
/// closure instead so they don't need to drive a real stdin.
///
/// On EOF (e.g. when stdin is `/dev/null` or a closed pipe) `read_line`
/// returns `Ok(0)` with an empty buffer — we surface that as the same
/// `"\n"` shape as a user pressing enter immediately, so the trim path
/// downstream skips the alias write.
fn read_first_name_from_stdin() -> io::Result<String> {
    // Flush stdout so the prompt is visible before we block on read_line.
    // Without this, line-buffered runtimes can hold the prompt back until
    // the user's newline echoes — confusing UX on Windows consoles.
    print!("{NAME_PROMPT}");
    io::stdout().flush()?;
    let mut buf = String::new();
    let stdin = io::stdin();
    stdin.lock().read_line(&mut buf)?;
    Ok(buf)
}

/// Read the config at `config_path`, optionally append a user alias from
/// the prompt closure, and rewrite the file atomically.
///
/// Skip rules:
/// - If the existing config already declares `identity.user_aliases`
///   (non-empty), log a brief notice and return without invoking the
///   closure — re-running `solo init --force` should not silently
///   overwrite a previously-configured name. (For first-init this branch
///   is unreachable; `solo_storage::init` writes the config with
///   `IdentityConfig::default()`, so the check is purely defensive and
///   forward-compatible if storage::init ever seeds aliases.)
/// - If the closure returns whitespace-only input (including pure `\n`),
///   leave `user_aliases` empty and return without rewriting the file.
///
/// On a non-blank name: trim → lowercase → store as the sole entry of
/// `user_aliases`. The atomic rewrite goes through `SoloConfig::write`,
/// which refuses to overwrite, so we delete first. Both the delete and
/// the rewrite happen while `solo.lock` is NOT held (the lockfile is
/// dropped at the end of `solo_storage::init`), but `solo init` is a
/// single-process command so a race is impossible in practice.
fn apply_user_alias_prompt(
    config_path: &Path,
    read_name: impl FnOnce() -> io::Result<String>,
) -> Result<()> {
    let mut cfg = SoloConfig::read(config_path)
        .map_err(|e| anyhow::anyhow!("read config back from {}: {e}", config_path.display()))?;

    if !cfg.identity.user_aliases.is_empty() {
        tracing::info!(
            existing_aliases = ?cfg.identity.user_aliases,
            "identity.user_aliases already configured; skipping first-name prompt"
        );
        return Ok(());
    }

    let raw = read_name().context("read first name from stdin")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // Blank or whitespace-only — leave `user_aliases` empty and bail
        // out before touching the file. The fresh config already has
        // `IdentityConfig::default()` from storage::init.
        return Ok(());
    }

    let alias = trimmed.to_lowercase();
    cfg.identity.user_aliases = vec![alias];

    // SoloConfig::write refuses to overwrite an existing file, so delete
    // first. `solo_storage::init` just wrote this file and the lockfile
    // is dropped — no concurrent reader to worry about.
    std::fs::remove_file(config_path).map_err(|e| {
        anyhow::anyhow!(
            "remove old config before alias rewrite at {}: {e}",
            config_path.display()
        )
    })?;
    cfg.write(config_path).map_err(|e| {
        anyhow::anyhow!(
            "rewrite config with user alias at {}: {e}",
            config_path.display()
        )
    })?;

    Ok(())
}

/// Resolve the passphrase. Env var first (with a security warning); otherwise
/// prompt twice and require both entries to match. Returns
/// `Zeroizing<String>` so the buffer wipes on drop.
fn read_passphrase() -> Result<zeroize::Zeroizing<String>> {
    use zeroize::Zeroizing;
    if let Ok(env_pass) = std::env::var(ENV_PASSPHRASE) {
        if env_pass.is_empty() {
            bail!("{ENV_PASSPHRASE} is set but empty");
        }
        eprintln!(
            "warning: reading passphrase from {ENV_PASSPHRASE} process environment; \
             it may be visible to same-user processes or diagnostic tools"
        );
        return Ok(Zeroizing::new(env_pass));
    }

    let p1 = rpassword::prompt_password("Enter passphrase (will not be echoed): ")
        .context("read passphrase")?;
    if p1.is_empty() {
        bail!("passphrase must not be empty");
    }
    let p2 = rpassword::prompt_password("Confirm passphrase: ").context("read confirmation")?;
    if p1 != p2 {
        bail!("passphrases did not match");
    }
    // Wipe the confirm buffer too.
    drop(Zeroizing::new(p2));
    Ok(Zeroizing::new(p1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Canonical fresh-init TOML body: matches the shape `SoloConfig::new`
    /// + `cfg.write` produces, with `[identity]` absent (the default-on-
    /// fresh-init case the prompt path is designed to fill). Used by every
    /// test that needs a config file on disk without spinning up the full
    /// `solo_storage::init` (which requires SQLCipher + Argon2id key
    /// derivation — too heavy for a unit test).
    fn write_fresh_config_no_identity(path: &Path) {
        let body = "schema_version = 1\n\
                    salt_hex = \"00000000000000000000000000000000\"\n\
                    \n\
                    [embedder]\n\
                    name = \"stub\"\n\
                    version = \"v1\"\n\
                    dim = 32\n\
                    dtype = \"f32\"\n";
        std::fs::write(path, body).unwrap();
    }

    /// Same canonical body but with an `[identity]` block that already
    /// declares a `user_aliases` entry. Exercises the skip-if-set branch.
    fn write_config_with_existing_alias(path: &Path, alias: &str) {
        let body = format!(
            "schema_version = 1\n\
             salt_hex = \"00000000000000000000000000000000\"\n\
             \n\
             [embedder]\n\
             name = \"stub\"\n\
             version = \"v1\"\n\
             dim = 32\n\
             dtype = \"f32\"\n\
             \n\
             [identity]\n\
             user_aliases = [\"{alias}\"]\n"
        );
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn non_blank_name_is_trimmed_lowered_and_persisted() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        write_fresh_config_no_identity(&path);

        apply_user_alias_prompt(&path, || Ok("  Alex\n".into())).expect("apply ok");

        let cfg = SoloConfig::read(&path).expect("read config");
        assert_eq!(cfg.identity.user_aliases, vec!["alex".to_string()]);
    }

    #[test]
    fn blank_input_leaves_user_aliases_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        write_fresh_config_no_identity(&path);

        apply_user_alias_prompt(&path, || Ok("\n".into())).expect("apply ok");

        let cfg = SoloConfig::read(&path).expect("read config");
        assert!(
            cfg.identity.user_aliases.is_empty(),
            "expected empty aliases, got {:?}",
            cfg.identity.user_aliases
        );
    }

    #[test]
    fn whitespace_only_input_leaves_user_aliases_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        write_fresh_config_no_identity(&path);

        apply_user_alias_prompt(&path, || Ok("   \t  \n".into())).expect("apply ok");

        let cfg = SoloConfig::read(&path).expect("read config");
        assert!(
            cfg.identity.user_aliases.is_empty(),
            "expected empty aliases, got {:?}",
            cfg.identity.user_aliases
        );
    }

    #[test]
    fn skip_when_user_aliases_already_set() {
        // If `solo init --force` (or a future code path) ever produces a
        // config with `user_aliases` already populated, the prompt path
        // must NOT clobber it. We assert two things at once:
        //   1. The pre-existing alias survives.
        //   2. The prompt closure is never invoked — exercised by a
        //      closure that panics if called.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        write_config_with_existing_alias(&path, "preexisting");

        apply_user_alias_prompt(&path, || {
            panic!("prompt closure must not be called when alias already set");
        })
        .expect("apply ok");

        let cfg = SoloConfig::read(&path).expect("read config");
        assert_eq!(
            cfg.identity.user_aliases,
            vec!["preexisting".to_string()],
            "pre-existing alias must survive"
        );
    }

    #[test]
    fn name_with_trailing_crlf_is_handled() {
        // Windows stdin delivers `\r\n`; trim must strip both.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        write_fresh_config_no_identity(&path);

        apply_user_alias_prompt(&path, || Ok("Maya\r\n".into())).expect("apply ok");

        let cfg = SoloConfig::read(&path).expect("read config");
        assert_eq!(cfg.identity.user_aliases, vec!["maya".to_string()]);
    }

    #[test]
    fn unicode_name_is_lowercased_via_unicode_rules() {
        // Solo is a personal memory tool — users put accents and non-ASCII
        // characters in their names. `String::to_lowercase` follows Unicode
        // case-folding, so `Á` → `á`. We don't validate or strip anything
        // beyond non-empty-after-trim.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        write_fresh_config_no_identity(&path);

        apply_user_alias_prompt(&path, || Ok("Álvaro\n".into())).expect("apply ok");

        let cfg = SoloConfig::read(&path).expect("read config");
        assert_eq!(cfg.identity.user_aliases, vec!["álvaro".to_string()]);
    }
}
