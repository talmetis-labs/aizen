"""Disposable actual-Core localhost lifetime regression; no external provider or browser.

Run with Python -I. The fixture binds port 0, reports its actual URL/PID, and
answers HTTP 200/404/500. All configuration and session data are synthetic.
On Windows, verifies normal one-shot exit and owner crash close the managed
server's Job Object while a separately owned terminal-style server stays up.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, build_opener


class Document(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        status = 404 if self.path == "/missing" else 500 if self.path == "/error" else 200
        body = b"disposable preview document"
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def fixture_server(marker):
    with ThreadingHTTPServer(("127.0.0.1", 0), Document) as server:
        marker.write_text(json.dumps({"pid": os.getpid(), "url": f"http://127.0.0.1:{server.server_port}/"}), encoding="utf-8")
        print(f"Ready: http://127.0.0.1:{server.server_port}/", flush=True)
        server.serve_forever()


def http_status(url):
    try:
        with build_opener(ProxyHandler({})).open(url, timeout=0.5) as response:
            return response.status
    except HTTPError as error:
        return error.code  # An HTTP error document is not a disconnected transport.
    except (URLError, OSError):
        return None


def wait_for(predicate, description, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.025)
    raise AssertionError(f"timed out: {description}")


def read_marker(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def in_windows_job(pid):
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.IsProcessInJob.argtypes = [wintypes.HANDLE, wintypes.HANDLE, ctypes.POINTER(wintypes.BOOL)]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.OpenProcess(0x1000, False, pid)
    assert handle, "fixture process must exist"
    try:
        contained = wintypes.BOOL()
        assert kernel.IsProcessInJob(handle, None, ctypes.byref(contained))
        return bool(contained.value)
    finally:
        kernel.CloseHandle(handle)


def run(binary, diagnostics):
    assert os.name == "nt", "owner-exit regression currently asserts Windows Job Object behavior only"
    with tempfile.TemporaryDirectory(prefix="aizen-preview-owner-") as temporary:
        root = Path(temporary)
        home, workspace = root / "home", root / "workspace"
        home.mkdir()
        workspace.mkdir()
        config = home / "cli-config.json"
        config.write_text(json.dumps({"architect_mode": False, "self_review": False, "eager_tools": False}), encoding="utf-8")
        config_before = config.read_bytes()
        env = {key: value for key, value in os.environ.items()
               if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_"))
               and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")}
        env.update({"AIZEN_HOME": str(home), "AIZEN_ARCHITECT": "0", "GIT_CONFIG_NOSYSTEM": "1",
                    "GIT_CONFIG_GLOBAL": str(home / "empty-gitconfig")})
        script = str(Path(__file__).resolve())
        manual_marker = root / "terminal.json"
        manual = subprocess.Popen([sys.executable, "-I", script, "--fixture-server", str(manual_marker)],
                                  cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        state = {}

        class Model(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def send_json(self, value):
                encoded = json.dumps(value).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

            def do_GET(self):
                self.send_json({"data": [{"id": "preview-fixture", "context_window": 200000}]})

            def do_POST(self):
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    if not body.get("stream"):
                        self.send_json({"choices": [{"message": {"role": "assistant", "content": "No durable learning for this disposable fixture."}, "finish_reason": "stop"}]})
                        return
                    step = state["step"]
                    state["step"] += 1
                    if step == 0:
                        name, args = "process", {"action": "start", "command": state["command"]}
                    elif step == 1:
                        outputs = [str(m.get("content", "")) for m in body["messages"] if m.get("role") == "tool"]
                        match = re.search(r"\bproc_\d+\b", "\n".join(outputs))
                        assert match, outputs
                        state["handle"] = match[0]
                        ready = wait_for(lambda: read_marker(state["marker"]), "managed readiness marker")
                        state["ready"] = ready
                        assert in_windows_job(ready["pid"]), "managed fixture must be contained"
                        wait_for(lambda: http_status(ready["url"]) == 200, "HTTP readiness during Core run")
                        assert http_status(ready["url"] + "missing") == 404
                        assert http_status(ready["url"] + "error") == 500
                        state["reachable"].set()
                        name, args = "process", {"action": "log", "id": match[0]}
                    else:
                        state["finish"].wait(15)
                        name, args = None, "Fixture server tested; one-shot task complete."
                    if name:
                        delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": f"fixture-process-{step}", "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}]}
                    else:
                        delta = {"role": "assistant", "content": args}
                    events = [{"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                              {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls" if name else "stop"}]}]
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
                    self.send_json({"error": {"message": "fixture failed"}})

        model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
        model.daemon_threads = True
        threading.Thread(target=model.serve_forever, daemon=True).start()
        child = None
        try:
            terminal = wait_for(lambda: read_marker(manual_marker), "terminal-owned fixture")
            for phase in ("normal", "crash"):
                marker = root / (phase + ".json")
                argv = [sys.executable, "-I", script, "--fixture-server", str(marker)]
                command = subprocess.list2cmdline(argv) if os.name == "nt" else shlex.join(argv)
                state.clear()
                state.update(step=0, command=command, marker=marker, errors=[], reachable=threading.Event(), finish=threading.Event())
                args = [str(binary), "agent", "Start the disposable fixture server using process, inspect its log, then finish. No edits.",
                        "--base-url", f"http://127.0.0.1:{model.server_port}/v1", "--api-key", "fixture-not-a-secret", "--model", "preview-fixture",
                        "--output-format", "stream-json", "--max-iters", "5", "--save-session"]
                if diagnostics:
                    args.append("--host-preview-diagnostics")
                child = subprocess.Popen(args, cwd=workspace, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                         stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
                events, stderr, reader_errors = [], [], []

                def read_output():
                    try:
                        for line in child.stdout:
                            event = json.loads(line)
                            events.append(event)
                            if event.get("type") == "control_request":
                                reply = {"type": "control_response", "response": {"subtype": "success", "request_id": event["request_id"], "response": {"behavior": "allow", "updatedInput": event["request"].get("input", {})}}}
                                child.stdin.write(json.dumps(reply) + "\n")
                                child.stdin.flush()
                    except Exception as error:
                        reader_errors.append(repr(error))

                reader = threading.Thread(target=read_output, daemon=True)
                err_reader = threading.Thread(target=lambda: stderr.append(child.stderr.read()), daemon=True)
                reader.start()
                err_reader.start()
                wait_for(lambda: state["reachable"].is_set() or child.poll() is not None, "managed HTTP probe or CLI exit", 30)
                assert state["reachable"].is_set(), (state["errors"], stderr, events[-4:])
                assert http_status(terminal["url"]) == 200
                if phase == "crash":
                    child.kill()
                state["finish"].set()
                code = child.wait(timeout=30)
                reader.join(timeout=5)
                err_reader.join(timeout=5)
                assert not reader.is_alive() and not reader_errors, reader_errors
                assert not state["errors"], state["errors"]
                wait_for(lambda: http_status(state["ready"]["url"]) is None, "managed port closes after owner exit")
                assert http_status(terminal["url"]) == 200, "unrelated terminal owner must survive"
                results = [event for event in events if event.get("type") == "result"]
                if phase == "normal":
                    assert code == 0, (code, stderr)
                    assert len(results) == 1 and results[0]["stop"] == "done", results
                    assert any(event.get("subtype") == "session_saved" for event in events)
                    init = next(event for event in events if event.get("subtype") == "init")
                    owner = [event for event in events if event.get("subtype") == "preview_owner_exiting"]
                    if diagnostics:
                        assert init["capabilities"]["host_preview"]["retention"] is False
                        assert len(owner) == 1 and owner[0]["retained"] is False
                        assert owner[0]["readiness"] == "unverified"
                        assert any(job["id"] == state["handle"] and job["running"] for job in owner[0]["processes"])
                        assert events.index(owner[0]) < events.index(results[0])
                    else:
                        assert "capabilities" not in init and not owner, "ordinary stream protocol changed"
                else:
                    assert not results, "crashed run cannot claim completion"
                print(f"PASS {phase}: managed HTTP 200/404/500 during run; Job Object confirmed; disconnected after Core exit; terminal fixture retained; results={len(results)}; diagnostics={diagnostics}")
                child.stdin.close()
                child.stdout.close()
                child.stderr.close()
                child = None
            assert config.read_bytes() == config_before
        finally:
            state.get("finish", threading.Event()).set()
            if child is not None and child.poll() is None:
                child.kill()
                child.wait(timeout=5)
            ready = state.get("ready")
            if ready and http_status(ready["url"]) is not None:
                # Only our marker's disposable server PID; never enumerate or kill user processes.
                os.kill(ready["pid"], 9)
            manual.terminate()
            manual.wait(timeout=5)
            model.shutdown()
            model.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path, nargs="?")
    parser.add_argument("--diagnostics", action="store_true")
    parser.add_argument("--fixture-server", type=Path)
    options = parser.parse_args()
    if options.fixture_server:
        fixture_server(options.fixture_server)
    else:
        assert options.binary, "pass the Core binary"
        run(options.binary.resolve(), options.diagnostics)
