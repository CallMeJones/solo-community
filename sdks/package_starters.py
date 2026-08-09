from __future__ import annotations

import argparse
import json
import tomllib
import zipfile
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FIXED_ZIP_TIME = (1980, 1, 1, 0, 0, 0)


@dataclass(frozen=True)
class Bundle:
    name: str
    root_name: str
    description: str
    files: tuple[tuple[str, str], ...]


POLICY_FILES = tuple(
    (str(path.relative_to(ROOT)), f"docs/policies/{path.name}")
    for path in sorted((ROOT / "docs" / "policies").glob("*.md"))
)

TYPESCRIPT_FILES = (
    ("sdks/typescript/README.md", "README.md"),
    ("sdks/typescript/package.json", "package.json"),
    ("sdks/typescript/solo-client.js", "solo-client.js"),
    ("sdks/typescript/solo-client.d.ts", "solo-client.d.ts"),
    ("sdks/typescript/smoke-test.mjs", "smoke-test.mjs"),
    ("examples/typescript/direct-http.mjs", "examples/direct-http.mjs"),
    ("examples/typescript/remember-recall.ts", "examples/remember-recall.ts"),
    ("examples/typescript/mcp-tools-list.ts", "examples/mcp-tools-list.ts"),
    ("examples/typescript/vercel-ai-sdk-memory.ts", "examples/vercel-ai-sdk-memory.ts"),
    ("docs/book/src/memory-policy-pack.md", "docs/memory-policy-pack.md"),
    *POLICY_FILES,
)

PYTHON_FILES = (
    ("sdks/python/README.md", "README.md"),
    ("sdks/python/solo_client.py", "solo_client.py"),
    ("sdks/python/smoke_test.py", "smoke_test.py"),
    ("examples/python/direct_http.py", "examples/direct_http.py"),
    ("examples/python/remember_recall.py", "examples/remember_recall.py"),
    ("examples/python/mcp_tools_list.py", "examples/mcp_tools_list.py"),
    ("examples/python/openai_agents_memory.py", "examples/openai_agents_memory.py"),
    ("examples/python/langgraph_memory.py", "examples/langgraph_memory.py"),
    ("docs/book/src/memory-policy-pack.md", "docs/memory-policy-pack.md"),
    *POLICY_FILES,
)

ALL_FILES = (
    ("sdks/README.md", "README.md"),
    ("sdks/DISTRIBUTION.md", "DISTRIBUTION.md"),
    ("docs/book/src/sdk-examples.md", "docs/sdk-examples.md"),
    *tuple((source, f"typescript/{dest}") for source, dest in TYPESCRIPT_FILES),
    *tuple((source, f"python/{dest}") for source, dest in PYTHON_FILES),
)

BUNDLES = {
    "typescript": Bundle(
        name="typescript",
        root_name="solo-typescript-starter",
        description="Dependency-free TypeScript/ESM Solo SDK starter.",
        files=TYPESCRIPT_FILES,
    ),
    "python": Bundle(
        name="python",
        root_name="solo-python-starter",
        description="Dependency-free Python standard-library Solo SDK starter.",
        files=PYTHON_FILES,
    ),
    "all": Bundle(
        name="all",
        root_name="solo-sdk-starters",
        description="Combined Solo SDK starter bundle for TypeScript and Python.",
        files=ALL_FILES,
    ),
}


def workspace_version() -> str:
    with (ROOT / "Cargo.toml").open("rb") as handle:
        cargo = tomllib.load(handle)
    return cargo["workspace"]["package"]["version"]


def manifest(bundle: Bundle, version: str) -> dict[str, object]:
    return {
        "name": bundle.root_name,
        "version": version,
        "description": bundle.description,
        "distribution": "repo-local starter bundle",
        "registry_publish": False,
        "registry_publish_reason": (
            "The HTTP/MCP starter surface is still pre-1.0; publish npm/PyPI "
            "packages only after the registry release bar in sdks/DISTRIBUTION.md is met."
        ),
        "files": [dest for _, dest in bundle.files],
    }


def check_source_metadata(version: str) -> None:
    package_json = json.loads((ROOT / "sdks" / "typescript" / "package.json").read_text())
    if package_json.get("version") != version:
        raise SystemExit(
            "sdks/typescript/package.json version must match workspace version "
            f"{version}"
        )
    if package_json.get("private") is not True:
        raise SystemExit("sdks/typescript/package.json must stay private before registry publish")


def add_text(zip_file: zipfile.ZipFile, archive_path: str, text: str) -> None:
    info = zipfile.ZipInfo(archive_path, FIXED_ZIP_TIME)
    info.compress_type = zipfile.ZIP_DEFLATED
    zip_file.writestr(info, text.encode("utf-8"))


def add_file(zip_file: zipfile.ZipFile, archive_path: str, source: Path) -> None:
    info = zipfile.ZipInfo(archive_path, FIXED_ZIP_TIME)
    info.compress_type = zipfile.ZIP_DEFLATED
    zip_file.writestr(info, source.read_bytes())


def build_bundle(bundle: Bundle, out_dir: Path, version: str) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    root = f"{bundle.root_name}-{version}"
    output = out_dir / f"{root}.zip"
    with zipfile.ZipFile(output, "w") as zip_file:
        add_text(
            zip_file,
            f"{root}/solo-starter-manifest.json",
            json.dumps(manifest(bundle, version), indent=2, sort_keys=True) + "\n",
        )
        for source, dest in bundle.files:
            add_file(zip_file, f"{root}/{dest}", ROOT / source)
    return output


def check_bundle(bundle: Bundle, version: str, path: Path) -> None:
    root = f"{bundle.root_name}-{version}"
    expected = {f"{root}/solo-starter-manifest.json"}
    expected.update(f"{root}/{dest}" for _, dest in bundle.files)
    with zipfile.ZipFile(path) as zip_file:
        names = set(zip_file.namelist())
        missing = sorted(expected - names)
        if missing:
            raise SystemExit(f"{path} is missing expected entries: {missing}")
        raw_manifest = json.loads(zip_file.read(f"{root}/solo-starter-manifest.json"))
    if raw_manifest["version"] != version or raw_manifest["registry_publish"] is not False:
        raise SystemExit(f"{path} has an invalid starter manifest")


def display_path(path: Path) -> str:
    try:
        return str(path.relative_to(ROOT))
    except ValueError:
        return str(path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build Solo SDK starter bundles.")
    parser.add_argument(
        "--out",
        type=Path,
        default=ROOT / ".smoke" / "sdk-starters",
        help="Output directory for starter zip files.",
    )
    parser.add_argument(
        "--bundle",
        action="append",
        choices=sorted(BUNDLES),
        help="Bundle to build. Repeatable. Defaults to all bundles.",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Open each zip and verify the expected manifest and file entries.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    version = workspace_version()
    check_source_metadata(version)
    out_dir = args.out if args.out.is_absolute() else ROOT / args.out
    selected = args.bundle or ["typescript", "python", "all"]
    for name in selected:
        bundle = BUNDLES[name]
        path = build_bundle(bundle, out_dir, version)
        if args.check:
            check_bundle(bundle, version, path)
        print(f"built {display_path(path)}")


if __name__ == "__main__":
    main()
