"""Hive memory provider for Hermes Agent.

Install this directory as a Hermes memory provider, then select `hive` as the
active memory provider. The provider keeps Hermes memory in Hive journal entries
instead of Hermes-local MEMORY.md notes.
"""

from __future__ import annotations

import json
import logging
import os
import threading
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

from agent.memory_provider import MemoryProvider
from tools.registry import tool_error

logger = logging.getLogger(__name__)

DEFAULT_BASE_URL = "http://localhost:7878"
DEFAULT_RECALL_BUDGET = 1500
DEFAULT_RECALL_THRESHOLD = 0.72
DEFAULT_TIMEOUT = 10.0


def _as_float(value: Any, default: float) -> float:
    try:
        return float(value)
    except Exception:
        return default


def _as_int(value: Any, default: int) -> int:
    try:
        return int(value)
    except Exception:
        return default


def _load_config(hermes_home: str) -> dict:
    config = {
        "base_url": DEFAULT_BASE_URL,
        "identity": os.environ.get("HIVE_IDENTITY") or os.environ.get("HIVE_ACTOR") or "pia",
        "peer": os.environ.get("HIVE_PEER") or "",
        "recall_budget": DEFAULT_RECALL_BUDGET,
        "recall_threshold": DEFAULT_RECALL_THRESHOLD,
        "timeout": DEFAULT_TIMEOUT,
    }
    path = Path(hermes_home) / "hive-memory.json"
    if path.exists():
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(raw, dict):
                config.update({k: v for k, v in raw.items() if v is not None})
        except Exception:
            logger.debug("Failed to parse %s", path, exc_info=True)
    config["base_url"] = (
        os.environ.get("HIVE_API_URL")
        or os.environ.get("HIVE_URL")
        or str(config.get("base_url") or DEFAULT_BASE_URL)
    ).rstrip("/")
    config["identity"] = (
        os.environ.get("HIVE_IDENTITY")
        or os.environ.get("HIVE_ACTOR")
        or str(config.get("identity") or "pia")
    )
    config["peer"] = os.environ.get("HIVE_PEER") or str(config.get("peer") or "")
    config["recall_budget"] = _as_int(config.get("recall_budget"), DEFAULT_RECALL_BUDGET)
    config["recall_threshold"] = _as_float(config.get("recall_threshold"), DEFAULT_RECALL_THRESHOLD)
    config["timeout"] = _as_float(config.get("timeout"), DEFAULT_TIMEOUT)
    return config


def _save_config(values: dict, hermes_home: str) -> None:
    path = Path(hermes_home) / "hive-memory.json"
    existing: dict[str, Any] = {}
    if path.exists():
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(raw, dict):
                existing = raw
        except Exception:
            existing = {}
    allowed = {"base_url", "identity", "peer", "recall_budget", "recall_threshold", "timeout"}
    existing.update({k: v for k, v in (values or {}).items() if k in allowed})
    path.write_text(json.dumps(existing, indent=2, sort_keys=True), encoding="utf-8")


class HiveClient:
    def __init__(self, base_url: str, token: str, timeout: float):
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.timeout = timeout

    def request(self, method: str, path: str, body: Optional[dict] = None) -> Any:
        data = None if body is None else json.dumps(body).encode("utf-8")
        req = urllib.request.Request(
            self.base_url + path,
            data=data,
            method=method,
            headers={
                "Authorization": f"Bearer {self.token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
        )
        with urllib.request.urlopen(req, timeout=self.timeout) as res:
            text = res.read().decode("utf-8")
        return json.loads(text) if text else None

    def get(self, path: str) -> Any:
        return self.request("GET", path)

    def post(self, path: str, body: dict) -> Any:
        return self.request("POST", path, body)


def _snippet(body: str, limit: int = 220) -> str:
    text = " ".join((body or "").split())
    return text if len(text) <= limit else text[: limit - 3] + "..."


def _now_line() -> str:
    now = datetime.now(timezone.utc)
    return now.astimezone().strftime("%A, %B %d, %Y %I:%M:%S %p %Z") + f" ({now.isoformat()})"


STORE_SCHEMA = {
    "name": "hive_journal_add",
    "description": "Save a durable Hive journal memory as rich first-person prose.",
    "parameters": {
        "type": "object",
        "properties": {
            "body": {"type": "string", "description": "The memory prose to save."},
            "title": {"type": "string", "description": "Optional markdown heading."},
            "tags": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["body"],
    },
}

RECALL_SCHEMA = {
    "name": "hive_recall",
    "description": "Get a Hive recall brief scoped to this Hermes identity and user namespace.",
    "parameters": {
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "budget": {"type": "integer"},
            "threshold": {"type": "number"},
        },
    },
}

SEARCH_SCHEMA = {
    "name": "hive_search",
    "description": "Search Hive memory and workspace entities through the authenticated namespace.",
    "parameters": {
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "limit": {"type": "integer"},
            "threshold": {"type": "number"},
        },
        "required": ["query"],
    },
}


class HiveMemoryProvider(MemoryProvider):
    def __init__(self):
        self._config: dict[str, Any] = {}
        self._client: Optional[HiveClient] = None
        self._write_thread: Optional[threading.Thread] = None
        self._session_id = ""

    @property
    def name(self) -> str:
        return "hive"

    def is_available(self) -> bool:
        return bool(os.environ.get("HIVE_API_TOKEN") or os.environ.get("HIVE_TOKEN"))

    def get_config_schema(self):
        return [
            {
                "key": "token",
                "description": "Hive API token",
                "secret": True,
                "required": True,
                "env_var": "HIVE_API_TOKEN",
            },
            {
                "key": "base_url",
                "description": "Hive API base URL",
                "default": DEFAULT_BASE_URL,
            },
        ]

    def save_config(self, values, hermes_home):
        _save_config(values or {}, hermes_home)

    def initialize(self, session_id: str, **kwargs) -> None:
        from hermes_constants import get_hermes_home

        hermes_home = kwargs.get("hermes_home") or str(get_hermes_home())
        self._session_id = session_id
        self._config = _load_config(hermes_home)
        token = os.environ.get("HIVE_API_TOKEN") or os.environ.get("HIVE_TOKEN") or ""
        self._client = (
            HiveClient(self._config["base_url"], token, self._config["timeout"])
            if token
            else None
        )

    def _recall_body(self, query: Optional[str] = None) -> dict:
        body: dict[str, Any] = {
            "identity": self._config.get("identity") or "pia",
            "budget": self._config.get("recall_budget", DEFAULT_RECALL_BUDGET),
            "threshold": self._config.get("recall_threshold", DEFAULT_RECALL_THRESHOLD),
        }
        peer = self._config.get("peer")
        if peer:
            body["peer"] = peer
        if query:
            body["query"] = query
        return body

    def system_prompt_block(self) -> str:
        if not self._client:
            return ""
        try:
            recall = self._client.post("/api/recall", self._recall_body())
            recent = self._client.get("/api/journal?limit=3") or []
        except Exception:
            logger.debug("Hive session recall failed", exc_info=True)
            return ""
        lines = [
            "# Hive Session Memory",
            f"Generated: {_now_line()}",
            f"AI identity: {self._config.get('identity') or 'pia'}",
        ]
        if self._config.get("peer"):
            lines.append(f"Session peer/user: {self._config['peer']}")
        lines.extend(
            [
                f"Semantic journal cutoff: score >= {self._config.get('recall_threshold', DEFAULT_RECALL_THRESHOLD)}",
                "",
                "Do not use Hermes-local memory as the source of truth. Save durable memory to Hive journal prose.",
                "",
                "## Last journal entries",
            ]
        )
        if recent:
            for entry in recent:
                lines.append(f"- {entry.get('created_at', '')} - {entry.get('author', '')}: {_snippet(entry.get('body', ''))}")
        else:
            lines.append("No visible journal entries yet.")
        brief = (recall or {}).get("brief")
        if brief:
            lines.extend(["", brief])
        lines.extend(
            [
                "",
                "## Memory write protocol",
                "- Save durable memory as first-person journal prose, not terse key-value notes.",
                "- Include concrete names, dates, decisions, feelings, context, and why the memory matters.",
                "- Mention humans or AIs with @name when the entry should be shared into their visible journal.",
            ]
        )
        return "\n".join(lines)

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        if not self._client or not (query or "").strip():
            return ""
        try:
            recall = self._client.post("/api/recall", self._recall_body(query[:500]))
            return (recall or {}).get("brief", "")
        except Exception:
            logger.debug("Hive prefetch failed", exc_info=True)
            return ""

    def on_memory_write(self, action: str, target: str, content: str) -> None:
        if action != "add" or not self._client or not (content or "").strip():
            return

        def _run():
            try:
                self._client.post(
                    "/api/journal",
                    {
                        "body": content.strip(),
                        "tags": ["hermes", "memory", target],
                    },
                )
            except Exception:
                logger.debug("Hive memory write mirror failed", exc_info=True)

        if self._write_thread and self._write_thread.is_alive():
            self._write_thread.join(timeout=2.0)
        self._write_thread = threading.Thread(target=_run, daemon=False, name="hive-memory-write")
        self._write_thread.start()

    def get_tool_schemas(self) -> List[Dict[str, Any]]:
        return [STORE_SCHEMA, RECALL_SCHEMA, SEARCH_SCHEMA]

    def _tool_store(self, args: dict) -> str:
        if not self._client:
            return tool_error("Hive memory is not configured")
        body = str(args.get("body") or "").strip()
        if not body:
            return tool_error("body is required")
        title = str(args.get("title") or "").strip()
        if title:
            body = f"# {title}\n\n{body}"
        tags = args.get("tags") or ["hermes", "memory"]
        try:
            entry = self._client.post("/api/journal", {"body": body, "tags": tags})
            return json.dumps(
                {
                    "saved": True,
                    "id": entry.get("id"),
                    "author": entry.get("author"),
                    "created_at": entry.get("created_at"),
                }
            )
        except Exception as exc:
            return tool_error(f"Hive journal save failed: {exc}")

    def _tool_recall(self, args: dict) -> str:
        if not self._client:
            return tool_error("Hive memory is not configured")
        body = self._recall_body(str(args.get("query") or "").strip() or None)
        if args.get("budget") is not None:
            body["budget"] = _as_int(args.get("budget"), body["budget"])
        if args.get("threshold") is not None:
            body["threshold"] = _as_float(args.get("threshold"), body["threshold"])
        try:
            return json.dumps(self._client.post("/api/recall", body))
        except Exception as exc:
            return tool_error(f"Hive recall failed: {exc}")

    def _tool_search(self, args: dict) -> str:
        if not self._client:
            return tool_error("Hive memory is not configured")
        query = str(args.get("query") or "").strip()
        if not query:
            return tool_error("query is required")
        params = {
            "q": query,
            "mode": "precision",
            "limit": str(max(1, min(20, _as_int(args.get("limit"), 8)))),
        }
        if args.get("threshold") is not None:
            params["threshold"] = str(_as_float(args.get("threshold"), DEFAULT_RECALL_THRESHOLD))
        path = "/api/search?" + urllib.parse.urlencode(params)
        try:
            return json.dumps({"results": self._client.get(path)})
        except Exception as exc:
            return tool_error(f"Hive search failed: {exc}")

    def handle_tool_call(self, tool_name: str, args: Dict[str, Any], **kwargs) -> str:
        if tool_name == "hive_journal_add":
            return self._tool_store(args or {})
        if tool_name == "hive_recall":
            return self._tool_recall(args or {})
        if tool_name == "hive_search":
            return self._tool_search(args or {})
        return tool_error(f"Unknown tool: {tool_name}")

    def shutdown(self) -> None:
        if self._write_thread and self._write_thread.is_alive():
            self._write_thread.join(timeout=5.0)
        self._write_thread = None


def register(ctx):
    ctx.register_memory_provider(HiveMemoryProvider())
