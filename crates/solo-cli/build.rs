// SPDX-License-Identifier: Apache-2.0

#[cfg(windows)]
#[path = "../build-support/build_metadata.rs"]
mod build_metadata;

fn main() {
    #[cfg(windows)]
    {
        let repo_root = build_metadata::repo_root_from_manifest_dir();
        build_metadata::emit_rerun_instructions(&repo_root);
        let metadata = build_metadata::collect(&repo_root);
        let version_with_build = build_metadata::version_with_build_metadata(&metadata);
        let build_comment =
            build_metadata::windows_resource_comment("Solo", &version_with_build, &metadata);

        let mut resource = winres::WindowsResource::new();
        resource
            .set_icon("assets/solo.ico")
            .set("FileDescription", "Solo")
            .set("FileVersion", &version_with_build)
            .set("ProductName", "Solo")
            .set("ProductVersion", &version_with_build)
            .set("OriginalFilename", "solo.exe")
            .set("Comments", &build_comment)
            .compile()
            .expect("compile solo.exe Windows icon resource");
    }
}
