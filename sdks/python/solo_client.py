import json
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode
from urllib.request import Request, urlopen


class SoloHttpError(RuntimeError):
    def __init__(self, message, status, body):
        super().__init__(message)
        self.status = status
        self.body = body


class SoloMcpError(RuntimeError):
    def __init__(self, message, error):
        super().__init__(message)
        self.error = error


class SoloClient:
    def __init__(
        self,
        base_url="http://127.0.0.1:17821",
        bearer_token=None,
        timeout=10.0,
    ):
        self.base_url = base_url.rstrip("/")
        self.bearer_token = bearer_token
        self.timeout = timeout
        self._rpc_id = 0

    def status(self):
        return self._json("GET", "/v1/status")

    def remember(
        self,
        content,
        source_type=None,
        source_id=None,
        salience=None,
    ):
        body = {"content": content}
        if source_type is not None:
            body["source_type"] = source_type
        if source_id is not None:
            body["source_id"] = source_id
        if salience is not None:
            body["salience"] = salience
        return self._json("POST", "/memory", body)

    def remember_inbox(
        self,
        content,
        source_type="solo_desktop.inbox",
        source_id=None,
        salience=None,
    ):
        return self.remember(
            content,
            source_type=source_type,
            source_id=source_id,
            salience=salience,
        )

    def memory_inbox(self, limit=None):
        params = {}
        if limit is not None:
            params["limit"] = limit
        return self._json("GET", f"/v1/inbox{_query(params)}")

    def review_memory(self, memory_id, state=None, note=None):
        body = {}
        if state is not None:
            body["state"] = state
        if note is not None:
            body["note"] = note
        return self._json(
            "POST",
            f"/v1/inbox/{_quote(memory_id)}/review",
            body,
        )

    def recall(self, query, limit=None):
        body = {"query": query}
        if limit is not None:
            body["limit"] = limit
        return self._json("POST", "/memory/search", body)

    def context(self, query, subject=None, window_days=None, limit=None):
        body = {"query": query}
        if subject is not None:
            body["subject"] = subject
        if window_days is not None:
            body["window_days"] = window_days
        if limit is not None:
            body["limit"] = limit
        return self._json("POST", "/memory/context", body)

    def facts_about(
        self,
        subject,
        predicate=None,
        since_ms=None,
        until_ms=None,
        include_as_object=None,
        limit=None,
    ):
        params = {"subject": subject}
        if predicate is not None:
            params["predicate"] = predicate
        if since_ms is not None:
            params["since_ms"] = since_ms
        if until_ms is not None:
            params["until_ms"] = until_ms
        if include_as_object is not None:
            params["include_as_object"] = _bool_query(include_as_object)
        if limit is not None:
            params["limit"] = limit
        return self._json("GET", f"/memory/facts_about{_query(params)}")

    def entities(self, query, limit=None):
        params = {"query": query}
        if limit is not None:
            params["limit"] = limit
        return self._json("GET", f"/memory/entities{_query(params)}")

    def inspect(self, memory_id):
        return self._json("GET", f"/memory/{_quote(memory_id)}")

    def update(self, memory_id, content):
        return self._json(
            "PATCH",
            f"/memory/{_quote(memory_id)}",
            {"content": content},
        )

    def forget(self, memory_id, reason=None):
        suffix = f"?reason={_quote(reason)}" if reason else ""
        return self._json("DELETE", f"/memory/{_quote(memory_id)}{suffix}")

    def list_documents(self, limit=None, offset=None, include_forgotten=None):
        params = {}
        if limit is not None:
            params["limit"] = limit
        if offset is not None:
            params["offset"] = offset
        if include_forgotten is not None:
            params["include_forgotten"] = _bool_query(include_forgotten)
        return self._json("GET", f"/memory/documents{_query(params)}")

    def ingest_document(self, path):
        return self._json("POST", "/memory/documents", {"path": path})

    def search_documents(self, query, limit=None):
        body = {"query": query}
        if limit is not None:
            body["limit"] = limit
        return self._json("POST", "/memory/documents/search", body)

    def inspect_document(self, doc_id):
        return self._json("GET", f"/memory/documents/{_quote(doc_id)}")

    def forget_document(self, doc_id):
        return self._json("DELETE", f"/memory/documents/{_quote(doc_id)}")

    def recent_memories(
        self,
        limit=100,
        cursor=None,
        since_ms=None,
        until_ms=None,
    ):
        params = {"kind": "episode"}
        if limit is not None:
            params["limit"] = limit
        if cursor is not None:
            params["cursor"] = cursor
        if since_ms is not None:
            params["since_ms"] = since_ms
        if until_ms is not None:
            params["until_ms"] = until_ms
        return self._json("GET", f"/v1/graph/nodes{_query(params)}")

    def mcp_connect(self, client_name="solo-sdk", client_version="0.0.0"):
        session = self.mcp_initialize(client_name, client_version)
        if not session["session_id"]:
            raise RuntimeError("Solo MCP initialize did not return an Mcp-Session-Id header.")
        self.mcp_notify_initialized(session)
        return session

    def mcp_initialize(self, client_name="solo-sdk", client_version="0.0.0"):
        payload, headers = self._send("POST", "/mcp", {
            "jsonrpc": "2.0",
            "id": self._next_rpc_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": client_name, "version": client_version},
            },
        })
        result = self._json_rpc_result(payload)
        return {
            "session_id": headers.get("Mcp-Session-Id"),
            "result": result,
            "raw": payload,
        }

    def mcp_notify_initialized(self, session):
        session_id = self._session_id(session)
        self._send(
            "POST",
            "/mcp",
            {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
            {"Mcp-Session-Id": session_id},
        )

    def mcp_list_tools(self, session):
        session_id = self._session_id(session)
        payload = self._json(
            "POST",
            "/mcp",
            {"jsonrpc": "2.0", "id": self._next_rpc_id(), "method": "tools/list"},
            {"Mcp-Session-Id": session_id},
        )
        return self._json_rpc_result(payload).get("tools", [])

    def mcp_call_tool(self, session, name, arguments=None):
        session_id = self._session_id(session)
        payload = self._json(
            "POST",
            "/mcp",
            {
                "jsonrpc": "2.0",
                "id": self._next_rpc_id(),
                "method": "tools/call",
                "params": {
                    "name": name,
                    "arguments": {} if arguments is None else arguments,
                },
            },
            {"Mcp-Session-Id": session_id},
        )
        return self._json_rpc_result(payload)

    def _json(self, method, path, body=None, headers=None):
        payload, _headers = self._send(method, path, body, headers)
        return payload

    def _send(self, method, path, body=None, headers=None):
        data = json.dumps(body).encode("utf-8") if body is not None else None
        req = Request(f"{self.base_url}{path}", data=data, method=method)
        req.add_header("Accept", "application/json")
        if body is not None:
            req.add_header("Content-Type", "application/json")
        if self.bearer_token:
            req.add_header("Authorization", f"Bearer {self.bearer_token}")
        for name, value in (headers or {}).items():
            req.add_header(name, value)

        try:
            with urlopen(req, timeout=self.timeout) as response:
                raw = response.read().decode("utf-8")
                return (json.loads(raw) if raw else {}, response.headers)
        except HTTPError as exc:
            raw = exc.read().decode("utf-8", errors="replace")
            body = _try_json(raw)
            message = body.get("error") if isinstance(body, dict) else raw
            raise SoloHttpError(message or f"Solo HTTP {exc.code}", exc.code, body or raw) from exc
        except URLError as exc:
            raise RuntimeError(
                f"Could not reach Solo daemon at {self.base_url}. "
                "Start Solo Desktop/tray or run solo daemon."
            ) from exc

    def _next_rpc_id(self):
        self._rpc_id += 1
        return self._rpc_id

    def _session_id(self, session):
        if isinstance(session, str):
            if not session:
                raise RuntimeError("Solo MCP session id is required.")
            return session
        session_id = session.get("session_id")
        if not session_id:
            raise RuntimeError("Solo MCP session id is required.")
        return session_id

    def _json_rpc_result(self, payload):
        error = payload.get("error")
        if error:
            message = (
                error.get("message") if isinstance(error, dict) else "Solo MCP JSON-RPC error"
            )
            raise SoloMcpError(message or "Solo MCP JSON-RPC error", error)
        return payload.get("result", {})


def _try_json(text):
    try:
        return json.loads(text) if text else None
    except json.JSONDecodeError:
        return None


def _quote(value):
    return quote(str(value), safe="")


def _query(params):
    if not params:
        return ""
    return "?" + urlencode({key: str(value) for key, value in params.items()})


def _bool_query(value):
    return "true" if value else "false"
