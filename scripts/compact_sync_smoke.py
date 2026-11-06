"""Verify CLI /compact is durable while idle and readable by the production Desktop reader.

Windows ConPTY, synthetic sessions, loopback HTTP only. Optionally pass the built Desktop
Rust test executable to check its real session projection at the process boundary.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def run(binary, desktop_tests):
    spec = importlib.util.spec_from_file_location("aizen_fixture_terminal", Path(__file__).with_name("vision_tui_smoke.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    with tempfile.TemporaryDirectory(prefix="aizen-compact-sync-") as temporary:
        root = Path(temporary)
        requests = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def respond(self, value):
                data = json.dumps(value).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                self.respond({"data": [{"id": "sync-fixture", "context_window": 128000}]})

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(body)
                self.respond({"choices": [{"finish_reason": "stop", "message": {
                    "role": "assistant", "content": "SYNC_FIXTURE_SUMMARY: original goal, decisions and pending work."
                }}]})

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        results = []
        try:
            for surface in ("plain", "retained"):
                home, workspace = root / (surface + "-home"), root / (surface + "-workspace")
                (home / "sessions").mkdir(parents=True)
                workspace.mkdir()
                (home / "cli-config.json").write_text(json.dumps({
                    "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                    "api_key": "loopback-placeholder", "model": "sync-fixture",
                    "onboarded": True, "approval_mode": "yolo", "auto_effort": False,
                    "architect_mode": False, "self_review": False, "auto_skill_learn": False,
                    "persona_evolve": False, "update_check": False, "eager_tools": False,
                    "compact_threshold_pct": 0,
                }), encoding="utf-8")
                slug = "original-sync-topic-1029"
                path = home / "sessions" / (slug + ".json")
                generation = 1798000000000000001
                original = {"version": 2, "generation": generation, "meta": {
                    "model": "sync-fixture", "project_root": str(workspace),
                    "created": "2020-01-01T00:00:00Z",
                }, "messages": [{"role": "system", "content": "Synthetic instructions"}] + [
                    message for i in range(7) for message in (
                        {"role": "user", "content": f"Original sync topic {i}: " + "old detail " * 300},
                        {"role": "assistant", "content": f"Answer {i}"},
                    )
                ]}
                path.write_text(json.dumps(original), encoding="utf-8")
                path.with_suffix(".jsonl").write_text(json.dumps({
                    "role": "assistant", "content": "committed delta tail", "_session_generation": generation,
                }) + "\n", encoding="utf-8")
                env = {key: value for key, value in os.environ.items()
                       if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_"))
                       and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")}
                env.update({"AIZEN_HOME": str(home), "AIZEN_NO_UPDATE_CHECK": "1", "AIZEN_NO_WARMUP": "1",
                            "AIZEN_NO_TIPS": "1", "AIZEN_NO_HOOKS": "1"})
                if surface == "plain":
                    env["AIZEN_NO_STICKY"] = "1"
                terminal = module.Terminal(Path(sys.executable), workspace, env, [
                    "-I", str(Path(__file__).with_name("vision_tui_smoke.py")), "--console-child", str(binary),
                ])
                try:
                    terminal.wait("❯")
                    threading.Event().wait(0.5)  # The initial frame can precede keyboard raw mode.
                    terminal.enter("/resume " + slug, slug)
                    terminal.wait("context restored")
                    terminal.enter("/compact", "/compact")
                    terminal.wait("Compacted", timeout=40)
                    assert terminal.kernel.WaitForSingleObject(terminal.process.hProcess, 0) == 0x102
                    compacted = json.loads(path.read_text(encoding="utf-8"))
                    assert compacted["generation"] != generation, "compact success was not persisted while CLI idle"
                    assert compacted["meta"]["created"] == original["meta"]["created"]
                    assert compacted["meta"]["project_root"] == str(workspace)
                    assert compacted["messages"][-1]["content"] == "committed delta tail"
                    assert "SYNC_FIXTURE_SUMMARY" in json.dumps(compacted)
                    assert not path.with_suffix(".jsonl").exists()
                    assert sorted(p.stem for p in path.parent.glob("*.json")) == [slug]
                    # A late old-generation writer cannot resurrect the summarized history.
                    path.with_suffix(".jsonl").write_text(json.dumps({
                        "role": "user", "content": "STALE_SYNC_FIXTURE", "_session_generation": generation,
                    }) + "\n", encoding="utf-8")
                    if desktop_tests:
                        checked = subprocess.run([str(desktop_tests), "--exact", "chat::tests::external_cli_compact_fixture", "--ignored", "--nocapture"],
                            env={**env, "AIZEN_DESKTOP_SESSION_FIXTURE": str(path)}, capture_output=True,
                            text=True, encoding="utf-8", errors="replace", timeout=20)
                        assert checked.returncode == 0, checked.stdout + checked.stderr
                        assert "1 passed" in checked.stdout, checked.stdout
                    results.append({"surface": surface, "saved_while_cli_idle": True, "same_slug": True,
                                    "desktop_reader": "passed" if desktop_tests else "not run"})
                finally:
                    terminal.close()  # Only this fixture's CLI/shell tree.
            assert len(requests) == 2
            assert all(not body.get("stream") and not body.get("tools") for body in requests)
            print(json.dumps({"ok": True, "cases": results,
                              "scope": "Real CLI /resume + /compact through ConPTY, loopback summary; production Desktop Rust reader, not native Desktop UI."}, indent=2))
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--desktop-tests", type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(), args.desktop_tests.resolve() if args.desktop_tests else None)
