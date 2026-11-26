"""Bounded Windows ConPTY regression for real CLI clarify/custom-answer input.

Only a disposable home/workspace and loopback completion fixture are used. Run:
python -I scripts/ask_tui_smoke.py target/release/aizen.exe
"""
import argparse
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# -I excludes the script directory: explicitly load the trusted sibling harness.
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("aizen_vision_tui_fixture", Path(__file__).with_name("vision_tui_smoke.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
Terminal = fixture.Terminal

QUESTIONS = {"questions": [
    {"question": "Fixture credential choice?", "header": "Credential", "options": ["Router", "Account", "Explicit"]},
    {"question": "Fixture checks?", "header": "Checks", "options": ["Format", "Test"], "multi_select": True},
]}


def screen_text(raw):
    """Reconstruct cursor-addressed retained frames rather than concatenate their diffs."""
    text = raw.decode("utf-8", errors="replace")
    cells = {}
    row = col = 0
    tokens = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[([0-?]*)([ -/]*)([@-~])|\x1b.|[^\x1b]", re.DOTALL)
    for token in tokens.finditer(text):
        value = token.group()
        if value.startswith("\x1b["):
            params, _, code = token.groups()
            nums = [int(n) if n else 0 for n in params.lstrip("?<>=").split(";")]
            n = nums[0] or 1
            if code in ("H", "f"):
                row, col = max(0, n - 1), max(0, (nums[1] if len(nums) > 1 else 1) - 1)
            elif code == "A": row = max(0, row - n)
            elif code == "B": row += n
            elif code == "C": col += n
            elif code == "D": col = max(0, col - n)
            elif code == "G": col = n - 1
            elif code == "d": row = n - 1
            elif code == "J":
                mode = nums[0]
                for position in list(cells):
                    if mode in (2, 3) or (mode == 0 and position >= (row, col)) or (mode == 1 and position <= (row, col)):
                        del cells[position]
            elif code == "K":
                for position in list(cells):
                    if position[0] == row and (nums[0] == 2 or (nums[0] == 0 and position[1] >= col) or (nums[0] == 1 and position[1] <= col)):
                        del cells[position]
        elif value.startswith("\x1b"):
            continue
        elif value == "\r": col = 0
        elif value == "\n": row += 1
        elif value == "\b": col = max(0, col - 1)
        elif value >= " ":
            if unicodedata.combining(value):
                cells[(row, max(0, col - 1))] = cells.get((row, max(0, col - 1)), "") + value
            else:
                cells[(row, col)] = value
                col += 2 if unicodedata.east_asian_width(value) in ("W", "F") else 1
    return "\n".join("".join(cells.get((r, c), " ") for c in range(160)) for r in range(max([r for r, _ in cells], default=0) + 1))


def run(binary, evidence):
    if os.name != "nt":
        raise SystemExit("This harness requires Windows ConPTY.")
    evidence.mkdir(parents=True, exist_ok=True)
    records, errors, passes = [], [], []
    changed = threading.Condition()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.respond(json.dumps({"data": [{"id": "ask-fixture"}]}).encode(), "application/json")

        def respond(self, payload, content_type):
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_POST(self):
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 < size <= 8 * 1024 * 1024:
                self.send_error(413)
                return
            try:
                body = json.loads(self.rfile.read(size))
                messages = body.get("messages", [])
                users = [m for m in messages if m.get("role") == "user"]
                content = users[-1].get("content", "") if users else ""
                if isinstance(content, list):
                    content = "\n".join(p.get("text", "") for p in content)
                # Text is synthetic and stays in the disposable evidence directory.
                with changed:
                    records.append(content)
                    index = len(records)
                    changed.notify_all()
                if "ASK_FIXTURE_" in content:
                    delta = {"tool_calls": [{"index": 0, "id": f"ask-{index}", "type": "function",
                             "function": {"name": "clarify", "arguments": json.dumps(QUESTIONS)}}]}
                    finish = "tool_calls"
                else:
                    delta = {"content": f"Fixture acknowledged {index}."}
                    finish = "stop"
                chunks = [{"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                          {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}]
                payload = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n"
                self.respond(payload.encode(), "text/event-stream")
            except Exception as error:
                with changed:
                    errors.append(repr(error))
                    changed.notify_all()
                raise

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    terminal = None
    stage = "startup"
    try:
        with contextlib.ExitStack() as resources:
            root = Path(resources.enter_context(tempfile.TemporaryDirectory(prefix="aizen-ask-tui-")))
            home, workspace = root / "home", root / "workspace"
            home.mkdir()
            workspace.mkdir()
            (home / "cli-config.json").write_text(json.dumps({
                "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                "api_key": "loopback-placeholder", "model": "ask-fixture", "onboarded": True,
                "approval_mode": "ask", "auto_effort": False, "architect_mode": False,
                "self_review": False, "auto_skill_learn": False, "persona_evolve": False,
                "memory_auto_learn": False, "update_check": False, "eager_tools": False,
            }), encoding="utf-8")
            env = {key: value for key, value in os.environ.items()
                   if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_"))
                   and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY")}
            env.update({"AIZEN_HOME": str(home), "AIZEN_NO_UPDATE_CHECK": "1", "AIZEN_NO_WARMUP": "1",
                        "AIZEN_NO_TIPS": "1", "AIZEN_NO_HOOKS": "1"})
            terminal = Terminal(Path(sys.executable), workspace, env,
                                ["-I", str(Path(__file__).resolve()), "--console-child", str(binary)])

            def save_and_close():
                (evidence / "terminal.raw").write_bytes(bytes(terminal.data))
                (evidence / "terminal.txt").write_text(terminal.text(), encoding="utf-8")
                (evidence / "screen.txt").write_text(screen_text(bytes(terminal.data)), encoding="utf-8")
                terminal.close()

            # Stop the fixture process before TemporaryDirectory removes its working directory.
            resources.callback(save_and_close)

            def checkpoint():
                with terminal.changed:
                    return len(terminal.data)

            def wait_new(marker, offset, timeout=20):
                deadline = time.monotonic() + timeout
                with terminal.changed:
                    while not (len(terminal.data) > offset and marker in screen_text(bytes(terminal.data))):
                        if time.monotonic() >= deadline:
                            raise AssertionError(f"{stage}: no fresh {marker!r}; tail={terminal.text()[-2400:]!r}")
                        terminal.changed.wait(0.1)

            def key(data, marker=None):
                offset = checkpoint()
                terminal.send(data)
                if marker:
                    wait_new(marker, offset)
                threading.Event().wait(0.15)

            def request_count(count, expected):
                deadline = time.monotonic() + 25
                with changed:
                    while len(records) < count and not errors:
                        if time.monotonic() >= deadline:
                            raise AssertionError(f"{stage}: request {count} missing; tail={terminal.text()[-2400:]!r}")
                        changed.wait(0.1)
                    assert not errors, errors
                    assert len(records) == count, (count, records)
                    assert expected in records[-1], (expected, records[-1])

            def enter(text):
                # Let the previous synthetic response finish before entering a new turn.
                threading.Event().wait(0.5)
                # Retained frames are diffs: a word can be split across cursor moves, so do
                # not require its contiguous appearance before Enter. Wire assertions below
                # check the exact received text instead.
                terminal.send(text)
                threading.Event().wait(0.35)
                terminal.send(b"\r")

            def begin(label, count):
                nonlocal stage
                stage = label
                offset = checkpoint()
                enter(label)
                wait_new("Fixture credential choice?", offset)
                wait_new("Space check", offset)
                request_count(count, label)
                threading.Event().wait(0.3)

            def done(message):
                passes.append(message)
                print("PASS " + message, flush=True)

            terminal.wait("❯")
            threading.Event().wait(0.6)
            begin("ASK_FIXTURE_CUSTOM", 1)
            stage = "activate custom and type first character"
            for _ in range(3):
                key(b"\x1b[B")
            key(b"\r", "type/paste answer")
            key("z", "✎ z")
            done("first custom character repaints with retained renderer active")
            stage = "backspace and leave empty custom buffer"
            key(b"\x7f")
            key(b"\x7f", "Space check")
            key(b"\r", "type/paste answer")
            key("x", "x")
            key(b"\x7f")
            key("custom-á", "á")
            done("custom Backspace, empty-buffer return and BMP Unicode remain responsive")
            stage = "escape and re-enter custom buffer"
            key(b"\x1b", "Space check")
            key(b"\r", "type/paste answer")
            key("!", "!")
            stage = "switch questions and validate incomplete answer"
            key(b"\t", "Fixture checks?")
            key(b"\r", "Answer this question")
            assert len(records) == 1, records
            key("2", "[x]")
            stage = "submit mixed answers"
            offset = checkpoint()
            key(b"\r")
            request_count(2, "Fixture credential choice? → custom-á!\nFixture checks? → Test")
            done("two-question validation preserves custom text and submits mixed answers once")
            stage = "ordinary input after custom submission"
            enter("AFTER_CUSTOM")
            request_count(3, "AFTER_CUSTOM")
            done("normal chat remains usable after custom answer submission")

            begin("ASK_FIXTURE_OPTIONS", 4)
            stage = "ordinary options"
            key(b"\x1b[B")
            key(b"\t", "Fixture checks?")
            key("1", "[x]")
            key(b"\r")
            request_count(5, "Fixture credential choice? → Account\nFixture checks? → Format")
            done("ordinary option selection still submits both questions exactly once")

            begin("ASK_FIXTURE_ESCAPE", 6)
            stage = "escape from empty custom then dismiss"
            for _ in range(3):
                key(b"\x1b[B")
            key(b"\r", "type/paste answer")
            key(b"\x1b", "Space check")
            key(b"\x1b")
            enter("AFTER_ESCAPE")
            request_count(7, "AFTER_ESCAPE")
            done("Esc exits custom editing and dismisses the panel without trapping normal input")
            threading.Event().wait(0.4)
            assert len(records) == 7 and not errors, (records, errors)
            result = {"ok": True, "binary": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                      "checks": passes, "requests": len(records), "limitations": [
                          "ConPTY UTF-8 input is not a physical Vietnamese IME test.",
                          "Only the repository-local artifact and synthetic loopback provider were exercised.",
                          "The executable that produced the user's screenshot remains unidentified."]}
            (evidence / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
            print(json.dumps(result, indent=2), flush=True)
    finally:
        (evidence / "requests.json").write_text(json.dumps({"stage": stage, "requests": records, "errors": errors}, indent=2), encoding="utf-8")
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    parser.add_argument("binary", type=Path)
    parser.add_argument("--console-child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    if args.console_child:
        fixture.console_child(args.binary.resolve())
    elif args.worker:
        run(args.binary.resolve(), args.evidence.resolve())
    else:
        # A hard outer deadline also bounds blocked ConPTY writes/cleanup, not only polling.
        evidence = args.evidence or Path(tempfile.mkdtemp(prefix="aizen-ask-evidence-"))
        print(f"EVIDENCE {evidence}", flush=True)
        child = subprocess.Popen([sys.executable, "-I", __file__, str(args.binary.resolve()),
                                  "--worker", "--evidence", str(evidence.resolve())])
        try:
            raise SystemExit(child.wait(timeout=150))
        except subprocess.TimeoutExpired:
            subprocess.run(["taskkill", "/PID", str(child.pid), "/T", "/F"], capture_output=True, timeout=15)
            raise SystemExit("FAIL outer 150s deadline; fixture-owned process tree terminated")
