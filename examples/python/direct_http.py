import json
import os
import sys
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


BASE_URL = os.getenv("SOLO_URL", "http://127.0.0.1:17821").rstrip("/")
BEARER_TOKEN = os.getenv("SOLO_BEARER_TOKEN")


def request(path, method="GET", body=None):
    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = Request(f"{BASE_URL}{path}", data=data, method=method)
    req.add_header("Accept", "application/json")
    if body is not None:
        req.add_header("Content-Type", "application/json")
    if BEARER_TOKEN:
        req.add_header("Authorization", f"Bearer {BEARER_TOKEN}")

    try:
        with urlopen(req, timeout=10) as response:
            text = response.read().decode("utf-8")
            return json.loads(text) if text else {}
    except HTTPError as exc:
        text = exc.read().decode("utf-8", errors="replace")
        try:
            payload = json.loads(text)
            detail = payload.get("error") or text
        except json.JSONDecodeError:
            detail = text
        raise RuntimeError(f"Solo HTTP {exc.code}: {detail}") from exc
    except URLError as exc:
        raise RuntimeError(
            f"Could not reach Solo daemon at {BASE_URL}. "
            "Start Solo Desktop/tray or run solo daemon."
        ) from exc


content = " ".join(sys.argv[1:]) or "Avery prefers planning notes with owners and dates."

status = request("/v1/status")
saved = request(
    "/memory",
    method="POST",
    body={
        "content": content,
        "source_type": "sdk_direct_http",
        "salience": 0.7,
    },
)
context = request(
    "/memory/context",
    method="POST",
    body={
        "query": "planning notes owners dates",
        "subject": "Avery",
        "limit": 3,
    },
)

print(
    json.dumps(
        {
            "library": status["library"]["name"],
            "memory_id": saved["memory_id"],
            "recall_count": context["sections"]["recall"]["count"],
            "facts_count": context["sections"]["facts"]["count"],
        },
        indent=2,
    )
)
