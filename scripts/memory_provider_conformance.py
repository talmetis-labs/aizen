"""Drive an independent native Memory provider over the actual stdio process boundary.

This is a protocol/example-provider fixture, not evidence of Aizen integration or Mnemos parity.
Uses synthetic content and owned temporary HOME/data roots. Run with Python -I.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


MAX_FRAME = 1024 * 1024


def strict_object(pairs: list[tuple[str, object]]) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix="memory-provider-wire-"))
    home = root / "home"
    home.mkdir()
    workspace = root / "workspace"
    workspace.mkdir()
    data = root / "data"
    # Do not inherit credentials, proxy settings or arbitrary host configuration.
    allowed = {"SYSTEMROOT", "WINDIR", "SYSTEMDRIVE", "COMSPEC", "PATHEXT"}
    env = {key: value for key, value in os.environ.items() if key.upper() in allowed}
    env.update({"HOME": str(home), "USERPROFILE": str(home), "TEMP": str(root), "TMP": str(root),
                "APPDATA": str(home), "LOCALAPPDATA": str(home), "PATH": str(binary.parent)})
    results = []

    def request(identifier: int, method: str, params: dict) -> bytes:
        return (json.dumps({"jsonrpc": "2.0", "id": identifier, "method": method,
                            "params": params}, separators=(",", ":")) + "\n").encode("utf-8")

    def initialize(project: str = "fixture-project") -> bytes:
        return request(1, "initialize", {"protocol": {"major": 1, "minor": 0}, "context": {
            "data_root": str(data), "workspace_id": project, "workspace_root": str(workspace),
            "cwd": str(workspace), "user_id": "fixture-user", "device_id": "fixture-device",
            "device_aliases": [], "session_id": "fixture-session", "turn_id": "fixture-turn",
            "boundary_id": "fixture-boundary", "generation": 1,
        }})

    def exchange(label: str, frames: bytes, *, good: bool = True) -> list[dict]:
        start = time.perf_counter()
        completed = subprocess.run([str(binary)], input=frames, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, cwd=workspace, env=env, timeout=15, check=False)
        assert len(completed.stdout) <= 16 * MAX_FRAME, label
        assert len(completed.stderr) <= MAX_FRAME, label
        if good:
            assert completed.returncode == 0, (label, completed.returncode, completed.stderr[:512])
        else:
            assert completed.returncode != 0, label
        responses = []
        for frame in completed.stdout.splitlines(keepends=True):
            assert frame.endswith(b"\n") and len(frame) <= MAX_FRAME, label
            response = json.loads(frame, object_pairs_hook=strict_object)
            assert set(response) in ({"jsonrpc", "id", "result"}, {"jsonrpc", "id", "error"}), label
            assert response["jsonrpc"] == "2.0", label
            responses.append(response)
        results.append({"label": label, "exit": completed.returncode,
                        "seconds": round(time.perf_counter() - start, 6), "responses": len(responses)})
        return responses

    def call(label: str, method: str, params: dict, project: str = "fixture-project") -> dict:
        responses = exchange(label, initialize(project) + request(2, method, params) + request(3, "shutdown", {}))
        assert [response["id"] for response in responses] == [1, 2, 3], label
        assert responses[0]["result"]["protocol"] == {"major": 1, "minor": 0}, label
        return responses[1]

    assert call("health-empty", "health", {})["result"]["status"] == "ready"
    assert not data.exists(), "initialize/health must not create data"
    first_params = {"operation_id": "store-first", "content": "Fixture project uses Rust", "scope": "project"}
    first = call("store", "store", first_params)["result"]
    record_id = first["record"]["id"]
    assert call("same-event-new-process", "store", first_params)["result"] == first
    changed = dict(first_params, content="Different payload")
    assert "error" in call("same-id-different-payload", "store", changed)
    assert "error" in call("cross-project-receipt", "store", first_params, "other-project")
    assert len(call("list", "list", {})["result"]["items"]) == 1
    assert not call("scope-isolation", "list", {}, "other-project")["result"]["items"]
    hits = call("recall", "recall", {"query": "Rust", "limit": 3})["result"]["items"]
    assert hits[0]["record"]["id"] == record_id and 0 <= hits[0]["score"] <= 1
    assert "error" in call("revision-conflict", "update", {"operation_id": "bad-revision", "id": record_id,
                                                               "expected_revision": 100, "content": "No"})
    update = call("update", "update", {"operation_id": "update-first", "id": record_id,
                                         "expected_revision": 1, "content": "Fixture project uses Rust and tests"})
    assert update["result"]["record"]["revision"] == 2
    assert call("get", "get", {"id": record_id})["result"]["record"]["revision"] == 2
    assert "error" in call("unsupported-learning", "process_turn", {"operation_id": "learn-first"})
    archived = call("forget", "forget", {"operation_id": "forget-first", "id": record_id, "expected_revision": 2})
    assert archived["result"]["record"]["archived"]
    assert not call("archive-not-recalled", "list", {})["result"]["items"]
    exchange("duplicate-json-key", b'{"jsonrpc":"2.0","id":1,"id":2,"method":"initialize"}\n', good=False)
    exchange("invalid-utf8", b'\xff\n', good=False)
    exchange("truncated-frame", b'{}', good=False)
    exchange("oversized-frame", b'x' * (MAX_FRAME + 1) + b'\n', good=False)
    exchange("duplicate-request-id", initialize() + request(1, "health", {}), good=False)
    exchange("batch", b'[]\n', good=False)
    (data / "records.next").write_text("interrupted fixture", encoding="utf-8")
    assert "error" in call("interrupted-commit-fails-closed", "list", {})
    report = {"binary": str(binary), "root": str(root), "cases": results, "status": "passed",
              "limitations": ["No Core integration tested", "No hostile-process containment tested",
                              "Example store is not the Mnemos engine"]}
    (root / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"PASS: {len(results)} executable protocol cases. Report: {root / 'report.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
