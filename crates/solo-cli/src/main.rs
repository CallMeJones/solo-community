// SPDX-License-Identifier: Apache-2.0

//! `solo` binary entry point. Delegates to `solo_cli::run`.
//!
//! Process-level setup happens here (panic hook + tracing) before the
//! tokio runtime is built so panics that fire inside the runtime still
//! exit cleanly. Per ADR-0003 §O8 ("Panic recovery: log + exit + OS
//! supervisor restart. Never in-process."), the daemon is designed to
//! fail fast and let the OS supervisor (systemd / launchd / shell wrapper)
//! restart it.

use anyhow::Result;

fn main() -> Result<()> {
    install_panic_hook();
    install_tracing();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    runtime.block_on(solo_cli::run())
}

fn install_tracing() {
    // Route tracing output to stderr, NOT stdout. The default
    // `tracing_subscriber::fmt()` writes to stdout, which corrupts:
    //   - `solo mcp-stdio`: stdout is the JSON-RPC channel; tracing
    //     INFO lines interleaved with responses break MCP clients
    //     (Claude Desktop, Cursor) — they expect newline-delimited
    //     JSON only.
    //   - one-shots like `solo remember`/`solo recall`: tracing would
    //     mix with the user-facing result lines, breaking shell-pipe
    //     consumers.
    // Stderr is the right channel: terminals still display it, ops
    // tooling captures it separately, and stdout stays clean for
    // protocol/data output. Caught by the `mcp_smoke` test harness.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("solo=info")),
        )
        .init();
}

/// Per ADR-0003 §O8: log the panic with full context, then `process::exit(1)`
/// so the supervisor can restart us cleanly. In-process recovery from a
/// panicked writer thread is unsafe (deadpool / rusqlite borrow state) —
/// fail fast.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Run the default hook first so we get the standard formatting and
        // any RUST_BACKTRACE output, then exit. Tracing may not be
        // installed yet (we install_tracing in main, after this), so we
        // also write to stderr directly.
        eprintln!("FATAL: solo panicked, exiting (supervisor should restart)");
        eprintln!("  {info}");
        default_hook(info);
        std::process::exit(1);
    }));
}
