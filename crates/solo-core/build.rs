// SPDX-License-Identifier: Apache-2.0

#[path = "../build-support/build_metadata.rs"]
mod build_metadata;

fn main() {
    let repo_root = build_metadata::repo_root_from_manifest_dir();
    build_metadata::emit_rerun_instructions(&repo_root);
    let metadata = build_metadata::collect(&repo_root);

    if let Some(sha) = metadata.git_sha {
        println!("cargo:rustc-env=SOLO_BUILD_GIT_SHA={sha}");
    }
    println!(
        "cargo:rustc-env=SOLO_BUILD_GIT_DIRTY={}",
        metadata.git_dirty
    );
    if let Some(run_number) = metadata.build_number {
        println!("cargo:rustc-env=SOLO_BUILD_NUMBER={run_number}");
    }
    if let Some(run_attempt) = metadata.build_attempt {
        println!("cargo:rustc-env=SOLO_BUILD_ATTEMPT={run_attempt}");
    }
    if let Some(ref_name) = metadata.build_ref {
        println!("cargo:rustc-env=SOLO_BUILD_REF={ref_name}");
    }
    println!(
        "cargo:rustc-env=SOLO_BUILD_TIMESTAMP={}",
        metadata.build_timestamp
    );
}
