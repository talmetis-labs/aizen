"""Drive the real Windows CLI through ConPTY with isolated home and loopback HTTP.

Uses only the Python standard library and OS APIs. No user clipboard, installed
app, external model, or user-owned process is touched.
"""
import argparse
import base64
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import zlib


class Coord(ctypes.Structure):
    _fields_ = [("X", ctypes.c_short), ("Y", ctypes.c_short)]


class StartupInfo(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD), ("lpReserved", wintypes.LPWSTR),
        ("lpDesktop", wintypes.LPWSTR), ("lpTitle", wintypes.LPWSTR),
        ("dwX", wintypes.DWORD), ("dwY", wintypes.DWORD),
        ("dwXSize", wintypes.DWORD), ("dwYSize", wintypes.DWORD),
        ("dwXCountChars", wintypes.DWORD), ("dwYCountChars", wintypes.DWORD),
        ("dwFillAttribute", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
        ("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
        ("lpReserved2", ctypes.POINTER(wintypes.BYTE)),
        ("hStdInput", wintypes.HANDLE), ("hStdOutput", wintypes.HANDLE),
        ("hStdError", wintypes.HANDLE),
    ]


class StartupInfoEx(ctypes.Structure):
    _fields_ = [("StartupInfo", StartupInfo), ("lpAttributeList", ctypes.c_void_p)]


class ProcessInfo(ctypes.Structure):
    _fields_ = [("hProcess", wintypes.HANDLE), ("hThread", wintypes.HANDLE),
                ("dwProcessId", wintypes.DWORD), ("dwThreadId", wintypes.DWORD)]


class Terminal:
    def __init__(self, binary, workspace, env, arguments=()):
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        k = self.kernel
        k.CreatePipe.argtypes = [ctypes.POINTER(wintypes.HANDLE), ctypes.POINTER(wintypes.HANDLE), ctypes.c_void_p, wintypes.DWORD]
        k.CreatePipe.restype = wintypes.BOOL
        k.CreatePseudoConsole.argtypes = [Coord, wintypes.HANDLE, wintypes.HANDLE, wintypes.DWORD, ctypes.POINTER(wintypes.HANDLE)]
        k.CreatePseudoConsole.restype = ctypes.c_long
        k.ClosePseudoConsole.argtypes = [wintypes.HANDLE]
        k.InitializeProcThreadAttributeList.argtypes = [ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(ctypes.c_size_t)]
        k.InitializeProcThreadAttributeList.restype = wintypes.BOOL
        k.UpdateProcThreadAttribute.argtypes = [ctypes.c_void_p, wintypes.DWORD, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_void_p]
        k.UpdateProcThreadAttribute.restype = wintypes.BOOL
        k.DeleteProcThreadAttributeList.argtypes = [ctypes.c_void_p]
        k.CreateProcessW.argtypes = [wintypes.LPCWSTR, wintypes.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, wintypes.BOOL, wintypes.DWORD, ctypes.c_void_p, wintypes.LPCWSTR, ctypes.POINTER(StartupInfoEx), ctypes.POINTER(ProcessInfo)]
        k.CreateProcessW.restype = wintypes.BOOL
        k.ReadFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
        k.ReadFile.restype = wintypes.BOOL
        k.WriteFile.argtypes = k.ReadFile.argtypes
        k.WriteFile.restype = wintypes.BOOL
        k.CloseHandle.argtypes = [wintypes.HANDLE]
        k.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        k.WaitForSingleObject.restype = wintypes.DWORD
        k.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]
        self.console = wintypes.HANDLE()
        self.process = ProcessInfo()
        self.input = self.output = None
        self.data = bytearray()
        self.changed = threading.Condition()
        self.write_lock = threading.Lock()
        incoming, outgoing = wintypes.HANDLE(), wintypes.HANDLE()
        input_write, output_read = wintypes.HANDLE(), wintypes.HANDLE()
        self.check(k.CreatePipe(ctypes.byref(incoming), ctypes.byref(input_write), None, 0))
        self.check(k.CreatePipe(ctypes.byref(output_read), ctypes.byref(outgoing), None, 0))
        self.input, self.output = input_write, output_read
        result = k.CreatePseudoConsole(Coord(140, 45), incoming, outgoing, 0, ctypes.byref(self.console))
        k.CloseHandle(incoming)
        k.CloseHandle(outgoing)
        if result < 0:
            raise OSError(f"CreatePseudoConsole failed: {result:#x}")
        size = ctypes.c_size_t()
        k.InitializeProcThreadAttributeList(None, 1, 0, ctypes.byref(size))
        attrs = ctypes.create_string_buffer(size.value)
        self.check(k.InitializeProcThreadAttributeList(attrs, 1, 0, ctypes.byref(size)))
        try:
            # PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE expects the handle value, not its address.
            self.check(k.UpdateProcThreadAttribute(attrs, 0, 0x00020016, self.console, ctypes.sizeof(self.console), None, None))
            startup = StartupInfoEx()
            startup.StartupInfo.cb = ctypes.sizeof(startup)
            startup.lpAttributeList = ctypes.cast(attrs, ctypes.c_void_p)
            # The calling host supplies redirected standard handles. Reopen the ConPTY's
            # console devices in a test-owned shell so the CLI receives real console handles.
            shell = str(Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32" / "cmd.exe")
            child_command = subprocess.list2cmdline([str(binary), *map(str, arguments)])
            command = ctypes.create_unicode_buffer(
                f'{subprocess.list2cmdline([shell])} /d /s /c "chcp 65001 >nul & {child_command} <CONIN$ >CONOUT$ 2>&1"'
            )
            environment = ctypes.create_unicode_buffer("\0".join(f"{key}={value}" for key, value in sorted(env.items(), key=lambda item: item[0].upper())) + "\0\0")
            self.check(k.CreateProcessW(shell, command, None, None, False,
                                       0x00080000 | 0x00000400, environment,
                                       str(workspace), ctypes.byref(startup), ctypes.byref(self.process)))
        finally:
            k.DeleteProcThreadAttributeList(attrs)
        k.CloseHandle(self.process.hThread)
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    @staticmethod
    def check(result):
        if not result:
            raise ctypes.WinError(ctypes.get_last_error())

    def read(self):
        buffer = ctypes.create_string_buffer(65536)
        count = wintypes.DWORD()
        while self.kernel.ReadFile(self.output, buffer, len(buffer), ctypes.byref(count), None) and count.value:
            chunk = buffer.raw[:count.value]
            with self.changed:
                self.data.extend(chunk)
                query = b"\x1b[6n" in self.data[-(len(chunk) + 4):]
                self.changed.notify_all()
            if query:
                self.send(b"\x1b[1;1R")

    def send(self, data):
        if isinstance(data, str):
            data = data.encode("utf-8")
        count = wintypes.DWORD()
        with self.write_lock:
            self.check(self.kernel.WriteFile(self.input, data, len(data), ctypes.byref(count), None))
        assert count.value == len(data)

    def text(self):
        with self.changed:
            text = self.data.decode("utf-8", errors="replace")
        text = re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", "", text)
        return re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)

    def wait(self, marker, timeout=25):
        deadline = time.monotonic() + timeout
        with self.changed:
            while marker not in self.text():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(f"terminal marker missing: {marker!r}; tail: {ascii(self.text()[-2000:])}; raw: {bytes(self.data[-500:])!r}")
                self.changed.wait(min(remaining, 0.25))

    def enter(self, text, draft_marker):
        # Windows crossterm reads console KEY_EVENT records, not bracketed-paste escapes.
        # ConPTY translates ordinary UTF-8 input into those records for both surfaces.
        self.send(text)
        self.wait(draft_marker)
        # Enter must be a deliberate key, not part of the input thread's paste burst.
        threading.Event().wait(0.2)
        self.send(b"\r")

    def close(self):
        # Only the shell and CLI tree created by this fixture may be terminated.
        if self.kernel.WaitForSingleObject(self.process.hProcess, 0) == 0x102:
            killed = subprocess.run(["taskkill", "/PID", str(self.process.dwProcessId), "/T", "/F"],
                                    capture_output=True, timeout=15)
            exited = self.kernel.WaitForSingleObject(self.process.hProcess, 5000) == 0
            if killed.returncode and not exited:
                killed.check_returncode()
        self.kernel.ClosePseudoConsole(self.console)
        self.reader.join(timeout=5)
        self.kernel.CloseHandle(self.input)
        self.kernel.CloseHandle(self.output)
        self.kernel.CloseHandle(self.process.hProcess)


def fixture_png(rgba=(255, 0, 0, 255)):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"\0" + bytes(rgba))) + chunk(b"IEND", b""))


def image_urls(value):
    if isinstance(value, dict):
        if value.get("type") == "image_url":
            yield value["image_url"]["url"]
        for child in value.values():
            yield from image_urls(child)
    elif isinstance(value, list):
        for child in value:
            yield from image_urls(child)


def run(binary, surface="all"):
    if os.name != "nt":
        raise SystemExit("This harness requires Windows ConPTY.")
    with tempfile.TemporaryDirectory(prefix="aizen-vision-tui-") as directory:
        root = Path(directory)
        records, errors = [], []
        received = threading.Event()
        release_response = threading.Event()
        fixtures = [fixture_png(color) for color in (
            (255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255),
            (255, 255, 0, 255), (255, 0, 255, 255),
        )]
        originals = ["data:image/png;base64," + base64.b64encode(image).decode("ascii")
                     for image in fixtures]

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                encoded = json.dumps({"data": [{"id": "vision-fixture"}]}).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

            def do_POST(self):
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= 32 * 1024 * 1024:
                    self.send_error(413)
                    return
                body = json.loads(self.rfile.read(size))
                urls = list(image_urls(body))
                try:
                    assert urls == originals[:4], "selected four originals did not reach the wire intact"
                    user_text = "\n".join(
                        str(part.get("text", ""))
                        for message in body.get("messages", []) if message.get("role") == "user"
                        for part in (message.get("content") if isinstance(message.get("content"), list)
                                     else [{"text": message.get("content", "")}])
                    )
                    assert "inspect" in user_text, "refused draft lost its accompanying text"
                    assert "fixture-" not in user_text, "selected image paths leaked into prose"
                    records.append({"images": len(urls), "image_uri_bytes": sum(map(len, urls)), "json_bytes": size})
                except AssertionError as error:
                    errors.append(str(error))
                received.set()
                release_response.wait(35)
                encoded = b'data: {"choices":[{"index":0,"delta":{"content":"Loopback TUI fixture complete."},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                try:
                    self.wfile.write(encoded)
                except (ConnectionError, OSError):
                    pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{server.server_port}/v1"
        results = []
        try:
            surfaces = (False, True) if surface == "all" else (surface == "retained",)
            for retained in surfaces:
                records.clear()
                received.clear()
                release_response.clear()
                home, workspace = root / ("retained-home" if retained else "plain-home"), root / ("retained-workspace" if retained else "plain-workspace")
                home.mkdir()
                workspace.mkdir()
                images = []
                for index, payload in enumerate(fixtures):
                    image = workspace / f"fixture-{index}-with space.png"
                    image.write_bytes(payload)
                    images.append(image)
                config_path = home / "cli-config.json"
                config_path.write_text(json.dumps({
                    "base_url": base, "api_key": "loopback-placeholder", "model": "vision-fixture",
                    "onboarded": True, "approval_mode": "yolo", "auto_effort": False,
                    "architect_mode": False, "self_review": False, "auto_skill_learn": False,
                    "persona_evolve": False, "update_check": False, "eager_tools": False,
                }), encoding="utf-8")
                env = {key: value for key, value in os.environ.items()
                       if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_"))
                       and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")}
                env.update({"AIZEN_HOME": str(home), "AIZEN_NO_UPDATE_CHECK": "1", "AIZEN_NO_WARMUP": "1",
                            "AIZEN_NO_TIPS": "1", "AIZEN_NO_HOOKS": "1", "AIZEN_IMAGE_SEND_MODE": "original"})
                if not retained:
                    env["AIZEN_NO_STICKY"] = "1"
                terminal = Terminal(Path(sys.executable), workspace, env,
                                    ["-I", __file__, "--console-child", binary])
                try:
                    terminal.wait("❯")
                    # The first rendered prompt can precede the keyboard thread's raw-mode setup.
                    threading.Event().wait(0.5)
                    quoted = [f'"{image.name}"' for image in images]
                    terminal.enter("inspect " + " ".join(quoted), "space.png")
                    terminal.wait("select at most four images")
                    assert not received.is_set(), "refused draft reached the network"
                    # Remove only the fifth path. A subsequent four-image request proves the
                    # refused Enter kept the previous text instead of clearing it.
                    # ConPTY maps DEL (0x7f), not BS (0x08), to the Win32 Backspace key.
                    # Pace events because console::Term does not expand Win32 repeat counts.
                    for _ in " " + quoted[-1]:
                        terminal.send(b"\x7f")
                        threading.Event().wait(0.02)
                    threading.Event().wait(0.2)
                    terminal.send(b"\r")
                    assert received.wait(30), f"edited draft did not submit: {terminal.text()[-2000:]!r}"
                    assert not errors, errors
                    assert len(records) == 1
                    assert "vision_capabilities" not in json.loads(config_path.read_text(encoding="utf-8")), "vision preflight persisted a declaration"
                    assert "Supported by default" not in terminal.text(), "automatic vision declaration was printed"
                    results.append({"surface": "retained" if retained else "plain", "refusal_kept_draft": True,
                                    "vision_config_required": False,
                                    "http": records[0], "scope": "real ConPTY Enter/refusal/edit/retry; no semantic model"})
                finally:
                    release_response.set()
                    terminal.close()
            print(json.dumps({"ok": True, "cases": results, "limitations": [
                "No clipboard was read or modified.", "No real provider, semantic quality, or Desktop UI was tested.",
                "Quoted paths with spaces are covered; Unicode console-key input is not certified.",
                "Test-owned CLI processes were terminated after observing the selected HTTP request."
            ]}, indent=2))
        finally:
            release_response.set()
            server.shutdown()
            server.server_close()


def console_child(binary):
    # Rust caches standard handles; inherit explicit console handles rather than the host pipes.
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                  ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.SetHandleInformation.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handles = []
    try:
        for name, access in (("CONIN$", 0xC0000000), ("CONOUT$", 0xC0000000)):
            handle = kernel.CreateFileW(name, access, 3, None, 3, 0, None)
            if handle == wintypes.HANDLE(-1).value:
                raise ctypes.WinError(ctypes.get_last_error())
            Terminal.check(kernel.SetHandleInformation(handle, 1, 1))
            handles.append(handle)
        kernel.GetConsoleMode.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        kernel.SetConsoleMode.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        mode = wintypes.DWORD()
        Terminal.check(kernel.GetConsoleMode(handles[1], ctypes.byref(mode)))
        Terminal.check(kernel.SetConsoleMode(handles[1], mode.value | 0x0005))
        kernel.SetConsoleCP(65001)
        kernel.SetConsoleOutputCP(65001)
        startup = subprocess.STARTUPINFO()
        startup.dwFlags |= subprocess.STARTF_USESTDHANDLES
        startup.hStdInput = handles[0]
        startup.hStdOutput = startup.hStdError = handles[1]
        startup.lpAttributeList = {"handle_list": handles}
        raise SystemExit(subprocess.call([str(binary)], startupinfo=startup, close_fds=True))
    finally:
        for handle in handles:
            kernel.CloseHandle(handle)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--surface", choices=("all", "plain", "retained"), default="all")
    parser.add_argument("--console-child", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.console_child:
        console_child(args.binary.resolve())
    else:
        run(args.binary.resolve(), args.surface)
