// SPDX-License-Identifier: Apache-2.0

//! Smoke tests for the command-line affordances users hit first.

use std::process::Command;

fn solo_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_solo"))
}

#[test]
fn bare_solo_prints_terminal_first_steps() {
    let out = solo_bin().output().expect("run bare solo");

    assert!(out.status.success(), "bare solo failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Solo is a command-line app"),
        "bare solo should explain terminal usage; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(".\\solo.exe init"),
        "bare solo should show Windows first-run commands; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("solo daemon"),
        "bare solo should still list commands; stdout:\n{stdout}"
    );
}

#[test]
fn help_prints_full_clap_usage() {
    let out = solo_bin().arg("--help").output().expect("run solo --help");

    assert!(out.status.success(), "solo --help failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Usage:"),
        "help should include clap usage; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Commands:"),
        "help should include commands; stdout:\n{stdout}"
    );
}
