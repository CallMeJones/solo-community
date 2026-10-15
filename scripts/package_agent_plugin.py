#!/usr/bin/env python3
"""Build the self-contained Solo agent-plugin marketplace ZIP (stdlib only)."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shlex
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PLUGIN_FILES = (
    ".codex-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".mcp.json",
    "hooks/hooks.json",
    "hooks/codex.json",
    "skills/memory/SKILL.md",
    "skills/setup/SKILL.md",
)
CATALOGS = (".agents/plugins/marketplace.json", ".claude-plugin/marketplace.json")


def read_file(relative: str) -> bytes:
    path = ROOT / relative
    # Package only the explicit source files, never caches or local credentials.
    if path.is_symlink() or not path.resolve().is_relative_to(ROOT) or not path.is_file():
        raise ValueError(f"Missing or unsafe package source: {relative}")
    return path.read_text(encoding="utf-8").encode("utf-8")


def read_json(relative: str) -> dict:
    result = json.loads(read_file(relative))
    if not isinstance(result, dict):
        raise ValueError(f"Expected JSON object: {relative}")
    return result


def validate() -> str:
    base = "plugins/solo/"
    codex = read_json(base + ".codex-plugin/plugin.json")
    claude = read_json(base + ".claude-plugin/plugin.json")
    version = codex["version"]
    if not isinstance(version, str) or not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+][\w.-]+)?", version):
        raise ValueError("Plugin version must be a safe semantic version")
    for manifest in (codex, claude):
        if manifest.get("name") != "solo" or manifest.get("version") != version:
            raise ValueError("Client manifests must have the same plugin identity and version")
    if codex["hooks"] != "./hooks/codex.json":
        raise ValueError("Codex must load only its own hook adapter")
    if "hooks" in claude:
        raise ValueError("Claude must auto-discover hooks/hooks.json without double loading it")
    if codex["mcpServers"] != "./.mcp.json" or codex["skills"] != "./skills/":
        raise ValueError("Compatibility manifest must include MCP and skills")
    if codex["extensions"]["com.openai"]["onboardingSkill"] != "./skills/setup/SKILL.md":
        raise ValueError("Onboarding must point at the packaged setup skill")

    mcp = read_json(base + ".mcp.json")["mcpServers"]["solo"]
    url = mcp["url"]
    if url != "http://127.0.0.1:17821/mcp" or mcp["type"] != "http":
        raise ValueError("MCP must use the documented local HTTP endpoint")

    for filename, client in (("hooks/hooks.json", "claude-code"), ("hooks/codex.json", "codex")):
        hooks = read_json(base + filename)["hooks"]
        if set(hooks) != {"SessionStart", "UserPromptSubmit"}:
            raise ValueError(f"Unexpected recall events in {filename}")
        for event, runner_event in (("SessionStart", "session-start"), ("UserPromptSubmit", "prompt")):
            groups = hooks[event]
            if len(groups) != 1 or len(groups[0]["hooks"]) != 1:
                raise ValueError(f"Recall must run exactly once per event: {filename}")
            handler = groups[0]["hooks"][0]
            args = shlex.split(handler["command"])
            expected = ["solo", "agent-hook", "--managed-by", "solo-plugin-v1",
                        "--client", client, "--event", runner_event, "--url", url]
            if args != expected or handler["type"] != "command" or handler["timeout"] != 10:
                raise ValueError(f"Invalid recall command in {filename}: {event}")

    for relative in PLUGIN_FILES:
        read_file(base + relative)
    for relative in CATALOGS:
        catalog = read_json(relative)
        if catalog["name"] != "solo-community" or len(catalog["plugins"]) != 1:
            raise ValueError(f"Invalid Solo marketplace identity: {relative}")
        entry = catalog["plugins"][0]
        source = entry["source"]
        path = source.get("path") if isinstance(source, dict) else source
        if entry["name"] != "solo" or path != "./plugins/solo":
            raise ValueError(f"Marketplace must point at the packaged plugin: {relative}")
    return version


def package(output_dir: Path, version: str) -> Path:
    output_dir.mkdir(parents=True, exist_ok=True)
    stem = f"solo-agent-plugin-{version}"
    output = output_dir / (stem + ".zip")
    sources = {relative: relative for relative in CATALOGS}
    sources.update({f"plugins/solo/{relative}": f"plugins/solo/{relative}" for relative in PLUGIN_FILES})
    sources.update({"LICENSE": "LICENSE", "README.md": "docs/agent-plugins.md"})
    # Fixed timestamps and normalized newlines give identical Windows/Linux ZIPs.
    with tempfile.NamedTemporaryFile(dir=output_dir, suffix=".zip", delete=False) as temp:
        temporary = Path(temp.name)
    try:
        with zipfile.ZipFile(temporary, "w", compression=zipfile.ZIP_STORED) as archive:
            for name, source in sorted(sources.items()):
                info = zipfile.ZipInfo(f"{stem}/{name}", date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_STORED
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                archive.writestr(info, read_file(source))
        with zipfile.ZipFile(temporary) as archive:
            if archive.testzip() is not None or len(archive.namelist()) != len(sources):
                raise ValueError("Plugin archive integrity check failed")
        temporary.replace(output)
    finally:
        temporary.unlink(missing_ok=True)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    output.with_suffix(".zip.sha256").write_text(
        f"{digest}  {output.name}\n", encoding="utf-8", newline="\n"
    )
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="Validate sources without producing a ZIP")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "target/agent-plugins")
    args = parser.parse_args()
    version = validate()
    print(f"Solo plugin {version}: manifests, MCP, hooks, skills, and catalogs verified")
    if not args.check:
        print(package(args.output_dir.resolve(), version))


if __name__ == "__main__":
    main()
