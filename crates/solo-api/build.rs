// SPDX-License-Identifier: Apache-2.0

fn main() {
    write_embedded_solo_web_assets();
}

fn write_embedded_solo_web_assets() {
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"),
    );
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let generated = out_dir.join("solo_web_assets.rs");

    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("assets").join("solo-web").display()
    );
    println!("cargo:rerun-if-env-changed=SOLO_WEB_DIST");

    let mut source = String::new();
    match find_solo_web_dist(&manifest_dir) {
        Some(dist) => {
            println!("cargo:rerun-if-changed={}", dist.display());
            let mut files = Vec::new();
            collect_dist_files(&dist, &dist, &mut files).expect("collect solo-web dist files");
            files.sort_by(|a, b| a.0.cmp(&b.0));
            writeln!(
                source,
                "pub const EMBEDDED_SOLO_WEB_REAL_DIST: bool = true;"
            )
            .unwrap();
            writeln!(
                source,
                "pub const EMBEDDED_SOLO_WEB_ASSETS: &[crate::desktop_assets::WebAsset] = &["
            )
            .unwrap();
            for (relative, absolute) in files {
                println!("cargo:rerun-if-changed={}", absolute.display());
                writeln!(
                    source,
                    "    crate::desktop_assets::WebAsset {{ path: {:?}, mime: {:?}, bytes: include_bytes!(r#\"{}\"#) }},",
                    relative,
                    mime_for_path(Path::new(&relative)),
                    absolute.display()
                )
                .unwrap();
            }
            writeln!(source, "];").unwrap();
        }
        None => {
            println!(
                "cargo:warning=embedded Solo Web assets not found; embedding fallback page for /desktop"
            );
            writeln!(
                source,
                "pub const EMBEDDED_SOLO_WEB_REAL_DIST: bool = false;"
            )
            .unwrap();
            writeln!(
                source,
                "pub const EMBEDDED_SOLO_WEB_ASSETS: &[crate::desktop_assets::WebAsset] = &["
            )
            .unwrap();
            writeln!(
                source,
                "    crate::desktop_assets::WebAsset {{ path: \"index.html\", mime: \"text/html; charset=utf-8\", bytes: br#\"{}\"# }},",
                fallback_html()
            )
            .unwrap();
            writeln!(source, "];").unwrap();
        }
    }

    std::fs::write(generated, source).expect("write generated solo-web assets");
}

fn find_solo_web_dist(manifest_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Ok(dist) = std::env::var("SOLO_WEB_DIST") {
        let dist = std::path::PathBuf::from(dist);
        if dist.join("index.html").is_file() {
            return Some(dist);
        }
        panic!(
            "SOLO_WEB_DIST={} does not contain index.html",
            dist.display()
        );
    }

    let local_assets = manifest_dir.join("assets").join("solo-web");
    if local_assets.join("index.html").is_file() {
        Some(local_assets)
    } else {
        None
    }
}

fn collect_dist_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(String, std::path::PathBuf)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_dist_files(root, &path, out)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("dist entry under root")
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        out.push((relative, path));
    }
    Ok(())
}

fn mime_for_path(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn fallback_html() -> &'static str {
    r#"<!doctype html><html><head><meta charset="utf-8"><title>Solo Desktop Missing</title></head><body><main style="font-family:system-ui;margin:2rem;max-width:42rem"><h1>Solo Desktop assets were not bundled</h1><p>Build the Web app in <code>apps/web</code>, run <code>scripts/sync_solo_web_assets.ps1</code>, then rebuild Solo.</p></main></body></html>"#
}
