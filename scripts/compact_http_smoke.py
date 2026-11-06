"""Exercise actual percentage-triggered CLI compaction against loopback HTTP only."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def run(binary):
    with tempfile.TemporaryDirectory(prefix="aizen-compact-smoke-") as temporary:
        root = Path(temporary)
        home, workspace = root / "home", root / "workspace"
        home.mkdir()
        workspace.mkdir()
        (home / "sessions").mkdir()
        (workspace / "fixture.txt").write_text("".join(f"local read fixture line {i}\n" for i in range(1, 13)), encoding="utf-8")
        state = {"summary": [], "main": [], "metadata": [], "phase": "success", "step": 0, "errors": []}

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def send_json(self, status, value):
                encoded = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

            def do_GET(self):
                state["metadata"].append((self.path, self.headers.get("Authorization")))
                self.send_json(200, {"data": [
                    {"id": "main-fixture", "context_window": 200000},
                    {"id": "summary-fixture", "context_window": 4096, "max_output_tokens": 512},
                ]})

            def do_POST(self):
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    if body["model"] == "summary-fixture":
                        state["summary"].append(body)
                        assert not body.get("tools") and not body.get("stream")
                        assert body.get("max_tokens") == 512, "main output cap leaked to summarizer"
                        text = "\n".join(m.get("content", "") for m in body["messages"])
                        assert len(text) < 15000, "unbounded transcript sent to small summarizer"
                        if state["phase"] == "overflow" and len(text) > 4000:
                            self.send_json(400, {"error": {"code": "context_length_exceeded", "message": "context window exceeded"}})
                            return
                        if state["phase"] == "empty":
                            content = ""
                        elif state["phase"] == "timeout":
                            import time
                            time.sleep(2)
                            content = "late summary"
                        else:
                            content = "COMPACTED_FIXTURE: preserve the current task and read fixture.txt."
                        finish = "length" if state["phase"] == "truncated" else "stop"
                        self.send_json(200, {"choices": [{"index": 0, "finish_reason": finish, "message": {"role": "assistant", "content": content}}]})
                        return
                    assert body["model"] == "main-fixture"
                    state["main"].append(body)
                    assert body.get("stream"), "fixture expects the real streaming CLI"
                    contents = "\n".join(str(m.get("content", "")) for m in body["messages"])
                    if state["phase"] in ("success", "overflow"):
                        assert "COMPACTED_FIXTURE" in contents, "summary missing from main request"
                        assert "Tiếng Việt 漢字 abc identifier\n" * 40 not in contents, "old transcript unexpectedly retained"
                    elif state["phase"] in ("empty", "truncated", "timeout", "disabled", "high-threshold"):
                        assert "OLD_BLOCK_MARKER" in contents, "failed/disabled compaction lost original history"
                    if state["phase"] == "live" and state["step"] >= 8:
                        assert "COMPACTED_FIXTURE" in contents, "single-turn run failed to compact between tool steps"
                        assert "LIVE_STEP_0 " + "g" * 1000 not in contents
                    step = state["step"]
                    state["step"] += 1
                    tool_steps = 8 if state["phase"] == "live" else 1
                    if step < tool_steps:
                        delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": f"fixture-read-{step}", "type": "function", "function": {"name": "file_read", "arguments": json.dumps({"path": "fixture.txt", "start": step + 1, "end": step + 1})}}]}
                        if state["phase"] == "live":
                            delta["content"] = f"LIVE_STEP_{step} " + "g" * 16000
                        finish = "tool_calls"
                    else:
                        assert any(m.get("role") == "tool" and m.get("tool_call_id") == f"fixture-read-{step - 1}" for m in body["messages"])
                        delta = {"role": "assistant", "content": "Fixture task completed after compaction boundary."}
                        finish = "stop"
                    events = [
                        {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                        {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
                    ]
                    encoded = b"".join(b"data: " + json.dumps(event).encode() + b"\n\n" for event in events) + b"data: [DONE]\n\n"
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(encoded)))
                    self.end_headers()
                    self.wfile.write(encoded)
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass
                except Exception as error:
                    state["errors"].append(repr(error))
                    self.send_json(400, {"error": {"message": "loopback fixture assertion failed"}})

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{server.server_port}/v1"
        env = {key: value for key, value in os.environ.items()
               if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_"))
               and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")}
        env.update({"AIZEN_HOME": str(home), "AIZEN_ARCHITECT": "0"})
        config = {
            "base_url": base, "api_key": "fixture-main", "model": "main-fixture",
            "architect_mode": False, "self_review": False, "eager_tools": False,
            "max_tokens": 64000, "compact_threshold_pct": 10,
            "roles": {"summarizer": {"model": "summary-fixture", "base_url": base, "api_key_ref": "fixture-summary"}},
        }
        seed = [
            {"role": "system", "content": "Synthetic offline fixture. Follow the current task."},
            {"role": "user", "content": "OLD_BLOCK_MARKER " + "Tiếng Việt 漢字 abc identifier\n" * 1800},
            {"role": "assistant", "content": "Old task processed."},
            {"role": "user", "content": "Second old task."},
            {"role": "assistant", "content": "Second old task processed."},
            {"role": "user", "content": "Third old task."},
            {"role": "assistant", "content": "Third old task processed."},
        ]
        try:
            for phase, threshold in [("success", 10), ("live", 10), ("overflow", 10), ("disabled", 0), ("high-threshold", 90), ("empty", 10), ("truncated", 10), ("timeout", 10)]:
                state.update({"phase": phase, "step": 0, "main": [], "summary": []})
                config["compact_threshold_pct"] = threshold
                path = home / "cli-config.json"
                path.write_text(json.dumps(config), encoding="utf-8")
                before = path.read_bytes()
                (home / "sessions" / "fixture.json").write_text(json.dumps(seed), encoding="utf-8")
                child_env = dict(env)
                if phase == "timeout":
                    child_env["AIZEN_CHORE_CALL_SECS"] = "1"
                arguments = [str(binary), "agent", "Read fixture.txt, then report completion. Do not edit files.",
                    "--base-url", base, "--api-key", "fixture-main", "--model", "main-fixture",
                    "--output-format", "stream-json", "--max-iters", "12", "--yes"]
                if phase != "live":
                    arguments += ["--resume", "fixture"]
                result = subprocess.run(arguments, cwd=workspace, env=child_env, input="", capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=45)
                assert result.returncode == 0, (phase, result.returncode, result.stderr[-2000:], result.stdout[-2000:], state["errors"])
                assert not state["errors"], state["errors"]
                events = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
                boundaries = [e for e in events if e.get("subtype") == "compact_boundary"]
                results = [e for e in events if e.get("type") == "result"]
                assert results and results[-1].get("stop") == "done", (phase, results)
                if phase in ("success", "live", "overflow"):
                    assert len(state["summary"]) > 2, "large transcript did not use bounded portions plus merge"
                    assert len(boundaries) == 1, boundaries
                    if phase == "overflow":
                        sizes = [len("\n".join(m.get("content", "") for m in request["messages"]))
                            for request in state["summary"]]
                        assert sizes[0] > 4000 and any(size <= 4000 for size in sizes[1:]), sizes
                else:
                    assert not boundaries, (phase, boundaries)
                if phase in ("disabled", "high-threshold"):
                    assert not state["summary"], phase
                expected_main = 9 if phase == "live" else 2
                assert len(state["main"]) == expected_main, (phase, len(state["main"]))
                assert path.read_bytes() == before, "discovery rewrote user config"
                print(f"PASS {phase}: summaries={len(state['summary'])}, main_calls={expected_main}, boundaries={len(boundaries)}")
            assert len(state["metadata"]) == 2, state["metadata"]
            cache = list((home / "cache" / "model-context").glob("*.json"))
            assert len(cache) == 2
            for path in cache:
                assert len(path.stem) == 64
                payload = path.read_text(encoding="utf-8")
                assert "fixture" not in payload and base not in payload
            print("PASS route/account cache reused across CLI processes; config unchanged; numeric-only cache")
            config["model_context_window"] = 32000
            path = home / "cli-config.json"
            path.write_text(json.dumps(config), encoding="utf-8")
            for flags, expected in [
                (["--base-url", base + "/", "--model", "main-fixture"], 32000),
                (["--api-key", "another-fixture-account"], None),
                (["--api-key", "fixture-main", "--context-window", "48000"], 48000),
                (["--base-url", base + "/alternate"], None),
            ]:
                result = subprocess.run([str(binary), "config", "set", *flags], cwd=workspace,
                    env=env, input="", capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=15)
                assert result.returncode == 0, (flags, result.returncode, result.stderr[-1000:])
                assert json.loads(path.read_text(encoding="utf-8")).get("model_context_window") == expected
            print("PASS context override follows the exact connection; same-route and explicit overrides retained")
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    run(parser.parse_args().binary.resolve())
