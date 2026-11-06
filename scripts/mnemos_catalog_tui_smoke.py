"""Windows ConPTY smoke for Mnemos selected-version install/update consent.

python -I scripts/mnemos_catalog_tui_smoke.py target/debug/aizen.exe
Uses the live anonymous public channel and new disposable homes only. Downloads
and installs but NEVER executes Mnemos, activates a public package, or runs Memory.
The update source is a synthetic 0.0.1 package containing an inert copy of Core,
not an older public release. Screen/state evidence remains in the printed temp root.
"""
import argparse
import ctypes
from ctypes import wintypes
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import unicodedata

sys.dont_write_bytecode = True


def load_sibling(name, filename):
    # -I excludes the script directory; load only explicitly named trusted harnesses.
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


fixture = load_sibling("catalog_terminal", "vision_tui_smoke.py")
support = load_sibling("catalog_environment", "memory_agent_smoke.py")
require = support.require
REPOSITORY = "talmetis-labs/aizen-mnemos-releases"
TARGET = "x86_64-pc-windows-msvc"
OLD_VERSION = "0.0.1"
WIDTH, HEIGHT = 140, 45  # vision_tui_smoke.Terminal's actual ConPTY dimensions


def screen_text(raw):
    """Reconstruct the visible viewport, not a transcript containing old prompts."""
    cells = {}
    row = col = 0
    saved = (0, 0)
    tokens = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[([0-?]*)([ -/]*)([@-~])|\x1b.|[^\x1b]", re.DOTALL)

    def scroll(count):
        nonlocal cells
        cells = {(r - count, c): char for (r, c), char in cells.items() if 0 <= r - count < HEIGHT}

    for token in tokens.finditer(raw.decode("utf-8", errors="replace")):
        value = token.group()
        if value.startswith("\x1b["):
            params, _, code = token.groups()
            parts = params.lstrip("?<>=").split(";")
            nums = [int(n) if n.isdigit() else 0 for n in parts]
            n = nums[0] or 1
            if code in ("H", "f"):
                row = min(HEIGHT - 1, max(0, n - 1))
                col = min(WIDTH - 1, max(0, (nums[1] if len(nums) > 1 else 1) - 1))
            elif code == "A": row = max(0, row - n)
            elif code == "B": row = min(HEIGHT - 1, row + n)
            elif code == "C": col = min(WIDTH - 1, col + n)
            elif code == "D": col = max(0, col - n)
            elif code == "E": row, col = min(HEIGHT - 1, row + n), 0
            elif code == "F": row, col = max(0, row - n), 0
            elif code == "G": col = min(WIDTH - 1, n - 1)
            elif code == "d": row = min(HEIGHT - 1, n - 1)
            elif code == "s": saved = (row, col)
            elif code == "u": row, col = saved
            elif code == "S": scroll(n)
            elif code == "T": scroll(-n)
            elif code == "J":
                mode = nums[0]
                for position in list(cells):
                    if mode == 2 or (mode == 0 and position >= (row, col)) or (mode == 1 and position <= (row, col)):
                        del cells[position]
            elif code == "K":
                for position in list(cells):
                    if position[0] == row and (nums[0] == 2 or (nums[0] == 0 and position[1] >= col) or (nums[0] == 1 and position[1] <= col)):
                        del cells[position]
            elif code == "X":
                for c in range(col, min(WIDTH, col + n)):
                    cells.pop((row, c), None)
            elif code in ("h", "l") and params in ("?1049", "?1047"):
                cells.clear()
                row = col = 0
        elif value == "\x1b7": saved = (row, col)
        elif value == "\x1b8": row, col = saved
        elif value.startswith("\x1b"):
            continue
        elif value == "\r": col = 0
        elif value == "\n":
            if row == HEIGHT - 1:
                scroll(1)
            else:
                row += 1
        elif value == "\b": col = max(0, col - 1)
        elif value == "\t": col = min(WIDTH - 1, (col // 8 + 1) * 8)
        elif value >= " ":
            if unicodedata.combining(value):
                position = (row, max(0, col - 1))
                cells[position] = cells.get(position, "") + value
            else:
                width = 2 if unicodedata.east_asian_width(value) in ("W", "F") else 1
                if col + width > WIDTH:
                    col = 0
                    if row == HEIGHT - 1:
                        scroll(1)
                    else:
                        row += 1
                cells[(row, col)] = value
                col += width
    return "\n".join("".join(cells.get((r, c), " ") for c in range(WIDTH)).rstrip() for r in range(HEIGHT))


def console_process_fence(kernel):
    """Allow only this Python console wrapper plus Core, never a provider child.

    The Job is inherited by Core. Without breakaway flags, ActiveProcessLimit=2
    refuses any third process before it starts. ConPTY/cmd already live outside it.
    """
    class BasicLimits(ctypes.Structure):
        _fields_ = [("PerProcessUserTimeLimit", ctypes.c_longlong),
                    ("PerJobUserTimeLimit", ctypes.c_longlong), ("LimitFlags", wintypes.DWORD),
                    ("MinimumWorkingSetSize", ctypes.c_size_t), ("MaximumWorkingSetSize", ctypes.c_size_t),
                    ("ActiveProcessLimit", wintypes.DWORD), ("Affinity", ctypes.c_size_t),
                    ("PriorityClass", wintypes.DWORD), ("SchedulingClass", wintypes.DWORD)]

    kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    kernel.CreateJobObjectW.restype = wintypes.HANDLE
    kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    job = kernel.CreateJobObjectW(None, None)
    fixture.Terminal.check(job)
    try:
        limits = BasicLimits()
        limits.LimitFlags, limits.ActiveProcessLimit = 0x8, 2
        fixture.Terminal.check(kernel.SetInformationJobObject(job, 2, ctypes.byref(limits), ctypes.sizeof(limits)))
        fixture.Terminal.check(kernel.AssignProcessToJobObject(job, kernel.GetCurrentProcess()))
    except BaseException:
        kernel.CloseHandle(job)
        raise
    return job


def console_child(binary, arguments):
    # Explicit console handles are required for Rust's cached stdio under ConPTY.
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                  ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.SetHandleInformation.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handles = [console_process_fence(kernel)]
    try:
        for name in ("CONIN$", "CONOUT$"):
            handle = kernel.CreateFileW(name, 0xC0000000, 3, None, 3, 0, None)
            if handle == wintypes.HANDLE(-1).value:
                raise ctypes.WinError(ctypes.get_last_error())
            fixture.Terminal.check(kernel.SetHandleInformation(handle, 1, 1))
            handles.append(handle)
        kernel.GetConsoleMode.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        kernel.SetConsoleMode.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        mode = wintypes.DWORD()
        fixture.Terminal.check(kernel.GetConsoleMode(handles[2], ctypes.byref(mode)))
        fixture.Terminal.check(kernel.SetConsoleMode(handles[2], mode.value | 0x0005))
        kernel.SetConsoleCP(65001)
        kernel.SetConsoleOutputCP(65001)
        startup = subprocess.STARTUPINFO()
        startup.dwFlags |= subprocess.STARTF_USESTDHANDLES
        startup.hStdInput = handles[1]
        startup.hStdOutput = startup.hStdError = handles[2]
        startup.lpAttributeList = {"handle_list": handles[1:]}
        return subprocess.call([str(binary), *arguments], startupinfo=startup, close_fds=True)
    finally:
        for handle in reversed(handles):
            kernel.CloseHandle(handle)


class Case:
    def __init__(self, root, name, binary, report):
        self.name, self.binary, self.report = name, binary, report
        self.root = root / name
        self.workspace = self.root / "workspace"
        self.workspace.mkdir(parents=True)
        self.env = support.isolated_environment(self.root)
        self.env.update({"AIZEN_NO_UPDATE_CHECK": "1", "TERM": "xterm-256color"})
        self.env.pop("NO_COLOR", None)
        self.home = Path(self.env["AIZEN_HOME"])
        self.home.mkdir()
        self.config = self.home / "cli-config.json"
        self.config.write_text(json.dumps({
            "base_url": "http://127.0.0.1:9/v1", "api_key": "fixture-not-a-credential",
            "model": "catalog-fixture", "onboarded": True, "approval_mode": "ask",
            "auto_effort": False, "architect_mode": False, "self_review": False,
            "auto_skill_learn": False, "persona_evolve": False, "update_check": False,
            "memory_auto_learn": False, "eager_tools": False,
        }), encoding="utf-8")
        self.runner = support.OwnedProcessRunner(self.root, self.workspace, self.env, 90)
        self.terminal = None
        self.frame = 0

    def cli(self, label, *arguments):
        # Only offline administration/consent and public metadata discovery call this helper.
        result = self.runner.run(label, [str(self.binary), *map(str, arguments)])
        self.report["commands"].append({"case": self.name, "label": label,
            **{k: v for k, v in result.items() if k not in ("stdout", "stderr")}})
        require(result["exit"] == 0, f"{self.name}/{label}: {result['stdout'][-1500:]} {result['stderr'][-2500:]}")
        require(result["job_settled"] and result["job_processes"] == 1,
                f"{self.name}/{label}: offline administration started another process")
        require(not self.runner.cleanup_failures, f"{self.name}: command cleanup failed")
        require(support.sha256(self.binary) == self.report["sha256"], "Core snapshot changed")
        return result["stdout"]

    def state(self):
        return json.loads(self.config.read_bytes()).get("memory_plugins", {
            "schema_version": 1, "generation": 0, "enabled": False, "selected": None, "packages": {}})

    def snapshot(self):
        return {"config": support.sha256(self.config), **{
            name: support.inventory(self.home / name)
            for name in ("plugins", "plugin-state", "plugin-temp", "memory-data", "cli-memory")}}

    def no_memory(self):
        for name in ("memory-data", "cli-memory"):
            require(not (self.home / name).exists(), f"{self.name}: {name} was initialized")

    def unchanged(self, baseline):
        require(self.snapshot() == baseline, f"{self.name}: cancel/browse/no-op changed config, pins, package or control/data files")
        self.no_memory()

    def start(self, retained=False):
        self.terminal = fixture.Terminal(Path(sys.executable), self.workspace, self.env,
            ["-I", str(Path(__file__).resolve()), "--console-child", str(self.binary),
             *([] if retained else ["config", "memory"])])
        if retained:
            self.terminal.wait("❯")
            threading.Event().wait(0.6)
            self.terminal.enter("/config", "/config")
            self.visible("config-hub", ["Config", "pick a section"])
            after = self.select(4, "Memory")
        else:
            after = 0
        self.visible("memory-hub", ["Provider", "Learning", "Storage"], after)
        after = self.select(0, "Provider")
        self.visible("provider", ["Provider", "Mnemos", "Import local package"], after)
        after = self.select(1, "Mnemos")
        self.setup("mnemos", after)

    def checkpoint(self):
        with self.terminal.changed:
            return len(self.terminal.data)

    def visible(self, label, markers, after=0, timeout=45, absent=()):
        deadline = time.monotonic() + timeout
        with self.terminal.changed:
            while True:
                raw = bytes(self.terminal.data)
                require(len(raw) <= 16 * 1024 * 1024, "ConPTY output exceeded fixture bound")
                screen = screen_text(raw)
                # Check the current viewport while the dialog is still OPEN. Never grep a
                # concatenated transcript after dismissal; retained rendering uses diffs.
                if len(raw) > after and all(m in screen for m in markers) and all(m not in screen for m in absent):
                    self.frame += 1
                    (self.root / f"{self.frame:02d}-{label}.txt").write_text(screen, encoding="utf-8")
                    return screen
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(f"{self.name}/{label}: missing visible {markers!r}; forbidden {absent!r}; screen={screen!r}")
                self.terminal.changed.wait(min(remaining, 0.2))

    def arrow(self, direction, selected=None):
        before = self.checkpoint()
        self.terminal.send(b"\x1b[B" if direction == "down" else b"\x1b[A")
        self.visible("arrow", [f"❯ {selected}"] if selected else [], before, timeout=10)
        threading.Event().wait(0.15)

    def select(self, index, label):
        for step in range(index):
            self.arrow("down", label if step == index - 1 else None)
        self.visible("selected-row", [f"❯ {label}"], timeout=10)
        # The synchronization checkpoint MUST follow the arrow redraw, not precede it.
        before = self.checkpoint()
        self.terminal.send(b"\r")
        return before

    def escape(self):
        before = self.checkpoint()
        self.terminal.send(b"\x1b")
        return before

    def setup(self, label, after, installed=None):
        if installed is None:
            packages = self.state()["packages"]
            installed = package_manifest(self.home)["version"] if "mnemos" in packages else "none"
        action = "Install local directory / ZIP" if installed == "none" else "Review provider activation (separate consent)"
        return self.visible(label, [f"Installed: {installed}", "List public Mnemos versions",
                                   "Check latest installable release", action, "Back"], after)

    def catalog(self, label, candidate, after):
        return self.visible(label, ["Mnemos public versions", REPOSITORY,
            "1 stable version(s) · complete", f"Target: {TARGET}",
            f"{candidate['version']} · package metadata present", "Refresh", "Back"], after)

    def review(self, label, candidate, after, old=None):
        version = candidate["version"]
        operation = f"Update Mnemos {old} -> {version}?" if old else f"Download and install Mnemos {version}?"
        consequences = (["Preserves settings, budgets, selection and data location.",
                         "Disables Mnemos; revokes execution and Learning consent.",
                         "If selected, Memory is disabled. Activation is required again."] if old else
                        ["Installs disabled, unselected and without execution consent.",
                         "Activation remains a separate choice."])
        return self.visible(label, [operation, f"Source: {REPOSITORY}", f"Target: {TARGET}",
            f"Archive: {candidate['filename']}", f"Size: {candidate['bytes']} bytes",
            f"SHA256: {candidate['sha256']}", "Hashes verify integrity, not publisher trust.",
            *consequences, "No provider execution, data transfer or migration.", "❯ No / cancel", "Yes"], after)

    def result(self, label, markers, after, timeout=45):
        screen = self.visible(label, [*markers, "❯ Back"], after, timeout=timeout,
                              absent=("No / cancel", "❯ Yes"))
        require(not any(line.strip() in ("Yes", "❯ Yes", "No / cancel", "❯ No / cancel")
                        for line in screen.splitlines()), f"{self.name}: result is not Back-only")

    def success(self, candidate, after):
        self.result("installed-successfully", [f"Mnemos {candidate['version']} installed successfully.",
            "Activation is a separate choice. No provider was executed."], after, timeout=330)

    def close(self):
        if self.terminal is not None:
            try:
                self.terminal.close()
            finally:
                (self.root / "terminal.raw").write_bytes(bytes(self.terminal.data))
                (self.root / "terminal-final.txt").write_text(screen_text(bytes(self.terminal.data)), encoding="utf-8")


def package_manifest(home):
    path = home / "plugins" / "mnemos" / "plugin.json"
    require(path.stat().st_size <= 128 * 1024, "oversized installed manifest")
    return json.loads(path.read_bytes())


def verify_package(case, version):
    state = case.state()
    require(set(state["packages"]) == {"mnemos"}, "unexpected package registry")
    package = state["packages"]["mnemos"]
    pins = package["pins"]
    directory = case.home / "plugins" / "mnemos"
    support.inventory(directory)  # Refuse links/reparse points before reading package metadata.
    manifest = package_manifest(case.home)
    require(manifest["id"] == pins["id"] == "mnemos" and manifest["version"] == version,
            "installed manifest identity/version mismatch")
    require(manifest["target"] == pins["target"] == TARGET, "wrong installed target")
    require(pins["schema_version"] == 1 and pins["manifest"]["path"] == "plugin.json", "bad host pins")
    require(json.loads((directory / ".install-pins.json").read_bytes()) == pins, "host receipt differs from pins")
    require({p["path"]: (p["bytes"], p["sha256"]) for p in manifest["files"]}
            == {p["path"]: (p["bytes"], p["sha256"]) for p in pins["files"]}, "manifest files differ from host pins")
    for pin in [pins["manifest"], *pins["files"]]:
        path = (directory / pin["path"]).resolve(strict=True)
        require(path.is_relative_to(directory.resolve()), "installed pin escaped package")
        require(re.fullmatch(r"[0-9a-f]{64}", pin["sha256"]) is not None, "truncated/noncanonical pin")
        require(path.stat().st_size == pin["bytes"] and support.sha256(path) == pin["sha256"], "installed bytes differ from pins")
    case.no_memory()
    return state, manifest


def fresh_case(case, retained, candidate):
    try:
        case.start(retained)
        baseline = case.snapshot()
        generation = case.state()["generation"]
        case.catalog("catalog", candidate, case.select(0, "List public Mnemos versions"))
        # Separate default Enter, explicit No after visiting Yes, and Escape cases.
        for dismissal in ("default-no", "explicit-no", "escape"):
            case.review("review-" + dismissal, candidate, case.select(0, candidate["version"]))
            case.unchanged(baseline)  # The complete review is visible before any consent key.
            if dismissal == "explicit-no":
                case.arrow("down", "Yes")
                case.arrow("up", "No / cancel")
            after = case.escape() if dismissal == "escape" else case.select(0, "No / cancel")
            case.catalog("catalog-after-" + dismissal, candidate, after)
            case.unchanged(baseline)
        case.catalog("refreshed", candidate, case.select(1, "Refresh"))
        case.setup("catalog-back", case.select(2, "Back"), "none")
        case.review("latest-review", candidate, case.select(1, "Check latest installable release"))
        case.unchanged(baseline)
        case.setup("latest-cancel", case.select(0, "No / cancel"), "none")
        case.unchanged(baseline)
        case.catalog("catalog-before-install", candidate, case.select(0, "List public Mnemos versions"))
        # Catalog Escape is independent from review Escape and must return to setup.
        case.setup("catalog-escape", case.escape(), "none")
        case.unchanged(baseline)
        case.catalog("catalog-install", candidate, case.select(0, "List public Mnemos versions"))
        case.review("install-review", candidate, case.select(0, candidate["version"]))
        case.unchanged(baseline)
        case.success(candidate, case.select(1, "Yes"))
        state, manifest = verify_package(case, candidate["version"])
        package = state["packages"]["mnemos"]
        require(state["generation"] > generation and state["enabled"] is False
                and state["selected"] is None and package["enabled"] is False and package["grant"] is None,
                "fresh install activated, selected or granted execution")
        (case.root / "installed-state.json").write_text(json.dumps(state, indent=2), encoding="utf-8")
        case.catalog("success-back-to-catalog", candidate, case.select(0, "Back"))
        installed = case.snapshot()
        case.result("same-version-refused", ["Mnemos release not completed",
            f"Already installed: Mnemos {candidate['version']}. No replacement needed"],
            case.select(0, candidate["version"]))
        case.unchanged(installed)
        case.catalog("same-version-back", candidate, case.select(0, "Back"))
        case.unchanged(installed)
        case.setup("installed-menu", case.select(2, "Back"), candidate["version"])
        case.unchanged(installed)
        # Setup row 3 is Back regardless of whether row 2 is import or activation.
        case.visible("setup-back", ["Provider", "Learning", "Storage"], case.select(3, "Back"))
        case.unchanged(installed)
        case.report["cases"].append({"surface": case.name, "passed": True,
            "default_no_explicit_no_escape_unchanged": True, "visible_review_metadata": True,
            "catalog_refresh_back_escape": True, "latest_cancel_returns_setup": True,
            "fresh_install_disabled_unselected_ungranted": True, "same_version_no_mutation": True,
            "installed_version": manifest["version"], "pins": package["pins"]})
        print("PASS", case.name, flush=True)
        return manifest, package["pins"]
    finally:
        case.close()


def seed_old_version(case, public_manifest):
    # Retain one real compatible setting, with a nondefault override; do not invent a
    # key that the new manifest would rightly reject at the update boundary.
    choice = next(((key, definition) for key, definition in public_manifest.get("settings", {}).items()
                   if definition["type"] == "boolean"), None)
    require(choice is not None, "public package has no boolean fixture setting; update fixture deliberately")
    key, definition = choice
    value = not definition["default"]
    capabilities = ["records", "recall", "context", "learning"]
    require(set(capabilities).issubset(public_manifest["capabilities"]), "public capabilities changed; review update fixture")
    source = case.root / "old-package-source"
    source.mkdir()
    inert = source / "provider.exe"
    shutil.copyfile(case.binary, inert)
    require(support.sha256(inert) == case.report["sha256"], "inert Core copy changed")
    with inert.open("rb") as stream:
        require(stream.read(2) == b"MZ", "fixture needs the trusted native PE Core binary")
    manifest = {"schema_version": 1, "id": "mnemos", "name": "Synthetic Mnemos update fixture",
        "version": OLD_VERSION, "kind": "memory", "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
        "target": TARGET, "entrypoint": "provider.exe", "capabilities": capabilities,
        "permissions": {"network": False, "host_llm": False}, "settings": {key: definition},
        "files": [{"path": "provider.exe", "bytes": inert.stat().st_size, "sha256": support.sha256(inert)}]}
    (source / "plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
    case.cli("import-synthetic", "plugin", "install", source)
    case.cli("set-options-budgets", "plugin", "configure", "mnemos", "--set", f"{key}={json.dumps(value)}",
             "--context-tokens", "600", "--recall-tokens", "400", "--recall-inject-tokens", "200",
             "--recall-limit", "5", "--max-records", "20")
    data = case.root / "synthetic-data-location"
    case.cli("set-storage", "plugin", "storage", "mnemos", "--path", data)
    require(not data.exists(), "offline storage initialized provider data")
    data.mkdir()
    (data / "preserve.txt").write_bytes(b"Synthetic preservation sentinel; not Memory records.\n")
    case.cli("learning-policy", "config", "set", "--memory-auto-learn", "true")
    case.cli("eligible-synthetic", "plugin", "enable", "mnemos")
    # memory_control::Use only reviews/apply_grant; it never executes a provider.
    # OwnedProcessRunner asserts ONE process for this command and every seed command.
    case.cli("consent-synthetic-only", "memory", "use", "mnemos", "--yes", "--allow-guarded", "--allow-learning")
    state, _ = verify_package(case, OLD_VERSION)
    saved = state["packages"]["mnemos"]
    require(state["selected"] == "mnemos" and state["enabled"] is True and saved["enabled"] is True,
            "synthetic selection was not seeded")
    require(saved["grant"]["learning"] is True and saved["grant"]["guarded_execution"] is True
            and saved["grant"]["manifest_sha256"] == saved["pins"]["manifest"]["sha256"], "synthetic consent missing")
    require(saved["settings"] == {key: value} and saved["budgets"] == {
        "context_tokens": 600, "recall_tokens": 400, "recall_inject_tokens": 200, "recall_limit": 5, "max_records": 20},
        "synthetic nondefault settings/budgets were not saved")
    return state, source, data


def update_case(case, candidate, public_manifest, public_pins):
    try:
        before, source, data = seed_old_version(case, public_manifest)
        source_before, data_before = support.inventory(source), support.inventory(data)
        config_before = json.loads(case.config.read_bytes())
        baseline = case.snapshot()
        (case.root / "before-update.json").write_text(json.dumps(before, indent=2), encoding="utf-8")
        # Direct config avoids REPL startup for this selected SYNTHETIC package.
        case.start()
        case.catalog("update-catalog", candidate, case.select(0, "List public Mnemos versions"))
        case.review("update-review", candidate, case.select(0, candidate["version"]), old=OLD_VERSION)
        case.unchanged(baseline)
        case.success(candidate, case.select(1, "Yes"))
        after, _ = verify_package(case, candidate["version"])
        old, new = before["packages"]["mnemos"], after["packages"]["mnemos"]
        require(after["generation"] == before["generation"] + 1, "update did not advance exactly one generation")
        require(new["pins"] != old["pins"] and new["pins"] == public_pins, "update failed to replace exact pinned package")
        require(all(new.get(field) == old.get(field) for field in ("settings", "budgets", "data_root")),
                "update lost settings, budgets or data location")
        require(after["selected"] == before["selected"] == "mnemos" and after["enabled"] is False
                and new["enabled"] is False and new["grant"] is None,
                "update failed to preserve selection, disable Memory or revoke execution/Learning consent")
        config_after = json.loads(case.config.read_bytes())
        require({k: v for k, v in config_after.items() if k != "memory_plugins"}
                == {k: v for k, v in config_before.items() if k != "memory_plugins"}, "update changed unrelated config/Learning policy")
        require(support.inventory(source) == source_before and support.inventory(data) == data_before,
                "update changed synthetic source or preservation sentinel")
        (case.root / "after-update.json").write_text(json.dumps(after, indent=2), encoding="utf-8")
        case.catalog("update-success-back", candidate, case.select(0, "Back"))
        case.setup("updated-menu", case.select(2, "Back"), candidate["version"])
        case.report["cases"].append({"surface": case.name, "passed": True,
            "synthetic_old_version": OLD_VERSION, "public_new_version": candidate["version"],
            "not_a_two_public_releases_upgrade": True, "visible_review_metadata": True,
            "generation_advance": True, "pins_changed": True, "settings_budgets_selection_storage_preserved": True,
            "disabled_execution_and_learning_consent_revoked": True, "sentinel_unchanged": True})
        print("PASS", case.name, flush=True)
    finally:
        case.close()


def run(binary, expected_version, expected_sha256=None, expected_bytes=None):
    require(binary.is_file(), "a prebuilt trusted Core executable is required")
    require(re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", expected_version),
            "expected version must be canonical X.Y.Z (without v)")
    require(tuple(map(int, expected_version.split("."))) > (0, 0, 1), "public version must be newer than synthetic 0.0.1")
    root = Path(tempfile.mkdtemp(prefix="aizen-mnemos-catalog-tui-"))
    report = {"binary_source": str(binary), "sha256": support.sha256(binary), "cases": [], "commands": [],
        "passed": False, "limitations": ["Windows ConPTY, not a physical terminal/IME test.",
            "Live anonymous GitHub channel; expects exactly one stable public version and fails closed on drift/rate limits.",
            "Update starts from synthetic 0.0.1 containing an inert Core copy, not an older public Mnemos release.",
            "Public packages are never activated. No provider commands or actual Memory data are used."]}
    print("EVIDENCE", root, flush=True)
    try:
        runtime = root / "trusted-runtime"
        runtime.mkdir()
        core = runtime / "aizen.exe"
        shutil.copyfile(binary, core)
        require(support.sha256(core) == report["sha256"] == support.sha256(binary), "Core changed during snapshot")
        report["binary"] = str(core)
        discovery = Case(root, "discovery", core, report)
        before = discovery.snapshot()
        # CLI Releases has no --version flag. Check once, validate the expected sole
        # stable candidate, and reuse its full digest/size/name for all UI assertions.
        # No guessed/truncated live SHA and no production HTTP endpoint override.
        status = json.loads(discovery.cli("public-check", "plugin", "releases", "mnemos", "--json"))
        candidate = status.get("candidate")
        require(status["repository"] == REPOSITORY and status["target"] == TARGET
                and status["discovery"] == "complete" and status["installed"] is None
                and status["highest_seen_version"] == expected_version and isinstance(candidate, dict),
                f"unexpected/incomplete public discovery: {status!r}")
        require(candidate["version"] == expected_version and candidate["target"] == TARGET
                and candidate["filename"] == f"mnemos-{expected_version}-{TARGET}.zip", "candidate identity changed")
        require(isinstance(candidate["bytes"], int) and 0 < candidate["bytes"] <= 272 * 1024 * 1024,
                "invalid candidate size")
        for field in ("sha256", "checksums_sha256"):
            require(re.fullmatch(r"[0-9a-f]{64}", candidate[field]) is not None, f"invalid/truncated {field}")
        if expected_sha256 is not None:
            require(re.fullmatch(r"[0-9a-f]{64}", expected_sha256) is not None
                    and candidate["sha256"] == expected_sha256, "public archive differs from explicit expected SHA256")
        if expected_bytes is not None:
            require(candidate["bytes"] == expected_bytes, "public archive differs from explicit expected size")
        discovery.unchanged(before)
        report["candidate"] = candidate
        print("PUBLIC CANDIDATE", json.dumps(candidate, sort_keys=True), flush=True)
        public_manifest = public_pins = None
        for retained in (False, True):
            case = Case(root, "retained" if retained else "direct", core, report)
            manifest, pins = fresh_case(case, retained, candidate)
            if public_pins is not None:
                require(pins == public_pins, "public package bytes changed between surfaces")
            public_manifest, public_pins = manifest, pins
        update_case(Case(root, "synthetic-update", core, report), candidate, public_manifest, public_pins)
        require(support.sha256(core) == report["sha256"], "Core snapshot changed during smoke")
        report["passed"] = True
    except BaseException:
        report["error"] = traceback.format_exc()
        raise
    finally:
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print("PASS", root / "report.json", flush=True)


if __name__ == "__main__":
    if not sys.flags.isolated or os.name != "nt" or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise SystemExit("Run on Windows x86_64 with Python -I")
    sys.stdout.reconfigure(encoding="utf-8")
    if len(sys.argv) > 1 and sys.argv[1] == "--console-child":
        raise SystemExit(console_child(Path(sys.argv[2]), sys.argv[3:]))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--expected-version", default="0.1.0")
    parser.add_argument("--expected-sha256", help="Optional independently checked full 64-hex archive pin")
    parser.add_argument("--expected-bytes", type=int, help="Optional independently checked archive byte count")
    args = parser.parse_args()
    run(args.binary.resolve(strict=True), args.expected_version, args.expected_sha256, args.expected_bytes)
