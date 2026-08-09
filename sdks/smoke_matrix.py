from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def run(label: str, args: list[str]) -> None:
    print(f"==> {label}", flush=True)
    subprocess.run(args, cwd=ROOT, check=True)


def require_tool(name: str) -> None:
    if not shutil.which(name):
        raise SystemExit(f"Missing required tool on PATH: {name}")


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def assert_contains(path: str, *needles: str) -> None:
    text = read(path)
    missing = [needle for needle in needles if needle not in text]
    if missing:
        raise SystemExit(f"{path} is missing expected text: {missing}")


def assert_not_contains(path: str, *needles: str) -> None:
    text = read(path)
    found = [needle for needle in needles if needle in text]
    if found:
        raise SystemExit(f"{path} contains disallowed text: {found}")


def static_framework_checks() -> None:
    shared_env = ("SOLO_URL", "SOLO_BEARER_TOKEN")
    for path in (
        "examples/typescript/direct-http.mjs",
        "examples/typescript/vercel-ai-sdk-memory.ts",
        "examples/python/direct_http.py",
        "examples/python/openai_agents_memory.py",
        "examples/python/langgraph_memory.py",
    ):
        assert_contains(path, *shared_env)
        assert_not_contains(path, "SOLO_PROFILE", "X-Solo-Tenant")

    assert_contains(
        "examples/typescript/vercel-ai-sdk-memory.ts",
        "generateText",
        "tool",
        "inputSchema",
        "memoryContext",
        "rememberDurableFact",
        "sourceType: \"vercel_ai_sdk\"",
    )
    assert_not_contains("examples/typescript/vercel-ai-sdk-memory.ts", "parameters:")

    assert_contains(
        "examples/python/openai_agents_memory.py",
        "Agent",
        "Runner.run",
        "function_tool",
        "memory_context",
        "remember_durable_fact",
        "source_type=\"openai_agents_sdk\"",
    )
    assert_contains(
        "examples/python/langgraph_memory.py",
        "StateGraph",
        "START",
        "END",
        "retrieve_solo_memory",
        "graph.invoke",
    )


def main() -> None:
    require_tool("node")

    run("typescript sdk contract smoke", ["node", "--test", "sdks/typescript/smoke-test.mjs"])
    run("typescript runtime syntax", ["node", "--check", "sdks/typescript/solo-client.js"])
    run("typescript direct HTTP syntax", ["node", "--check", "examples/typescript/direct-http.mjs"])
    run("python sdk contract smoke", [sys.executable, "-B", "sdks/python/smoke_test.py"])
    run(
        "python example syntax",
        [
            sys.executable,
            "-m",
            "py_compile",
            "sdks/python/solo_client.py",
            "examples/python/direct_http.py",
            "examples/python/openai_agents_memory.py",
            "examples/python/langgraph_memory.py",
        ],
    )
    run(
        "sdk starter bundle smoke",
        [
            sys.executable,
            "-B",
            "sdks/package_starters.py",
            "--out",
            ".smoke/sdk-starters",
            "--check",
        ],
    )
    static_framework_checks()
    print("sdk smoke matrix ok")


if __name__ == "__main__":
    main()
