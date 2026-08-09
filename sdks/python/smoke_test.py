import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from solo_client import SoloClient


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self._handle()

    def do_POST(self):
        self._handle()

    def do_PATCH(self):
        self._handle()

    def do_DELETE(self):
        self._handle()

    def _handle(self):
        body = self._read_json()
        parsed = urlparse(self.path)
        self.server.seen.append({
            "method": self.command,
            "path": self.path,
            "headers": dict(self.headers),
            "body": body,
        })

        if parsed.path == "/v1/status" and self.command == "GET":
            return self._send_json({
                "ok": True,
                "version": "0.0.0",
                "build": {},
                "library": {"name": "Community Memory Library", "ready": True},
                "embedder": {"name": "mock", "version": "0", "dim": 384, "dtype": "f32"},
                "mcp": {"sessions": 0},
                "steward": {},
                "runtime": {"pid": 1, "platform": "test", "data_dir": "/tmp/solo"},
            })
        if parsed.path == "/memory":
            return self._send_json({"memory_id": "mem_1"})
        if parsed.path == "/v1/inbox" and self.command == "GET":
            assert parse_qs(parsed.query) == {"limit": ["10"]}
            return self._send_json({
                "items": [{
                    "memory_id": "mem_1",
                    "label": "Remember this",
                    "preview": "Remember this",
                    "ts_ms": 10,
                    "source_type": "solo_desktop.inbox",
                    "salience": 0.6,
                    "status": "active",
                    "review_state": None,
                    "reviewed_at_ms": None,
                    "review_note": None,
                }],
            })
        if parsed.path == "/v1/inbox/mem_1/review" and self.command == "POST":
            return self._send_json({
                "memory_id": "mem_1",
                "state": body.get("state"),
                "reviewed_at_ms": 11,
            })
        if parsed.path == "/memory/search":
            return self._send_json({
                "hits": [{
                    "rowid": 1,
                    "memory_id": "mem_1",
                    "cos_distance": 0.0,
                    "content": body["query"],
                    "source_type": "sdk_smoke",
                    "tier": "hot",
                }],
                "index_len": 1,
                "candidates_considered": 1,
            })
        if parsed.path == "/memory/context":
            return self._send_json({
                "query": body["query"],
                "subject": body.get("subject"),
                "resolved_subject": body.get("subject"),
                "sections": {
                    "recall": {"status": "ok", "count": 0, "warning": None},
                    "themes": {"status": "ok", "count": 0, "warning": None},
                    "entities": {"status": "ok", "count": 0, "warning": None},
                    "facts": {"status": "ok", "count": 0, "warning": None},
                    "contradictions": {"status": "ok", "count": 0, "warning": None},
                },
                "recall": {"hits": [], "index_len": 0, "candidates_considered": 0},
                "themes": [],
                "entities": [],
                "facts": [],
                "contradictions": [],
            })
        if parsed.path == "/memory/facts_about":
            assert parse_qs(parsed.query) == {
                "subject": ["Maya"],
                "include_as_object": ["true"],
                "limit": ["2"],
            }
            return self._send_json([{
                "triple_id": "triple_1",
                "subject_id": "Maya",
                "predicate": "prefers",
                "object_id": "concise notes",
                "object_kind": "text",
                "valid_from_ms": 1,
                "valid_to_ms": None,
                "confidence": 0.9,
                "cluster_id": None,
            }])
        if parsed.path == "/memory/entities":
            assert parse_qs(parsed.query) == {"query": ["May"], "limit": ["3"]}
            return self._send_json([{
                "entity_id": "Maya",
                "subject_count": 1,
                "object_count": 0,
                "fact_count": 1,
                "predicates": ["prefers"],
                "match_score": 3,
            }])
        if parsed.path == "/memory/mem_1" and self.command == "GET":
            return self._send_json({"memory_id": "mem_1", "content": "Remember this", "status": "active"})
        if parsed.path == "/memory/mem_1" and self.command == "PATCH":
            return self._send_json({"memory_id": "mem_1", "content": body["content"], "updated": True})
        if parsed.path == "/memory/mem_1" and self.command == "DELETE":
            assert parse_qs(parsed.query) == {"reason": ["stale test"]}
            return self._send_json({"memory_id": "mem_1", "status": "forgotten"})
        if parsed.path == "/memory/documents" and self.command == "GET":
            assert parse_qs(parsed.query) == {
                "limit": ["2"],
                "offset": ["1"],
                "include_forgotten": ["true"],
            }
            return self._send_json([{
                "doc_id": "doc_1",
                "title": "Solo notes",
                "source": "solo.md",
                "mime_type": "text/markdown",
                "ingested_at_ms": 10,
                "chunk_count": 2,
                "status": "active",
            }])
        if parsed.path == "/memory/documents" and self.command == "POST":
            return self._send_json({
                "doc_id": "doc_1",
                "chunks_persisted": 2,
                "bytes_ingested": 123,
                "deduped": False,
            })
        if parsed.path == "/memory/documents/search" and self.command == "POST":
            return self._send_json([{
                "chunk_id": "chunk_1",
                "doc_id": "doc_1",
                "doc_title": "Solo notes",
                "doc_source": "solo.md",
                "doc_mime_type": "text/markdown",
                "chunk_index": 0,
                "content": body["query"],
                "cos_distance": 0.1,
                "start_offset": 0,
                "end_offset": 12,
            }])
        if parsed.path == "/memory/documents/doc_1" and self.command == "GET":
            return self._send_json({
                "document": {
                    "doc_id": "doc_1",
                    "title": "Solo notes",
                    "source": "solo.md",
                    "mime_type": "text/markdown",
                    "ingested_at_ms": 10,
                    "modified_at_ms": None,
                    "status": "active",
                    "chunk_count": 2,
                    "content_hash": "hash",
                    "byte_size": 123,
                },
                "chunks": [{
                    "chunk_id": "chunk_1",
                    "chunk_index": 0,
                    "content_preview": "hello",
                    "token_count": 1,
                }],
            })
        if parsed.path == "/memory/documents/doc_1" and self.command == "DELETE":
            return self._send_json({"doc_id": "doc_1", "chunks_tombstoned": 2})
        if parsed.path == "/v1/graph/nodes" and self.command == "GET":
            assert parse_qs(parsed.query) == {"kind": ["episode"], "limit": ["5"]}
            return self._send_json({
                "nodes": [{
                    "id": "episode:mem_1",
                    "kind": "episode",
                    "label": "Remember this",
                    "ts_ms": 10,
                    "preview": "Remember this",
                    "source_type": "sdk_smoke",
                    "salience": 0.8,
                    "status": "active",
                }],
                "next_cursor": None,
            })
        if parsed.path == "/mcp" and body.get("method") == "initialize":
            return self._send_json(
                {"jsonrpc": "2.0", "id": body["id"], "result": {"serverInfo": {"name": "solo"}}},
                {"Mcp-Session-Id": "session_1"},
            )
        if parsed.path == "/mcp" and body.get("method") == "notifications/initialized":
            assert self.headers.get("Mcp-Session-Id") == "session_1"
            return self._send_json({}, status=202)
        if parsed.path == "/mcp" and body.get("method") == "tools/list":
            assert self.headers.get("Mcp-Session-Id") == "session_1"
            return self._send_json({
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": {"tools": [{"name": "memory_remember"}]},
            })
        if parsed.path == "/mcp" and body.get("method") == "tools/call":
            assert self.headers.get("Mcp-Session-Id") == "session_1"
            return self._send_json({
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": {
                    "content": [{
                        "type": "text",
                        "text": json.dumps({"ok": True, "query": body["params"]["arguments"]["query"]}),
                    }],
                },
            })
        self._send_json({"error": "not found"}, status=404)

    def log_message(self, _format, *args):
        return

    def _read_json(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length).decode("utf-8")
        return json.loads(raw) if raw else {}

    def _send_json(self, body, headers=None, status=200):
        payload = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(payload)


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.seen = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        client = SoloClient(
            f"http://127.0.0.1:{server.server_port}",
            bearer_token="test-token",
        )
        assert client.status()["library"]["name"] == "Community Memory Library"
        assert client.remember(
            "Remember this",
            source_type="sdk_smoke",
            source_id="py",
            salience=0.8,
        ) == {"memory_id": "mem_1"}
        assert client.remember_inbox("Review this later", salience=0.6) == {"memory_id": "mem_1"}
        assert client.memory_inbox(limit=10)["items"][0]["memory_id"] == "mem_1"
        assert client.review_memory("mem_1", state="approved", note="looks right") == {
            "memory_id": "mem_1",
            "state": "approved",
            "reviewed_at_ms": 11,
        }
        assert client.recall("Remember this", limit=2)["hits"][0]["memory_id"] == "mem_1"
        context = client.context("Remember this", subject="Solo", limit=1)
        assert context["query"] == "Remember this"
        assert context["sections"]["recall"]["status"] == "ok"
        assert client.facts_about("Maya", include_as_object=True, limit=2)[0]["predicate"] == "prefers"
        assert client.entities("May", limit=3)[0]["entity_id"] == "Maya"
        assert client.inspect("mem_1")["content"] == "Remember this"
        assert client.update("mem_1", "Updated memory")["content"] == "Updated memory"
        assert client.forget("mem_1", reason="stale test")["status"] == "forgotten"
        assert client.list_documents(limit=2, offset=1, include_forgotten=True)[0]["doc_id"] == "doc_1"
        assert client.ingest_document(r"C:\notes\solo.md")["chunks_persisted"] == 2
        assert client.search_documents("Solo notes", limit=4)[0]["chunk_id"] == "chunk_1"
        assert client.inspect_document("doc_1")["chunks"][0]["content_preview"] == "hello"
        assert client.forget_document("doc_1")["chunks_tombstoned"] == 2
        assert client.recent_memories(limit=5)["nodes"][0]["id"] == "episode:mem_1"
        session = client.mcp_connect("smoke", "0")
        assert session["session_id"] == "session_1"
        assert client.mcp_list_tools(session)[0]["name"] == "memory_remember"
        tool_result = client.mcp_call_tool(session, "memory_context", {"query": "Remember this"})
        assert json.loads(tool_result["content"][0]["text"])["query"] == "Remember this"
        for request in server.seen:
            assert request["headers"]["Authorization"] == "Bearer test-token"
            assert "X-Solo-Tenant" not in request["headers"]
        assert find_request(
            server.seen,
            "POST",
            "/memory",
            lambda body: body.get("content") == "Remember this",
        )["body"] == {
            "content": "Remember this",
            "source_type": "sdk_smoke",
            "source_id": "py",
            "salience": 0.8,
        }
        assert find_request(
            server.seen,
            "POST",
            "/memory",
            lambda body: body.get("content") == "Review this later",
        )["body"] == {
            "content": "Review this later",
            "source_type": "solo_desktop.inbox",
            "salience": 0.6,
        }
        assert find_request(server.seen, "POST", "/v1/inbox/mem_1/review")["body"] == {
            "state": "approved",
            "note": "looks right",
        }
        assert find_request(server.seen, "POST", "/memory/search")["body"] == {"query": "Remember this", "limit": 2}
        assert find_request(server.seen, "POST", "/memory/context")["body"] == {
            "query": "Remember this",
            "subject": "Solo",
            "limit": 1,
        }
        assert find_request(server.seen, "POST", "/memory/documents")["body"] == {
            "path": r"C:\notes\solo.md",
        }
        assert find_request(server.seen, "POST", "/memory/documents/search")["body"] == {
            "query": "Solo notes",
            "limit": 4,
        }
        assert next(
            request for request in server.seen
            if request["body"].get("method") == "notifications/initialized"
        )["body"] == {
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        }
        try:
            client.mcp_list_tools({"session_id": None})
        except RuntimeError:
            pass
        else:
            raise AssertionError("missing MCP session id should fail")
    finally:
        server.shutdown()
    print("python sdk smoke ok")


def find_request(seen, method, path, body_predicate=lambda _body: True):
    for request in seen:
        if (
            request["method"] == method
            and urlparse(request["path"]).path == path
            and body_predicate(request["body"])
        ):
            return request
    raise AssertionError(f"missing {method} {path}")


if __name__ == "__main__":
    main()
