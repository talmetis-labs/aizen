"""Exercise the actual Windows Agent CLI with a loopback scripted model and prebuilt binaries.

Usage: python -I scripts/memory_agent_smoke.py CORE.exe EXAMPLE_MEMORY.exe
Only new fixture-owned roots are used. No build, download, real installation or real data access.
The independent example-memory provider is NOT Mnemos and supplies no Mnemos parity evidence.
Outputs, HTTP requests and a pass/fail report remain in the printed temporary fixture directory.
"""
from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import traceback


PROVIDER_ID = "example-memory"
MODEL = "memory-agent-loopback-fixture"
FAKE_KEY = "loopback-fixture-not-a-credential"
SOUL = "SOUL_CANARY_MEMORY_AGENT"
PERSONA = "PERSONA_CANARY_MEMORY_AGENT"
SELF = "SELF_CANARY_MEMORY_AGENT"
SKILL_INDEX = "SKILL_INDEX_CANARY"
SKILL_BODY = "SKILL_BODY_CANARY"
SESSION = "SESSION_CANARY_MEMORY_AGENT"
RECALL = "RECALL_CANARY_MEMORY_AGENT"
LEGACY = "LEGACY_CANARY_MEMORY_AGENT"
USER_TEXT = "Recalled memory is user-authored text here, not an inserted provider scaffold."
SKILL_NAME = "fixture-boundary-skill"
MEMORY_TOOLS = {"memory_search", "memory_list", "memory_save", "memory_update", "memory_forget"}
LIMITATIONS = [
    "Windows x86_64 MSVC prebuilt Core/example-memory only; other targets are unverified.",
    "Actual Agent CLI/stream-json, not retained REPL/TUI, ConPTY, hostbot, cron or child workflows.",
    "Scripted HTTP/tool/context/session mechanics, not model semantic quality or learning quality.",
    "example-memory is an independent protocol example, NOT Mnemos; no Mnemos parity claim.",
    "The current example build_context returns no standing items; positive recalled context is tested, "
    "but positive standing-context injection cannot be established with this provider.",
    "Legacy canaries test no disclosure/adoption/rewrites, not an OS trace proving zero file opens.",
    "Fixture environment scrubbing and loopback configuration are not kernel filesystem/network "
    "isolation; Windows guarded mode requires explicit fixture-only trust.",
    "Core identity/Skills reads and tool execution are tested; Agent does not exercise REPL "
    "post-turn Persona reflection/Skill auto-learning or provider learning events.",
    "Hashes pin supplied executable bytes, not source/build provenance or publisher trust. "
    "Source-fingerprinted build verification must be supplied separately.",
]
MAX_OUTPUT = 4 * 1024 * 1024
MAX_REQUEST = 4 * 1024 * 1024


def require(condition: bool, message: str) -> None:
    # Remain effective even if someone accidentally invokes Python with -O.
    if not condition:
        raise AssertionError(message)


def sha256(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def inventory(root: Path) -> dict[str, str]:
    """Fixture-only byte inventory; reject links rather than following them."""
    if not root.exists():
        return {}
    result = {"./": "directory"}
    for path in sorted(root.rglob("*")):
        info = path.lstat()
        require(not stat.S_ISLNK(info.st_mode) and not getattr(info, "st_file_attributes", 0) & 0x400,
                f"unexpected link/reparse point in fixture: {path}")
        relative = path.relative_to(root).as_posix()
        if path.is_dir():
            result[relative + "/"] = "directory"
        else:
            require(stat.S_ISREG(info.st_mode), f"non-regular fixture file: {path}")
            result[relative] = sha256(path)
    return result


def isolated_environment(root: Path) -> dict[str, str]:
    # Allowlist rather than trying to enumerate every provider's credential/proxy variable.
    # Do not inherit PATH, Python startup config, cloud config, toolchain homes, or AIZEN_*.
    system_root = Path(os.environ.get("SystemRoot", r"C:\Windows"))
    require(system_root.is_absolute() and system_root.is_dir(), "Windows system root is unavailable")
    home = root / "home"
    values = {
        "SystemRoot": str(system_root), "WINDIR": str(system_root),
        "COMSPEC": str(system_root / "System32" / "cmd.exe"),
        "PATH": os.pathsep.join((str(system_root / "System32"), str(system_root))),
        "PATHEXT": ".COM;.EXE;.BAT;.CMD", "OS": "Windows_NT",
        "HOME": str(home), "USERPROFILE": str(home),
        "APPDATA": str(home / "AppData" / "Roaming"),
        "LOCALAPPDATA": str(home / "AppData" / "Local"),
        "XDG_CONFIG_HOME": str(home / "config"), "XDG_CACHE_HOME": str(home / "cache"),
        "XDG_DATA_HOME": str(home / "data"), "XDG_STATE_HOME": str(home / "state"),
        "TEMP": str(root / "temp"), "TMP": str(root / "temp"), "TMPDIR": str(root / "temp"),
        "AIZEN_HOME": str(home / ".aizen"), "AIZEN_PROJECT_ROOT": str(root / "workspace"),
        # An explicit nonexistent override prevents git's fallback to real known-install locations
        # and hence machine/user git config reads. No tool in this fixture needs git.
        "AIZEN_GIT": str(root / "no-git.exe"),
        "AIZEN_NO_HOOKS": "1", "AIZEN_NO_UPDATE": "1", "AIZEN_NO_WARMUP": "1",
        "AIZEN_NO_TIPS": "1", "AIZEN_NO_EAGER": "1", "AIZEN_ARCHITECT": "0",
        "AIZEN_NO_ID_MIGRATE": "1",  # preserve explicitly seeded identity fixture filenames
        "NO_COLOR": "1", "TERM": "dumb",
    }
    for name in ("HOME", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
                 "XDG_DATA_HOME", "XDG_STATE_HOME", "TEMP"):
        Path(values[name]).mkdir(parents=True, exist_ok=True)
    return values


class OwnedProcessRunner:
    """Create suspended children, assign a private kill-on-close Job, then resume.

    No taskkill, process-name lookup, shell, or unrelated PID is used. Assignment before ResumeThread
    closes the Popen/AssignProcessToJobObject race. Each command must drain its entire job, not just
    return an exit code. Logs go directly to fixture files, with a wall/output cap.
    """

    def __init__(self, root: Path, workspace: Path, env: dict[str, str], timeout: int):
        import msvcrt
        self.msvcrt = msvcrt
        self.root, self.workspace, self.env, self.timeout = root, workspace, env, timeout
        self.cleanup_failures: list[str] = []
        self._job_lock = threading.Lock()
        self._active_job = None
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        size = ctypes.c_size_t
        ulonglong = ctypes.c_ulonglong

        class BasicLimits(ctypes.Structure):
            _fields_ = [("PerProcessUserTimeLimit", ctypes.c_longlong),
                        ("PerJobUserTimeLimit", ctypes.c_longlong), ("LimitFlags", wintypes.DWORD),
                        ("MinimumWorkingSetSize", size), ("MaximumWorkingSetSize", size),
                        ("ActiveProcessLimit", wintypes.DWORD), ("Affinity", size),
                        ("PriorityClass", wintypes.DWORD), ("SchedulingClass", wintypes.DWORD)]

        class IoCounters(ctypes.Structure):
            _fields_ = [(name, ulonglong) for name in ("ReadOperationCount", "WriteOperationCount",
                        "OtherOperationCount", "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]

        class ExtendedLimits(ctypes.Structure):
            _fields_ = [("BasicLimitInformation", BasicLimits), ("IoInfo", IoCounters),
                        ("ProcessMemoryLimit", size), ("JobMemoryLimit", size),
                        ("PeakProcessMemoryUsed", size), ("PeakJobMemoryUsed", size)]

        class Accounting(ctypes.Structure):
            _fields_ = [(name, ctypes.c_longlong) for name in ("TotalUserTime", "TotalKernelTime",
                        "ThisPeriodTotalUserTime", "ThisPeriodTotalKernelTime")] + [
                        (name, wintypes.DWORD) for name in ("TotalPageFaultCount", "TotalProcesses",
                        "ActiveProcesses", "TotalTerminatedProcesses")]

        class StartupInfo(ctypes.Structure):
            _fields_ = [("cb", wintypes.DWORD), ("lpReserved", wintypes.LPWSTR),
                        ("lpDesktop", wintypes.LPWSTR), ("lpTitle", wintypes.LPWSTR)] + [
                        (name, wintypes.DWORD) for name in ("dwX", "dwY", "dwXSize", "dwYSize",
                        "dwXCountChars", "dwYCountChars", "dwFillAttribute", "dwFlags")] + [
                        ("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
                        ("lpReserved2", ctypes.POINTER(ctypes.c_ubyte)),
                        ("hStdInput", wintypes.HANDLE), ("hStdOutput", wintypes.HANDLE),
                        ("hStdError", wintypes.HANDLE)]

        class ProcessInfo(ctypes.Structure):
            _fields_ = [("hProcess", wintypes.HANDLE), ("hThread", wintypes.HANDLE),
                        ("dwProcessId", wintypes.DWORD), ("dwThreadId", wintypes.DWORD)]

        self.ExtendedLimits, self.Accounting = ExtendedLimits, Accounting
        self.StartupInfo, self.ProcessInfo = StartupInfo, ProcessInfo
        signatures = {
            "CreateJobObjectW": ([ctypes.c_void_p, wintypes.LPCWSTR], wintypes.HANDLE),
            "SetInformationJobObject": ([wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD], wintypes.BOOL),
            "QueryInformationJobObject": ([wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD, ctypes.c_void_p], wintypes.BOOL),
            "AssignProcessToJobObject": ([wintypes.HANDLE, wintypes.HANDLE], wintypes.BOOL),
            "CreateProcessW": ([wintypes.LPCWSTR, wintypes.LPWSTR, ctypes.c_void_p, ctypes.c_void_p,
                                wintypes.BOOL, wintypes.DWORD, ctypes.c_void_p, wintypes.LPCWSTR,
                                ctypes.POINTER(StartupInfo), ctypes.POINTER(ProcessInfo)], wintypes.BOOL),
            "ResumeThread": ([wintypes.HANDLE], wintypes.DWORD),
            "WaitForSingleObject": ([wintypes.HANDLE, wintypes.DWORD], wintypes.DWORD),
            "GetExitCodeProcess": ([wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)], wintypes.BOOL),
            "TerminateProcess": ([wintypes.HANDLE, wintypes.UINT], wintypes.BOOL),
            "TerminateJobObject": ([wintypes.HANDLE, wintypes.UINT], wintypes.BOOL),
            "CloseHandle": ([wintypes.HANDLE], wintypes.BOOL),
        }
        for name, (parameters, result) in signatures.items():
            function = getattr(self.kernel, name)
            function.argtypes, function.restype = parameters, result

    @staticmethod
    def checked(ok, operation: str):
        if not ok:
            raise OSError(ctypes.get_last_error(), operation)
        return ok

    def active_processes(self) -> int:
        """Inspect only this runner's live Job; never enumerate the machine's processes."""
        with self._job_lock:
            require(self._active_job is not None, "no fixture Job is active")
            accounting = self.Accounting()
            self.checked(self.kernel.QueryInformationJobObject(self._active_job, 1, ctypes.byref(accounting),
                         ctypes.sizeof(accounting), None), "QueryInformationJobObject")
            return accounting.ActiveProcesses

    def run(self, name: str, argv: list[str], stdin: bytes | None = None) -> dict:
        stdout_path, stderr_path = self.root / f"{name}.stdout", self.root / f"{name}.stderr"
        input_path = os.devnull
        if stdin is not None:
            require(len(stdin) <= MAX_OUTPUT, "fixture input exceeds output-sized bound")
            input_path = self.root / f"{name}.stdin"
            with input_path.open("xb") as stream:
                stream.write(stdin)
        job = self.checked(self.kernel.CreateJobObjectW(None, None), "CreateJobObjectW")
        process = self.ProcessInfo()
        assigned = settled = False
        started = time.monotonic()
        peak_active = 0
        total_processes = 0
        try:
            limits = self.ExtendedLimits()
            limits.BasicLimitInformation.LimitFlags = 0x2000 | 0x8  # kill-on-close + active limit
            limits.BasicLimitInformation.ActiveProcessLimit = 12
            self.checked(self.kernel.SetInformationJobObject(job, 9, ctypes.byref(limits), ctypes.sizeof(limits)),
                         "SetInformationJobObject")
            with open(input_path, "rb") as source, stdout_path.open("xb") as output, stderr_path.open("xb") as errors:
                streams = (source, output, errors)
                handles = [self.msvcrt.get_osfhandle(stream.fileno()) for stream in streams]
                startup = self.StartupInfo()
                startup.cb, startup.dwFlags = ctypes.sizeof(startup), 0x100  # STARTF_USESTDHANDLES
                startup.hStdInput, startup.hStdOutput, startup.hStdError = handles
                block = ctypes.create_unicode_buffer("\0".join(
                    f"{key}={value}" for key, value in sorted(self.env.items(), key=lambda pair: pair[0].upper())) + "\0\0")
                command = ctypes.create_unicode_buffer(subprocess.list2cmdline(argv))
                try:
                    for handle in handles:
                        os.set_handle_inheritable(handle, True)
                    # DETACHED_PROCESS avoids a hidden conhost.exe being charged as a child.
                    # Explicit stdio handles still capture logs; no command needs a console.
                    self.checked(self.kernel.CreateProcessW(argv[0], command, None, None, True,
                                 0x4 | 0x400 | 0x8, block, str(self.workspace),
                                 ctypes.byref(startup), ctypes.byref(process)), "CreateProcessW")
                finally:
                    for handle in handles:
                        os.set_handle_inheritable(handle, False)
                self.checked(self.kernel.AssignProcessToJobObject(job, process.hProcess), "AssignProcessToJobObject")
                assigned = True
                with self._job_lock:
                    require(self._active_job is None, "fixture runner cannot execute concurrent commands")
                    self._active_job = job
                require(self.kernel.ResumeThread(process.hThread) != 0xFFFFFFFF, "ResumeThread failed")
                deadline = started + self.timeout
                exited_at = None
                while True:
                    accounting = self.Accounting()
                    self.checked(self.kernel.QueryInformationJobObject(job, 1, ctypes.byref(accounting),
                                 ctypes.sizeof(accounting), None), "QueryInformationJobObject")
                    peak_active = max(peak_active, accounting.ActiveProcesses)
                    total_processes = accounting.TotalProcesses
                    now = time.monotonic()
                    require(now <= deadline, f"{name}: own job exceeded {self.timeout}s")
                    require(stdout_path.stat().st_size + stderr_path.stat().st_size <= MAX_OUTPUT,
                            f"{name}: output exceeded {MAX_OUTPUT} bytes")
                    wait = self.kernel.WaitForSingleObject(process.hProcess, 50)
                    require(wait in (0, 258), f"{name}: process wait failed")
                    if wait == 0:
                        exited_at = exited_at or now
                        if accounting.ActiveProcesses == 0:
                            settled = True
                            break
                        require(now - exited_at < 5, f"{name}: descendants outlived Core for five seconds")
                        time.sleep(0.025)
                code = wintypes.DWORD()
                self.checked(self.kernel.GetExitCodeProcess(process.hProcess, ctypes.byref(code)), "GetExitCodeProcess")
            require(stdout_path.stat().st_size + stderr_path.stat().st_size <= MAX_OUTPUT,
                    f"{name}: final output exceeded {MAX_OUTPUT} bytes")
            return {"exit": code.value, "stdout": stdout_path.read_text(encoding="utf-8", errors="replace"),
                    "stderr": stderr_path.read_text(encoding="utf-8", errors="replace"),
                    "job_settled": settled, "job_processes": total_processes,
                    "job_peak_sampled": peak_active, "wall_seconds": round(time.monotonic() - started, 3)}
        finally:
            if process.hProcess and not settled:
                # Only handles created above; a failure before assignment leaves a suspended child.
                operation = self.kernel.TerminateJobObject if assigned else self.kernel.TerminateProcess
                target = job if assigned else process.hProcess
                if not operation(target, 1):
                    self.cleanup_failures.append(f"{name}: own termination failed ({ctypes.get_last_error()})")
                cleanup_deadline = time.monotonic() + 5
                while time.monotonic() < cleanup_deadline:
                    if assigned:
                        accounting = self.Accounting()
                        ok = self.kernel.QueryInformationJobObject(job, 1, ctypes.byref(accounting),
                                                                  ctypes.sizeof(accounting), None)
                        if not ok:
                            self.cleanup_failures.append(f"{name}: cannot query own cleanup settlement")
                            break
                        # Job accounting may reach zero before the root's final handle cleanup.
                        if accounting.ActiveProcesses == 0 and self.kernel.WaitForSingleObject(process.hProcess, 50) == 0:
                            settled = True
                            break
                    elif self.kernel.WaitForSingleObject(process.hProcess, 50) == 0:
                        settled = True
                        break
                    time.sleep(0.025)
                if not settled:
                    self.cleanup_failures.append(f"{name}: own tree settlement unproved after bounded cleanup")
            with self._job_lock:
                if self._active_job == job:
                    self._active_job = None
            for handle in (process.hThread, process.hProcess, job):
                if handle:
                    self.kernel.CloseHandle(handle)


def message_text(message: dict) -> str:
    content = message.get("content", "")
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    require(isinstance(content, list), "unexpected message content type")
    require(all(part.get("type") == "text" for part in content), "non-text fixture message")
    return "\n".join(part.get("text", "") for part in content)


def matching_tool_result(body: dict, call_id: str, name: str) -> str:
    calls = [call for message in body["messages"] for call in message.get("tool_calls", [])
             if call.get("id") == call_id]
    require(len(calls) == 1 and calls[0]["function"]["name"] == name, f"missing actual assistant tool call {name}")
    results = [message for message in body["messages"]
               if message.get("role") == "tool" and message.get("tool_call_id") == call_id]
    require(len(results) == 1, f"missing/duplicate actual tool result {name}")
    return message_text(results[0])


class ScriptedResponse:
    """An explicit fixture HTTP reply, including bounded transport-failure cases."""

    def __init__(self, status: int, body: bytes):
        require(100 <= status <= 599 and len(body) <= MAX_REQUEST, "invalid fixture response")
        self.status, self.body = status, body


class ScriptedModel:
    def __init__(self, root: Path):
        self.root = root
        self.lock = threading.Lock()
        self.phase = "unarmed"
        self.step = 0
        self.active = False
        self.resume = False
        self.source_task = None
        self.session_slug = None
        self.record_id = None
        self.record_body = None
        self.pending = None
        self.records: list[dict] = []
        self.errors: list[str] = []
        self.phase_tools: list[str] = []

    def arm(self, phase: str, *, active: bool = False, resume: bool = False):
        with self.lock:
            self.phase, self.step, self.active, self.resume = phase, 0, active, resume
            self.pending, self.phase_tools = None, []

    def reply(self, body: dict, raw: bytes):
        with self.lock:
            require(self.phase != "unarmed", "unscheduled model request")
            number = len(self.records)
            self.root.joinpath(f"http-{number:03d}-{self.phase}.json").write_bytes(raw)
            names = [tool["function"]["name"] for tool in body.get("tools", [])]
            require(len(names) == len(set(names)), "duplicate tool definitions")
            require(body.get("model") == MODEL and body.get("stream") is True,
                    "unexpected model route/non-stream auxiliary call")
            require({"skill_load", "session_recall", "file_read"} <= set(names), "Core tools unavailable")
            memory_names = {name for name in names if name.startswith("memory_")}
            require(memory_names == (MEMORY_TOOLS if self.active else set()),
                    f"{self.phase}: unexpected Memory tools: {memory_names}")
            messages = body.get("messages", [])
            systems = "\n".join(message_text(message) for message in messages if message.get("role") == "system")
            texts = "\n".join(message_text(message) for message in messages)
            for tag, canary in (("agent_identity", SOUL), ("persona", PERSONA), ("self", SELF), ("skills", SKILL_INDEX)):
                require(f"\n<{tag}>\n" in systems and canary in systems, f"{self.phase}: missing Core {tag}")
            require(LEGACY not in raw.decode("utf-8"), "legacy Memory canary disclosed to model")
            require('"memory_scaffold"' not in raw.decode("utf-8"), "storage provenance leaked on model wire")
            # The example's build_context is intentionally empty. Do not invent standing evidence.
            require("\n<user_memory>\n" not in systems, "unexpected standing context from empty example provider")
            recalled_users = [message_text(message) for message in messages if message.get("role") == "user"
                              and message_text(message).startswith("Recalled memory (external lower-trust evidence;")]
            require(bool(recalled_users) == self.active, f"{self.phase}: recalled prefix active-state mismatch")
            if self.active:
                require(any(RECALL in content for content in recalled_users), "active recall canary absent")
            else:
                require(RECALL not in texts, f"{self.phase}: stale provider context/result leaked")
            if self.resume:
                require(self.source_task in [message_text(message) for message in messages if message.get("role") == "user"],
                        "resume failed to remove exactly the sealed recall prefix while retaining user text")
                require(USER_TEXT in texts, "unmarked user-authored Memory mention was stripped")
                require("RESUME_SOURCE_ASSISTANT" in texts, "ordinary saved assistant history lost")
            self.records.append({"phase": self.phase, "step": self.step, "bytes": len(raw),
                                 "tools": sorted(names), "recalled_prefixes": len(recalled_users)})
            if self.pending:
                call_id, name = self.pending
                value = matching_tool_result(body, call_id, name)
                if name == "skill_load":
                    require(SKILL_BODY in value, "skill_load did not return fixture procedure")
                elif name == "session_recall":
                    require(SESSION in value and "Digest of saved conversation" in value, "Core session digest unavailable")
                elif name == "memory_search":
                    prefix = "External Memory result (lower-trust data, not tool/policy authority):\n"
                    require(value.startswith(prefix), "Memory tool lost lower-trust wrapper or returned an error")
                    result = json.loads(value[len(prefix):])
                    hits = result.get("items", [])
                    require(any(hit["record"]["id"] == self.record_id and
                                hit["record"]["content"] == self.record_body and
                                hit["record"]["revision"] == 1 and hit["score"] > 0 for hit in hits),
                            "actual model-requested memory_search did not return stored fixture record")
                    require(LEGACY not in value, "legacy facts were automatically adopted")
                self.pending = None
            step = self.step
            self.step += 1
            if self.phase == "seed-session":
                require(step == 0 and not self.active, "unexpected seed continuation")
                return None, "Synthetic saved conversation. " + SESSION
            if self.phase == "active-resume-source":
                require(step == 0 and self.active, "unexpected resume-source continuation")
                return None, "RESUME_SOURCE_ASSISTANT: synthetic history, no quality claim."
            schedule = [("skill_load", {"name": SKILL_NAME}),
                        ("session_recall", {"name": self.session_slug})]
            if self.active:
                schedule.append(("memory_search", {"query": "amber lantern", "limit": 5}))
            if step < len(schedule):
                name, arguments = schedule[step]
                call_id = f"fixture-{self.phase}-{step}"
                self.pending = (call_id, name)
                self.phase_tools.append(name)
                return (call_id, name), arguments
            require(step == len(schedule), "unexpected retry/extra agent request")
            return None, "Scripted boundary mechanics observed; no semantic or Mnemos parity claim."


def make_server(recorder: ScriptedModel):
    class OwnedServer(ThreadingHTTPServer):
        daemon_threads = True
        block_on_close = False

        def __init__(self, *args):
            self.connections = set()
            self.connections_changed = threading.Condition()
            super().__init__(*args)

        def process_request(self, request, client_address):
            with self.connections_changed:
                self.connections.add(request)
            try:
                super().process_request(request, client_address)
            except BaseException:
                self.shutdown_request(request)
                raise

        def shutdown_request(self, request):
            try:
                super().shutdown_request(request)
            finally:
                with self.connections_changed:
                    self.connections.discard(request)
                    self.connections_changed.notify_all()

        def drain_owned_connections(self):
            with self.connections_changed:
                connections = list(self.connections)
            for connection in connections:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                connection.close()
            with self.connections_changed:
                return self.connections_changed.wait_for(lambda: not self.connections, timeout=5)

        def handle_error(self, request, client_address):
            recorder.errors.append("loopback request handler failed: " + traceback.format_exc()[-1000:])

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def log_message(self, *_):
            pass

        def send_bytes(self, status: int, data: bytes, content_type: str = "application/json"):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            # Some prebuilt versions discover model limits even with the exact configured route.
            if self.path in ("/v1/models", "/v1/models/" + MODEL):
                self.send_bytes(200, json.dumps({"object": "list", "data": [
                    {"id": MODEL, "context_window": 131072}]}).encode())
            else:
                recorder.errors.append("unexpected GET " + self.path)
                self.send_bytes(404, b'{"error":"unexpected fixture route"}')

        def do_POST(self):
            try:
                require(self.client_address[0] == "127.0.0.1" and self.path == "/v1/chat/completions",
                        "non-loopback/unexpected request path")
                require(self.headers.get("Authorization") == "Bearer " + FAKE_KEY, "unexpected fixture authorization")
                require(not self.headers.get("Transfer-Encoding"), "unexpected chunked request")
                size = int(self.headers.get("Content-Length", "0"))
                require(0 < size <= MAX_REQUEST, "request exceeded fixture body bound")
                raw = self.rfile.read(size)
                require(len(raw) == size, "truncated HTTP request")
                require(len(recorder.records) < 64, "too many model requests")
                body = json.loads(raw)
                reply = recorder.reply(body, raw)
                if isinstance(reply, ScriptedResponse):
                    self.send_bytes(reply.status, reply.body)
                    return
                call, value = reply
                if body.get("stream") is False:
                    require(call is None, "non-stream fixture response must not invent tool calls")
                    self.send_bytes(200, json.dumps({"id": "loopback-memory", "object": "chat.completion",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": value},
                                     "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 128, "completion_tokens": 16}}).encode())
                    return
                require(body.get("stream") is True, "fixture request must select a stream mode")
                if call:
                    call_id, name = call
                    delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": call_id,
                             "type": "function", "function": {"name": name, "arguments": json.dumps(value)}}]}
                    finish = "tool_calls"
                else:
                    delta, finish = {"role": "assistant", "content": value}, "stop"
                events = [
                    {"id": "loopback-memory", "object": "chat.completion.chunk",
                     "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                    {"id": "loopback-memory", "object": "chat.completion.chunk",
                     "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                     "usage": {"prompt_tokens": 128, "completion_tokens": 16}},
                ]
                encoded = b"".join(b"data: " + json.dumps(event).encode() + b"\n\n" for event in events)
                self.send_bytes(200, encoded + b"data: [DONE]\n\n", "text/event-stream")
            except Exception as error:
                recorder.errors.append(f"{recorder.phase}: {type(error).__name__}: {error}")
                try:
                    self.send_bytes(400, json.dumps({"error": {"message": str(error),
                                    "type": "invalid_request_error"}}).encode())
                except OSError:
                    pass

    server = OwnedServer(("127.0.0.1", 0), Handler)
    server.timeout = 5
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)
    thread.start()
    return server, thread


def write_fixture(path: Path, text: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path, help="Prebuilt, separately verified Windows MSVC aizen executable")
    parser.add_argument("example_provider", type=Path, help="Prebuilt independent example-memory executable, NOT Mnemos")
    parser.add_argument("--timeout", type=int, default=90, help="Per-command wall bound, 15..180 seconds (default 90)")
    parser.add_argument("--temp-parent", type=Path, help="Existing parent for a NEW unique retained fixture directory")
    args = parser.parse_args()
    require(sys.flags.isolated == 1, "run this harness with Python -I")
    require(os.name == "nt" and ctypes.sizeof(ctypes.c_void_p) == 8, "Windows x86_64 harness only")
    require(15 <= args.timeout <= 180, "--timeout must be 15..180")
    core, provider = args.core.resolve(strict=True), args.example_provider.resolve(strict=True)
    require(core.is_file() and provider.is_file() and core != provider, "two distinct prebuilt binaries are required")
    repo = Path(__file__).resolve().parents[1]
    parent = args.temp_parent.resolve(strict=True) if args.temp_parent else None
    require(parent is None or parent.is_dir(), "--temp-parent must be an existing directory")
    root = Path(tempfile.mkdtemp(prefix="aizen-memory-agent-", dir=parent))
    print(f"Agent Memory fixture: {root}", flush=True)
    workspace = root / "workspace"
    workspace.mkdir()
    # Stop project-convention discovery at the owned fixture, never at a real ancestor's CLAUDE.md.
    (workspace / ".git").mkdir()
    env = isolated_environment(root)
    aizen = Path(env["AIZEN_HOME"])
    runner = OwnedProcessRunner(root, workspace, env, args.timeout)
    recorder = ScriptedModel(root)
    server = server_thread = None
    report = {"root": str(root), "passed": False, "core": str(core), "provider": str(provider),
              "core_sha256_before": sha256(core), "provider_sha256_before": sha256(provider),
              "script_sha256": sha256(Path(__file__).resolve()), "cases": [],
              "limitations": LIMITATIONS, "provider_identity": PROVIDER_ID,
              "standing_context_positive_tested": False}
    report_path = root / "report.json"

    def save_report():
        report["model_requests"], report["model_errors"] = recorder.records, recorder.errors
        report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    def run(name: str, *arguments: str, success: bool = True, program: Path = core):
        result = runner.run(name, [str(program), *map(str, arguments)])
        report["cases"].append({"name": name, **{key: value for key, value in result.items()
                                if key not in ("stdout", "stderr")}})
        save_report()
        require((result["exit"] == 0) == success,
                f"{name}: exit {result['exit']}; {result['stderr'][-2500:]} {result['stdout'][-1000:]}")
        require(sha256(core) == report["core_sha256_before"], f"{name}: Core binary changed")
        return result

    def status(name: str, *, enabled: bool, selected, installed: int):
        value = json.loads(run(name, "memory", "status", "--json")["stdout"])
        expected_state = "selected" if enabled else "disabled" if selected else "unavailable"
        require(value["enabled"] is enabled and value["selected"] == selected and
                value["installed"] == installed and value["state"] == expected_state and
                value["recovery_required"] is False, f"{name}: unexpected provider state {value}")
        return value

    def task(label: str):
        return (f"Synthetic amber lantern boundary probe {label}.\n{USER_TEXT}\n"
                "Use only the fixture-directed read tools, no edits, shell, delegation or external services. "
                "Do not claim semantic quality or production readiness.")

    def agent(phase: str, *, active=False, resume=None, save=True):
        recorder.arm(phase, active=active, resume=resume is not None)
        before = len(recorder.records)
        prompt = task(phase)
        if phase == "active-resume-source":
            recorder.source_task = prompt
        arguments = ["--sandbox", "guarded", "agent", prompt, "--base-url", base,
                     "--api-key", FAKE_KEY, "--model", MODEL, "--design", "off",
                     "--output-format", "stream-json", "--max-iters", "8", "--yes"]
        if save:
            arguments.append("--save-session")
        if resume:
            arguments.extend(("--resume", resume))
        completed = run(phase, *arguments)
        require(completed["job_processes"] > 1 if active else completed["job_processes"] == 1,
                f"{phase}: unexpected child-process activity: {completed['job_processes']}")
        require(not recorder.errors, f"scripted provider assertions failed: {recorder.errors}")
        events = [json.loads(line) for line in completed["stdout"].splitlines() if line.strip()]
        initial = [event for event in events if event.get("type") == "system" and event.get("subtype") == "init"]
        results = [event for event in events if event.get("type") == "result"]
        require(len(initial) == 1 and len(results) == 1, f"{phase}: missing/duplicate init or result")
        require(not results[0].get("is_error") and not results[0].get("permission_denials"),
                f"{phase}: Agent reported failure/denial: {results[0]}")
        advertised = {name for name in initial[0]["tools"] if name.startswith("memory_")}
        require(advertised == (MEMORY_TOOLS if active else set()), f"{phase}: init Memory tools mismatch")
        expected_tools = [] if phase in ("seed-session", "active-resume-source") else ["skill_load", "session_recall"]
        if active and phase != "active-resume-source":
            expected_tools.append("memory_search")
        require(recorder.phase_tools == expected_tools and recorder.pending is None, f"{phase}: tool sequence incomplete")
        require(len(recorder.records) - before == len(expected_tools) + 1, f"{phase}: unexpected request count")
        saved = [event for event in events if event.get("type") == "system" and event.get("subtype") == "session_saved"]
        if save:
            require(len(saved) == 1 and results[0].get("session") == saved[0]["slug"], f"{phase}: session not saved")
            path = Path(saved[0]["path"])
            require(path.resolve().parent == (aizen / "sessions").resolve() and path.is_file(),
                    f"{phase}: session escaped fixture or is missing")
            value = json.loads(path.read_text(encoding="utf-8"))
            require(value.get("version") == 2 and value["meta"]["project_root"], "missing session provenance")
            return saved[0]["slug"], path, value
        require(not saved, "read-only resume unexpectedly saved history")
        return None

    try:
        status("initial-status", enabled=False, selected=None, installed=0)
        require(not aizen.exists(), "read-only initial status initialized a data/config root")
        server, server_thread = make_server(recorder)
        base = f"http://127.0.0.1:{server.server_port}/v1"
        write_fixture(aizen / "cli-config.json", json.dumps({
            "base_url": base, "api_key": FAKE_KEY, "model": MODEL, "model_context_window": 131072,
            "architect_mode": False, "self_review": False, "eager_tools": False, "lean_tools": False,
            "auto_effort": False, "compact_threshold_pct": 0, "auto_skill_learn": False,
            "memory_auto_learn": False, "persona_evolve": False, "persona_for_coding": True,
            "persona": "fixture-guide", "update_check": False, "auto_copy": False,
        }, indent=2) + "\n")
        write_fixture(aizen / "SOUL.md", f"Patient fixture operating voice. {SOUL}.\n")
        write_fixture(aizen / "personas/fixture-guide.md",
                      f"---\nname: fixture-guide\nrole: a patient fixture guide\nvoice: concise\n---\n{PERSONA}.\n")
        write_fixture(aizen / "personas/fixture-guide.self/fixture-insight.md",
                      f"---\nkind: insight\nimportance: 10\n---\n{SELF}: patient fixture experience.\n")
        write_fixture(aizen / f"skills/{SKILL_NAME}.md",
                      f"---\nname: {SKILL_NAME}\ndescription: {SKILL_INDEX}\nwhen: amber lantern boundary probe {SKILL_INDEX}\n---\n"
                      f"1. Identify this synthetic fixture. {SKILL_BODY}.\n")
        identity_files = {str(path): sha256(path) for path in (aizen / "SOUL.md",
                          aizen / "personas/fixture-guide.md", aizen / "personas/fixture-guide.self/fixture-insight.md")}
        recorder.session_slug, _, _ = agent("seed-session")
        agent("no-provider")
        for name in ("plugins", "memory-data", "plugin-state", "cli-memory", "models"):
            require(not (aizen / name).exists(), f"clean no-provider Agent initialized {name}")

        # Add ONLY fixture legacy material after the clean-root case. Never inspect the real HOME.
        legacy = aizen / "cli-memory"
        for name in ("entries/legacy-fact.md", "review/legacy-review.md", "archive/legacy-archive.md"):
            write_fixture(legacy / name, "---\nname: legacy-fixture\ntype: user\n---\n"
                          f"amber lantern {LEGACY}: legacy fixture only.\n")
        for name in ("STYLE.md", "core.active.md", "core.next.md", "core/active/user.md",
                     "core/active/device.md", "core/next/user.md"):
            write_fixture(legacy / name, f"amber lantern {LEGACY}: no implicit adoption.\n")
        write_fixture(legacy / "graph.tsv", f"{LEGACY}\tfixture\t1\n")
        legacy_before = inventory(legacy)
        agent("legacy-no-provider")
        require(inventory(legacy) == legacy_before, "no-provider Agent rewrote legacy data")

        # Byte-copy hardlinked Cargo artifacts before invoking the verified pure packaging helper.
        source = root / "package-source"
        source.mkdir()
        for original, name in ((provider, "example-memory.exe"), (repo / "LICENSE", "LICENSE"), (repo / "NOTICE", "NOTICE")):
            shutil.copyfile(original, source / name)
        package_output = root / "packaged"
        run("package-example", "-I", str(repo / "scripts/package_memory_plugin.py"),
            "--id", PROVIDER_ID, "--name", "Independent example fixture", "--version", "0.1.0",
            "--target", "x86_64-pc-windows-msvc", "--binary", str(source / "example-memory.exe"),
            "--entrypoint", "example-memory.exe", "--license", str(source / "LICENSE"),
            "--notice", str(source / "NOTICE"), "--capability", "records", "--capability", "recall",
            "--capability", "context", "--output", str(package_output), program=Path(sys.executable).resolve())
        archives = list(package_output.glob("*.zip"))
        require(len(archives) == 1, "packager did not produce exactly one archive")
        report["package"] = {"path": str(archives[0]), "sha256": sha256(archives[0])}
        run("install-example", "plugin", "install", str(archives[0]))
        status("installed-status", enabled=False, selected=None, installed=1)
        installed_before = inventory(aizen / "plugins" / PROVIDER_ID)
        agent("installed-inactive")
        require(not (aizen / "memory-data").exists(), "inactive package initialized provider data")
        run("eligible-example", "plugin", "enable", PROVIDER_ID)
        status("eligible-status", enabled=False, selected=None, installed=1)
        agent("eligible-unselected")
        require(not (aizen / "memory-data").exists(), "enable silently executed/selected the provider")
        run("select-example", "memory", "use", PROVIDER_ID, "--yes", "--allow-guarded")
        active = status("active-status", enabled=True, selected=PROVIDER_ID, installed=1)
        require(not (aizen / "memory-data").exists(), "selection alone initialized provider data")
        health = json.loads(run("example-health", "--sandbox", "guarded", "memory", "health")["stdout"])
        require(health.get("provider") == PROVIDER_ID and health.get("status") == "ready", "not the expected example fixture")
        recorder.record_body = "Amber lantern synthetic fixture record. " + RECALL
        stored = json.loads(run("seed-provider-record", "--sandbox", "guarded", "memory", "add", "fixture-record",
                            "--body", recorder.record_body)["stdout"])
        record = stored["record"]
        require(record["content"] == recorder.record_body and record["revision"] == 1, "record setup failed")
        recorder.record_id = record["id"]
        report["fixture_record_id"] = record["id"]
        agent("selected-active", active=True)
        source_slug, source_path, saved_source = agent("active-resume-source", active=True)
        sealed = [message for message in saved_source["messages"] if message.get("memory_scaffold")]
        require(len(sealed) == 1 and RECALL in message_text(sealed[0]), "saved recall lacks host provenance")
        require(message_text(sealed[0]).endswith(recorder.source_task), "saved actual user text missing")
        source_before = inventory(aizen / "sessions")
        data = aizen / "memory-data" / PROVIDER_ID
        require((data / "records.json").is_file(), "example did not persist data")
        write_fixture(data / "fixture-preserve.txt", "Provider data survives disable and uninstall.\n")
        data_before = inventory(data)
        require(LEGACY not in (data / "records.json").read_text(encoding="utf-8"), "legacy facts automatically adopted")
        run("disable-memory", "memory", "disable")
        disabled = status("disabled-status", enabled=False, selected=PROVIDER_ID, installed=1)
        require(disabled["generation"] > active["generation"], "disable did not invalidate selection generation")
        agent("disabled", save=False)
        agent("resume-disabled", resume=source_slug, save=False)
        require(inventory(aizen / "sessions") == source_before, "resume rewrote saved source history")
        require(inventory(data) == data_before, "disabled/resumed Agent modified provider data")
        run("disabled-explicit-memory", "--sandbox", "guarded", "memory", "search", "amber lantern", success=False)
        run("disable-package", "plugin", "disable", PROVIDER_ID)
        status("package-disabled-status", enabled=False, selected=PROVIDER_ID, installed=1)
        agent("package-disabled", save=False)
        require(inventory(aizen / "plugins" / PROVIDER_ID) == installed_before, "installed package bytes changed")
        run("uninstall-example", "plugin", "uninstall", PROVIDER_ID, "--yes")
        status("uninstalled-status", enabled=False, selected=None, installed=0)
        agent("resume-uninstalled", resume=source_slug, save=False)
        require(not (aizen / "plugins" / PROVIDER_ID).exists(), "uninstall left package present")
        require(inventory(data) == data_before, "uninstall/resume deleted or modified preserved provider data")
        require(inventory(aizen / "sessions") == source_before, "disabled/uninstalled runs rewrote saved history")
        require(inventory(legacy) == legacy_before, "legacy data changed during lifecycle")
        require(all(sha256(Path(path)) == digest for path, digest in identity_files.items()), "Core identity files changed")
        require(not (aizen / "plugin-state/memory-transaction.json").exists(), "provider transaction did not settle")
        report.update({"passed": True, "provider_data_preserved": True, "legacy_unchanged": True,
                       "saved_source_unchanged": True, "actual_memory_search_verified": True,
                       "resume_source": str(source_path), "recall_invalidation_verified": True})
    except Exception as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        traceback.print_exc()
    finally:
        if server is not None:
            stop = threading.Thread(target=server.shutdown, daemon=True)
            stop.start()
            stop.join(timeout=5)
            connections_drained = server.drain_owned_connections()
            server.server_close()
            server_thread.join(timeout=5)
            if stop.is_alive() or server_thread.is_alive() or not connections_drained:
                report["passed"] = False
                report["server_cleanup_error"] = "loopback server failed bounded shutdown"
        report["core_sha256_after"] = sha256(core)
        report["provider_sha256_after"] = sha256(provider)
        report["core_unchanged"] = report["core_sha256_after"] == report["core_sha256_before"]
        if not report["core_unchanged"] or report["provider_sha256_after"] != report["provider_sha256_before"]:
            report["passed"] = False
            report["binary_integrity_error"] = "supplied executable bytes changed"
        if recorder.errors:
            report["passed"] = False
        report["process_cleanup_failures"] = runner.cleanup_failures
        if runner.cleanup_failures:
            report["passed"] = False
        save_report()
    print(f"{'PASS' if report['passed'] else 'FAIL'}: {report_path}", flush=True)
    for limitation in LIMITATIONS:
        print("LIMITATION: " + limitation, flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
