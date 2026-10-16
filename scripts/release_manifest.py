#!/usr/bin/env python3
"""Record exact release identity and SHA-256 for the artifacts built on a host."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if commit != os.environ["GITHUB_SHA"]:
        raise RuntimeError("Checkout differs from the release workflow commit")
    dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], text=True)
    if dirty.strip():
        raise RuntimeError("Tracked release inputs changed during the build: " + dirty)
    files = []
    for path in sorted(args.directory.iterdir()):
        if path.is_symlink() or not path.is_file():
            raise RuntimeError(f"Unexpected release output: {path}")
        if path.name.endswith(".sha256") or path.name.startswith("release-manifest-"):
            continue
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        path.with_name(path.name + ".sha256").write_text(f"{digest}  {path.name}\n", encoding="utf-8")
        files.append({"name": path.name, "bytes": path.stat().st_size, "sha256": digest})
    if not files:
        raise RuntimeError("No release artifacts")
    manifest = {"schema_version": 1, "version": args.version, "target": args.target,
                "commit": commit, "run_id": os.environ["GITHUB_RUN_ID"],
                "run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
                "run_url": f"https://github.com/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}",
                "signing": "unsigned preview; macOS app uses ad-hoc integrity signing",
                "artifacts": files}
    path = args.directory / f"release-manifest-{args.target}.json"
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    path.with_name(path.name + ".sha256").write_text(f"{digest}  {path.name}\n", encoding="utf-8")
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
