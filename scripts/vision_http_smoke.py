"""Drive the actual CLI against a loopback-only scripted vision provider.

No external model is called. This verifies HTTP bodies and session transitions,
not image comprehension. Uses only the Python standard library.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import zlib


def png_fixture():
    def chunk(kind, value):
        return struct.pack(">I", len(value)) + kind + value + struct.pack(">I", zlib.crc32(kind + value))
    row = bytes([255, 255, 255, 255, 0, 0, 0, 255, 40, 100, 180, 255, 255, 255, 255, 255])
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 4, 2, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress((b"\0" + row) * 2)) + chunk(b"IEND", b"")


def photo_fixture(width=97, height=83, noise=True):
    def chunk(kind, value):
        return struct.pack(">I", len(value)) + kind + value + struct.pack(">I", zlib.crc32(kind + value))
    seed, pixels = 0x958AB147, bytearray()
    for index in range(width * height):
        if noise:
            seed ^= (seed << 13) & 0xFFFFFFFF
            seed ^= seed >> 17
            seed ^= (seed << 5) & 0xFFFFFFFF
            pixels.extend((seed & 255, (seed >> 8) & 255, (seed >> 16) & 255))
        else:
            value = 24 if index % width % 64 < 3 or index // width % 40 < 2 else 230
            pixels.extend((value, value, value))
    raster = b"".join(b"\0" + pixels[y * width * 3:(y + 1) * width * 3] for y in range(height))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raster)) + chunk(b"IEND", b"")


def decode_rgba_png(url):
    raw = base64.b64decode(url.split(",", 1)[1], validate=True)
    assert raw.startswith(b"\x89PNG\r\n\x1a\n")
    width, height, depth, color, compression, filtering, interlace = struct.unpack(">IIBBBBB", raw[16:29])
    assert (depth, color, compression, filtering, interlace) == (8, 6, 0, 0, 0)
    offset, compressed = 8, bytearray()
    while offset < len(raw):
        length = struct.unpack(">I", raw[offset:offset + 4])[0]
        kind, value = raw[offset + 4:offset + 8], raw[offset + 8:offset + 8 + length]
        if kind == b"IDAT":
            compressed.extend(value)
        offset += length + 12
    raster, stride, pixels, previous = zlib.decompress(compressed), width * 4, bytearray(), bytes(width * 4)
    assert len(raster) == (stride + 1) * height
    for y in range(height):
        start = y * (stride + 1)
        mode, row = raster[start], bytearray(raster[start + 1:start + stride + 1])
        for x in range(stride):
            left = row[x - 4] if x >= 4 else 0
            above, upper_left = previous[x], previous[x - 4] if x >= 4 else 0
            if mode == 0:
                predictor = 0
            elif mode == 1:
                predictor = left
            elif mode == 2:
                predictor = above
            elif mode == 3:
                predictor = (left + above) // 2
            else:
                assert mode == 4
                p = left + above - upper_left
                distances = (abs(p - left), abs(p - above), abs(p - upper_left))
                predictor = (left, above, upper_left)[distances.index(min(distances))]
            row[x] = (row[x] + predictor) & 255
        pixels.extend(row)
        previous = row
    return width, height, bytes(pixels)


def image_urls(value):
    if isinstance(value, dict):
        if value.get("type") == "image_url":
            yield value["image_url"]["url"]
        for child in value.values():
            yield from image_urls(child)
    elif isinstance(value, list):
        for child in value:
            yield from image_urls(child)


def tool_result(body, name):
    calls = {}
    for message in body.get("messages", []):
        for call in message.get("tool_calls", []):
            calls[call["id"]] = call["function"]["name"]
    for message in reversed(body.get("messages", [])):
        if message.get("role") == "tool" and calls.get(message.get("tool_call_id")) == name:
            content = message.get("content", "")
            try:
                result = json.loads(content)
                if isinstance(result, list) and result:
                    return result
            except (ValueError, TypeError):
                pass
    return None


def percentile(values, percent):
    ordered = sorted(values)
    return ordered[max(0, (len(ordered) * percent + 99) // 100 - 1)]


def peak_working_set(pid):
    if os.name != "nt":
        return None
    import ctypes
    from ctypes import wintypes

    class Counters(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
            (name, ctypes.c_size_t) for name in (
                "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
                "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage", "QuotaNonPagedPoolUsage",
                "PagefileUsage", "PeakPagefileUsage",
            )
        ]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
    psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    handle = kernel.OpenProcess(0x0400 | 0x0010, False, pid)
    if not handle:
        return None
    try:
        counters = Counters()
        counters.cb = ctypes.sizeof(counters)
        if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            return counters.PeakWorkingSetSize
        return None
    finally:
        kernel.CloseHandle(handle)


class Recorder:
    def __init__(self, original):
        self.original = original
        self.phase = "inspect"
        self.step = 0
        self.records = []
        self.errors = []
        self.image_id = None
        self.reference_id = None
        self.reread_id = None
        self.photo_original = None
        self.handoff_parent_step = 0
        self.handoff_children = []
        self.lock = threading.Lock()
        self.call_started = None
        self.performance_requests = 0
        self.performance_mode = None

    def reply(self, body, raw):
        with self.lock:
            urls = list(image_urls(body))
            step = self.step
            self.step += 1
            record = {"phase": self.phase, "step": step, "json_bytes": len(raw), "images": len(urls), "image_uri_bytes": sum(map(len, urls))}
            if self.call_started is not None:
                record["launch_to_request_ms"] = (time.perf_counter() - self.call_started) * 1000
                record["process_peak_working_set_bytes"] = peak_working_set(self.current_pid)
            self.records.append(record)
            for url in urls:
                assert url.startswith("data:image/"), "unexpected remote image"
            if self.phase.startswith("performance-"):
                if self.performance_mode == "release":
                    if step < 2:
                        assert urls == [self.photo_original]
                    else:
                        assert not urls, "large dormant fixture was retransmitted"
                    if step == 0:
                        return "image_context", {"action": "list"}
                    if step == 1:
                        active = [entry for entry in tool_result(body, "image_context") if entry["state"] == "active"]
                        assert len(active) == 1 and active[0]["delivered"] and active[0]["reference_id"]
                        return "image_context", {"action": "remember", "id": active[0]["id"],
                                                  "note": "Synthetic fixture delivery measured; no semantic verification.", "release_pixels": True}
                else:
                    assert len(urls) == 1
                    if self.performance_mode in ("auto", "original"):
                        assert urls == [self.photo_original], "pixel-sensitive performance fixture was transformed"
                    else:
                        assert len(urls[0]) <= len(self.photo_original), "chosen derivative grew"
                if step + 1 < self.performance_requests:
                    return "file_read", {"path": "fixture.txt", "start": 1, "end": 1}
                return None, "Loopback performance fixture complete; no semantic quality assertion."
            if self.phase in ("task-handoff", "workflow-handoff"):
                user_text = "\n".join(
                    message.get("content", "") if isinstance(message.get("content"), str)
                    else "\n".join(part.get("text", "") for part in message.get("content", []) if part.get("type") == "text")
                    for message in body.get("messages", []) if message.get("role") == "user")
                if not body.get("stream"):
                    # Synthesis repeats child reports/markers, but is not another child run.
                    assert self.phase == "workflow-handoff" and not urls
                    assert set(self.handoff_children) == {"VISION_CHILD_SELECTED", "VISION_CHILD_TEXT_ONLY"}
                    return None, "Scoped workflow reports merged; no image disclosure or verification claim."
                child = next((marker for marker in ("VISION_CHILD_SELECTED", "VISION_CHILD_TEXT_ONLY") if marker in user_text), None)
                if child:
                    # The child's brief excludes the parent's history. A selector narrows a live
                    # grant; the text-only sibling must get neither pixels nor reference handles.
                    assert "VISION_PARENT_PRIVATE" not in user_text
                    if child == "VISION_CHILD_SELECTED":
                        assert urls == [self.original], "selected child did not get exactly its granted original"
                    else:
                        assert not urls, "text-only sibling inherited parent pixels"
                        assert "<selected_references>" not in user_text
                    self.handoff_children.append(child)
                    return None, child + " completed the scoped fixture inspection; no acceptance claim."
                if "VISION_PARENT_PRIVATE" not in user_text:
                    # A workflow's final report synthesis is text-only in this fixture.
                    assert self.phase == "workflow-handoff" and not urls
                    return None, "Scoped workflow reports merged; no image disclosure or verification claim."
                parent_step = self.handoff_parent_step
                self.handoff_parent_step += 1
                assert urls == [self.original], "parent working set changed during child dispatch"
                if parent_step == 0:
                    return "image_context", {"action": "list"}
                if parent_step == 1:
                    entries = tool_result(body, "image_context")
                    assert entries and len(entries) == 1 and entries[0]["reference_id"]
                    ref = entries[0]["reference_id"]
                    brief = "Inspect only the explicitly selected fixture attachment. Do not edit or execute commands. Return a short observation, no acceptance or implementation claim. "
                    if self.phase == "task-handoff":
                        return "task", {"role": "argus", "prompt": brief + "VISION_CHILD_SELECTED", "reference_ids": [ref], "max_steps": 3}
                    return "workflow", {"mode": "fanout", "tasks": [
                        {"id": "selected", "role": "argus", "prompt": brief + "VISION_CHILD_SELECTED", "reference_ids": [ref], "max_steps": 3},
                        {"id": "text-only", "role": "argus", "prompt": "Return only a text-only acknowledgement. Do not inspect files or images, edit, execute commands or claim visual verification. VISION_CHILD_TEXT_ONLY", "max_steps": 3},
                    ]}
                assert "VISION_CHILD_SELECTED" in self.handoff_children, "selected child never reached HTTP"
                if self.phase == "workflow-handoff":
                    assert "VISION_CHILD_TEXT_ONLY" in self.handoff_children, "text-only sibling never reached HTTP"
                return None, "Scoped child handoff completed; no inherited image acceptance evidence."
            if self.phase == "default-reread":
                if step == 0:
                    assert urls == [self.original]
                    return "image_context", {"action": "list"}
                if step == 1:
                    entries = tool_result(body, "image_context")
                    assert len(entries) == 1 and entries[0]["reference_id"]
                    return "reference_read", {"ref_id": entries[0]["reference_id"], "transmit_image": True}
                assert step == 2 and urls == [self.original, self.original], "approved tool reread missing or altered"
                return None, "Approved original reread received; no semantic verification."
            if self.phase.startswith("policy-"):
                assert b"image_preferences" not in raw, "storage preferences leaked into wire schema"
                expected = 2 if self.phase == "policy-mixed" else 1
                assert len(urls) == expected
                if self.phase in ("policy-photo", "policy-mixed"):
                    assert urls[-1].startswith("data:image/jpeg;base64,")
                    assert len(urls[-1]) < len(self.photo_original), "photo preview did not reduce URI bytes"
                    jpeg = base64.b64decode(urls[-1].split(",", 1)[1], validate=True)
                    assert jpeg.startswith(b"\xff\xd8") and jpeg.endswith(b"\xff\xd9")
                    if expected == 2:
                        assert urls[0] == self.photo_original, "Auto text/unknown selection was transformed"
                else:
                    assert urls == [self.photo_original], "Original/Auto/Exact policy changed pixels"
                return None, "Per-image transport fixture completed; no semantic quality assertion."
            if self.phase == "inspect":
                if step == 0:
                    assert urls == [self.original], "first request must preserve original pixels"
                    return "image_context", {"action": "list"}
                if step == 1:
                    entries = tool_result(body, "image_context")
                    assert entries and entries[0]["delivered"], "successful delivery not recorded"
                    self.image_id = entries[0]["id"]
                    self.reference_id = entries[0]["reference_id"]
                    assert self.reference_id, "ordinary supported image needs live reread handle"
                    return "image_context", {"action": "remember", "id": self.image_id, "note": "Fixture has two rows; black and blue marks on white.", "uncertainty": "Scripted observation, not semantic verification.", "release_pixels": True}
                if 2 <= step <= 4:
                    assert not urls, "released pixels were retransmitted during code steps"
                    return "file_read", {"path": "fixture.txt", "start": 1, "end": 1}
                if step == 5:
                    assert not urls, "dormant original reactivated before ROI reread"
                    return "reference_read", {"ref_id": self.reference_id, "transmit_image": True, "crop": {"x": 1, "y": 0, "width": 2, "height": 2}}
                if step == 6:
                    assert len(urls) == 1 and urls[0] != self.original, "ROI reread must not send the dormant whole image"
                    width, height, pixels = decode_rgba_png(urls[0])
                    assert (width, height) == (2, 2)
                    assert pixels == bytes([0, 0, 0, 255, 40, 100, 180, 255]) * 2, "ROI did not map to original pixels"
                    return "reference_read", {"ref_id": self.reference_id, "transmit_image": True}
                if step == 7:
                    assert self.original in urls, "explicit original reread missing"
                    assert len(urls) == 2, "original/ROI occurrences must stay distinct"
                    return "image_context", {"action": "list"}
                if step in (8, 9):
                    entries = tool_result(body, "image_context")
                    active = [entry for entry in entries if entry["state"] == "active"]
                    assert active, "reread occurrence is not lifecycle-managed"
                    entry = active[-1]
                    assert entry["kind"] == "tool" and entry["delivered"] and entry["reference_id"]
                    self.reread_id = entry["id"]
                    return "image_context", {"action": "remember", "id": self.reread_id, "note": "Explicit fixture reread inspected; no semantic verification.", "release_pixels": True}
                assert not urls, "released original/ROI tool occurrences were retransmitted"
            elif self.phase == "resume":
                assert not urls, "resume renewed dormant pixels without selection"
                assert any(tool["function"]["name"] == "image_context" for tool in body.get("tools", [])), "saved visual descriptors need lazy lifecycle tool"
            elif self.phase in ("reselect", "reselect-capture"):
                if step < 2:
                    assert urls == [self.original], "reselect must send only the new original occurrence"
                else:
                    assert not urls, "reselected occurrence did not release independently"
                if step == 0:
                    return "image_context", {"action": "list"}
                if step == 1:
                    entries = tool_result(body, "image_context")
                    active = [entry for entry in entries if entry["state"] == "active"]
                    assert len(active) == 1 and active[0]["id"] != self.image_id
                    assert active[0]["kind"] == "attachment", "saved capture authority was copied instead of creating a fresh attachment"
                    assert active[0]["reference_id"] and active[0]["delivered"]
                    return "image_context", {"action": "remember", "id": active[0]["id"], "note": "Explicitly reselected owned fixture; no acceptance credit.", "release_pixels": True}
            elif self.phase == "summary":
                assert not urls, "summarizer unexpectedly received pixels"
            elif self.phase == "postcompact":
                assert not urls, "compacted dormant original reactivated implicitly"
            return None, "Loopback vision smoke completed. No semantic model was used."


def run(binary, performance_samples=0, performance_requests=4, default_vision_only=False):
    with tempfile.TemporaryDirectory(prefix="aizen-vision-smoke-") as temporary:
        root = Path(temporary)
        home = root / "home"
        home.mkdir()
        workspace = root / "workspace"
        workspace.mkdir()
        (workspace / "fixture.txt").write_text("local fixture only\n", encoding="utf-8")
        fixture = png_fixture()
        image = workspace / "fixture.png"
        image.write_bytes(fixture)
        original = "data:image/png;base64," + base64.b64encode(fixture).decode("ascii")
        recorder = Recorder(original)
        invocations = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= 32 * 1024 * 1024:
                    self.send_error(413)
                    return
                raw = self.rfile.read(size)
                body = json.loads(raw)
                if recorder.phase == "provider-unsupported":
                    urls = list(image_urls(body))
                    assert urls == [original], "unsupported fixture must receive the selected image, not a text-only fallback"
                    recorder.records.append({"phase": recorder.phase, "images": len(urls), "json_bytes": len(raw)})
                    encoded = json.dumps({"error": {"message": "Fixture endpoint does not support image input", "type": "invalid_request_error", "code": "unsupported_image"}}).encode()
                    self.send_response(400)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(encoded)))
                    self.end_headers()
                    self.wfile.write(encoded)
                    return
                try:
                    name, value = recorder.reply(body, raw)
                except Exception as error:
                    recorder.errors.append(str(error))
                    self.send_response(500)
                    self.end_headers()
                    self.wfile.write(str(error).encode())
                    return
                if not body.get("stream"):
                    response = {"id": "loopback", "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": value}}], "usage": {"prompt_tokens": 128, "completion_tokens": 16}}
                    encoded = json.dumps(response).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(encoded)))
                    self.end_headers()
                    self.wfile.write(encoded)
                    return
                if name:
                    delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": f"smoke-{recorder.step}", "type": "function", "function": {"name": name, "arguments": json.dumps(value)}}]}
                    finish = "tool_calls"
                else:
                    delta = {"role": "assistant", "content": value}
                    finish = "stop"
                events = [
                    {"id": "loopback", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                    {"id": "loopback", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": {"prompt_tokens": 128, "completion_tokens": 16}},
                ]
                encoded = b"".join(b"data: " + json.dumps(event).encode() + b"\n\n" for event in events) + b"data: [DONE]\n\n"
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{server.server_port}/v1"
        config_path = home / "cli-config.json"
        config_path.write_text(json.dumps({
            "architect_mode": False, "self_review": False, "eager_tools": False,
        }), encoding="utf-8")
        initial_config = config_path.read_bytes()
        env = {key: value for key, value in os.environ.items()
               if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_"))
               and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")}
        env.update({"AIZEN_HOME": str(home), "AIZEN_ARCHITECT": "0", "AIZEN_IMAGE_SEND_MODE": "original"})

        def invoke(arguments, machine=False, expected_code=0):
            started = time.perf_counter()
            recorder.call_started = started
            proc = subprocess.Popen([str(binary), *arguments], cwd=workspace, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
            recorder.current_pid = proc.pid
            first_event_ms = first_text_ms = None
            stderr = []
            def drain():
                stderr.append(proc.stderr.read())
            thread = threading.Thread(target=drain, daemon=True)
            thread.start()
            timer = threading.Timer(90, proc.kill)
            timer.start()
            lines = []
            try:
                for line in proc.stdout:
                    lines.append(line)
                    if machine:
                        try:
                            event = json.loads(line)
                        except ValueError:
                            continue
                        elapsed = (time.perf_counter() - started) * 1000
                        if event.get("type") == "stream_event":
                            if first_event_ms is None:
                                first_event_ms = elapsed
                            delta = event.get("event", {}).get("delta", {})
                            if first_text_ms is None and delta.get("type") == "text_delta" and delta.get("text"):
                                first_text_ms = elapsed
                        if event.get("type") == "control_request":
                            response = {"type": "control_response", "response": {"subtype": "success", "request_id": event["request_id"], "response": {"behavior": "allow"}}}
                            proc.stdin.write(json.dumps(response) + "\n")
                            proc.stdin.flush()
                code = proc.wait(timeout=5)
                thread.join(timeout=5)
                assert code == expected_code, f"CLI exit {code}, expected {expected_code}; provider assertions: {recorder.errors}: {''.join(stderr)[-3000:]} {''.join(lines)[-3000:]}"
                if machine:
                    invocations.append({
                        "phase": recorder.phase,
                        "launch_to_first_stream_event_ms": first_event_ms,
                        "launch_to_first_text_ms": first_text_ms,
                        "wall_ms": (time.perf_counter() - started) * 1000,
                    })
                return [json.loads(line) for line in lines if line.strip()] if machine else "".join(lines)
            finally:
                timer.cancel()
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()

        def agent(task, extra=(), expected_code=0):
            return invoke(["agent", task, "--base-url", base, "--api-key", "loopback-placeholder", "--model", "vision-fixture", "--output-format", "stream-json", "--max-iters", "12", "--yes", "--save-session", *extra], True, expected_code)

        try:
            if default_vision_only:
                recorder.phase, recorder.step = "policy-exact", 0
                recorder.photo_original = original
                events = agent("Inspect only this selected fixture; no verification claim.",
                               ["--image", str(image), "--reference-images"])
                # Exact comparison still requires current implementation evidence. Receiving the
                # original proves transport only; this fixture must not claim visual verification.
                result = [event for event in events if event.get("type") == "result"][-1]
                assert result.get("is_error") and result.get("stop") == "verification_failed", result
                assert result["media"]["attempts"] == 1 and result["media"]["images"] == 1, result
                assert result["verification"]["visual"]["status"] == "unavailable", result
                assert recorder.step == 1, "unexpected capability probe or extra request"
                recorder.phase, recorder.step = "default-reread", 0
                events = agent("Inspect only the selected original and its approved reread; no verification claim.",
                               ["--image", str(image)])
                result = [event for event in events if event.get("type") == "result"][-1]
                assert not result.get("is_error"), result
                assert recorder.step == 3
                for phase in ("task-handoff", "workflow-handoff"):
                    recorder.phase, recorder.step = phase, 0
                    recorder.handoff_parent_step, recorder.handoff_children = 0, []
                    events = agent("VISION_PARENT_PRIVATE: inspect the selected attachment with scoped delegation; do not edit files or claim acceptance.",
                                   ["--image", str(image)])
                    assert not [event for event in events if event.get("type") == "result"][-1].get("is_error")
                    assert recorder.handoff_children.count("VISION_CHILD_SELECTED") == 1
                    if phase == "workflow-handoff":
                        assert recorder.handoff_children.count("VISION_CHILD_TEXT_ONLY") == 1
                assert not recorder.errors, recorder.errors
                assert config_path.read_bytes() == initial_config, "default vision wrote user config"
                recorder.phase = "provider-unsupported"
                before_rejection = len(recorder.records)
                rejected = agent("Inspect only this image; report a provider error honestly.", ["--image", str(image)], expected_code=1)
                rejected_result = [event for event in rejected if event.get("type") == "result"][-1]
                assert rejected_result.get("is_error"), "provider rejection was hidden"
                assert "HTTP 400 code=unsupported_image" in " ".join(rejected_result.get("errors", [])), rejected_result
                assert len(recorder.records) == before_rejection + 1, "provider rejection triggered a fallback or retry"
                assert config_path.read_bytes() == initial_config
                disabled = json.loads(initial_config)
                disabled["vision_capabilities"] = [{"base_url": base, "model": "vision-fixture", "supported": False}]
                config_path.write_text(json.dumps(disabled), encoding="utf-8")
                before = len(recorder.records)
                blocked = subprocess.run([str(binary), "agent", "Inspect the selected reference.",
                    "--base-url", base, "--api-key", "loopback-placeholder", "--model", "vision-fixture",
                    "--output-format", "stream-json", "--reference-images", "--image", str(image)],
                    cwd=workspace, env=env, stdin=subprocess.DEVNULL, capture_output=True,
                    text=True, encoding="utf-8", errors="replace", timeout=20)
                assert blocked.returncode != 0, "explicit disable was ignored"
                assert "configured without vision" in blocked.stderr + blocked.stdout
                assert len(recorder.records) == before, "disabled reference reached the provider"
                print(json.dumps({"status": "passed", "scope": "actual CLI / loopback only; no vision config, exact attachment, approved reread, scoped task/workflow child and text-only sibling, config unchanged, explicit disable; no semantic quality claim",
                                  "requests": recorder.records}, indent=2))
                return
            if performance_samples:
                import hashlib

                def distribution(values):
                    return {"samples": len(values), "p50": percentile(values, 50), "p95": percentile(values, 95)}

                startup = []
                for index in range(23):
                    started = time.perf_counter()
                    completed = subprocess.run([str(binary), "--version"], cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                                               capture_output=True, text=True, timeout=15, check=True)
                    version = completed.stdout.strip()
                    if index >= 3:
                        startup.append((time.perf_counter() - started) * 1000)
                cases = []
                for fixture_name, noise in (("grid", False), ("noise", True)):
                    source = photo_fixture(1536, 1024, noise)
                    source_path = workspace / f"{fixture_name}.png"
                    source_path.write_bytes(source)
                    recorder.photo_original = "data:image/png;base64," + base64.b64encode(source).decode("ascii")
                    for mode in ("auto", "original", "photo", "release"):
                        recorder.phase = f"performance-{fixture_name}-{mode}"
                        recorder.performance_mode = mode
                        recorder.performance_requests = performance_requests
                        cold, warm, first_event, first_text, wall, peaks, json_bytes, uri_bytes, leases = ([] for _ in range(9))
                        for _ in range(performance_samples):
                            recorder.step = 0
                            start = len(recorder.records)
                            env["AIZEN_IMAGE_SEND_MODE"] = "auto"
                            events = agent("Inspect only the synthetic performance fixture. Read fixture.txt as requested; keep its selected pixels active. Do not edit or claim visual verification.",
                                           ["--image", str(source_path), "--image-mode", "auto" if mode == "release" else mode])
                            result = [event for event in events if event.get("type") == "result"][-1]
                            assert not result.get("is_error"), result
                            records = recorder.records[start:]
                            assert len(records) == performance_requests
                            assert result["media"]["attempts"] == len(records)
                            for field in ("json_bytes", "image_uri_bytes", "images"):
                                assert result["media"][field] == sum(row[field] for row in records), f"performance ledger mismatch: {field}"
                            cold.append(records[0]["launch_to_request_ms"])
                            warm.extend(current["launch_to_request_ms"] - previous["launch_to_request_ms"] for previous, current in zip(records, records[1:]))
                            timing = invocations[-1]
                            assert timing["launch_to_first_stream_event_ms"] is not None
                            assert timing["launch_to_first_text_ms"] is not None
                            first_event.append(timing["launch_to_first_stream_event_ms"])
                            first_text.append(timing["launch_to_first_text_ms"])
                            wall.append(timing["wall_ms"])
                            peaks.extend(row["process_peak_working_set_bytes"] for row in records if row["process_peak_working_set_bytes"] is not None)
                            json_bytes.append(sum(row["json_bytes"] for row in records))
                            uri_bytes.append(sum(row["image_uri_bytes"] for row in records))
                            leases.append(result["media"]["peak_reserved_bytes"])
                        cases.append({
                            "fixture": fixture_name, "mode": mode,
                            "source_encoded_bytes": len(source), "source_uri_bytes": len(recorder.photo_original),
                            "requests_per_process": performance_requests,
                            "cold_launch_to_request_ms": distribution(cold),
                            "warm_request_interval_ms": distribution(warm),
                            "launch_to_first_stream_event_ms": distribution(first_event),
                            "launch_to_first_text_ms": distribution(first_text),
                            "process_wall_ms": distribution(wall),
                            "cumulative_json_bytes": distribution(json_bytes),
                            "cumulative_image_uri_bytes": distribution(uri_bytes),
                            "request_media_lease_peak_bytes": max(leases),
                            "process_peak_working_set_bytes": max(peaks) if peaks else None,
                        })
                print(json.dumps({
                    "status": "passed", "scope": "actual CLI / immediate loopback responses; synthetic fixtures; no provider TTFT, semantic quality or pure codec-time claim",
                    "binary": {"version": version, "bytes": binary.stat().st_size, "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()},
                    "startup_launch_version_wait_ms": distribution(startup),
                    "notes": ["cold includes process startup, intake, preparation, JSON and loopback receive",
                              "warm interval includes previous response, tool execution, request preparation and loopback receive; same-process cache is eligible",
                              "first stream event may be tool arguments; first text arrives only at the scripted final response",
                              "working-set peak sampled at request receipt includes the whole process; media lease is not process RSS",
                              "provider usage is synthetic and is not a billing or image-token measurement"],
                    "cases": cases,
                }, indent=2))
                return
            events = agent("Inspect this fixture then record observations and release pixels; read the local fixture file three times.", ["--image", str(image)])
            results = [event for event in events if event.get("type") == "result"]
            assert results and not results[-1].get("is_error"), results
            slug = results[-1].get("session")
            media = results[-1].get("media")
            inspect = [record for record in recorder.records if record["phase"] == "inspect"]
            assert media and media["attempts"] == len(inspect), "actual-attempt media ledger missing"
            for field in ("json_bytes", "image_uri_bytes", "images"):
                assert media[field] == sum(record[field] for record in inspect), f"media ledger mismatch: {field}"
            assert media["peak_reserved_bytes"] > 0
            assert slug, "session slug missing"
            listing = json.loads(invoke(["images", slug, "--json"]))["images"]
            assert len(listing) == 3 and all(row["state"] == "dormant" and row["available"] for row in listing)
            assert sum(row["kind"] == "attachment" for row in listing) == 1
            assert sum(row["kind"] == "tool" for row in listing) == 2
            assert "base64," not in json.dumps(listing)
            assert recorder.step >= 8, "original/ROI tool sequence did not complete"
            assert not recorder.errors, recorder.errors
            recorder.phase, recorder.step = "resume", 0
            for turn in range(4):
                agent(f"Text-only continuation {turn}, no renewed image selection.", ["--resume", slug])
            recorder.phase, recorder.step = "summary", 0
            compacted = json.loads(invoke(["compact", slug, "--base-url", base, "--api-key", "loopback-placeholder", "--model", "vision-fixture", "--output-format", "json"]))
            assert compacted["slug"] == slug and recorder.step > 0, "actual compact did not call summarizer"
            listing_after = json.loads(invoke(["images", slug, "--json"]))["images"]
            def listing_identity(rows):
                return {row["id"]: row for row in rows}
            assert listing_identity(listing_after) == listing_identity(listing), "compact changed saved original identity/notes/availability"
            recorder.phase, recorder.step = "postcompact", 0
            agent("Continue after compaction without image reselection.", ["--resume", slug])
            recorder.phase, recorder.step = "reselect", 0
            attachment = next(row for row in listing_after if row["kind"] == "attachment")
            agent("Inspect the explicitly reselected saved image.", ["--resume", slug, "--image-id", attachment["id"]])
            recorder.phase, recorder.step = "reselect-capture", 0
            # Select the uncropped original reread, not the independently retained 2x2 ROI.
            capture = next(row for row in listing_after if row["kind"] == "tool" and (row["width"], row["height"]) == (4, 2))
            agent("Inspect only this explicitly reselected saved capture; grant no acceptance credit.", ["--resume", slug, "--image-id", capture["id"]])
            assert not recorder.errors, recorder.errors
            assert {record["phase"] for record in recorder.records} == {"inspect", "resume", "summary", "postcompact", "reselect", "reselect-capture"}
            photo = photo_fixture()
            photo_path = workspace / "compressible.png"
            photo_path.write_bytes(photo)
            recorder.photo_original = "data:image/png;base64," + base64.b64encode(photo).decode("ascii")
            for phase, mode, extra in [
                ("policy-auto", "auto", []),
                ("policy-photo", "photo", []),
                ("policy-original", "original", []),
                ("policy-exact", "photo", ["--reference-images"]),
                ("policy-rollback", "photo", []),
                ("policy-mixed", "auto", ["--image", str(photo_path), "--image-mode", "photo"]),
            ]:
                recorder.phase, recorder.step = phase, 0
                env["AIZEN_IMAGE_SEND_MODE"] = "original" if phase == "policy-rollback" else "auto"
                agent("Inspect only the selected fixture; report no implementation or verification claim.",
                      ["--image", str(photo_path), "--image-mode", mode, *extra])
                assert recorder.step == 1, f"unexpected extra request in {phase}"
            for phase in ("task-handoff", "workflow-handoff"):
                recorder.phase, recorder.step = phase, 0
                recorder.handoff_parent_step, recorder.handoff_children = 0, []
                env["AIZEN_IMAGE_SEND_MODE"] = "original"
                start = len(recorder.records)
                handoff = agent("VISION_PARENT_PRIVATE: inspect the selected attachment with scoped delegation; do not edit files or claim acceptance.", ["--image", str(image)])
                result = [event for event in handoff if event.get("type") == "result"][-1]
                assert not result.get("is_error"), result
                records = recorder.records[start:]
                assert result["media"]["attempts"] == len(records), f"child sends reset or escaped parent media accounting: {phase}: ledger={result['media']} recorded={records}"
                for field in ("json_bytes", "image_uri_bytes", "images"):
                    assert result["media"][field] == sum(record[field] for record in records), f"child media mismatch: {field}"
                assert recorder.handoff_children.count("VISION_CHILD_SELECTED") == 1
                if phase == "workflow-handoff":
                    assert recorder.handoff_children.count("VISION_CHILD_TEXT_ONLY") == 1
            assert not recorder.errors, recorder.errors
            print(json.dumps({"status": "passed", "scope": "actual CLI / loopback HTTP; original/ROI, resume, compact, reselect, per-image policies and scoped reference task/workflow handoff; no semantic quality claim", "requests": recorder.records}, indent=2))
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--default-vision-only", action="store_true", help="verify default vision and child isolation without legacy saved-image workflows")
    parser.add_argument("--performance-samples", type=int, default=0, help="run fresh-process synthetic performance cases instead of lifecycle smoke")
    parser.add_argument("--performance-requests", type=int, default=4, help="requests per performance process, including cold and cache-eligible follow-ups")
    args = parser.parse_args()
    if not 0 <= args.performance_samples <= 30 or not 3 <= args.performance_requests <= 10:
        parser.error("performance samples must be 0..30 and requests must be 3..10")
    run(args.binary.resolve(), args.performance_samples, args.performance_requests, args.default_vision_only)
