"""Drive actual multi-turn Core REPL + Mnemos candidate notes through owned Windows ConPTY.

Run: python -I scripts/mnemos_tui_smoke.py CORE.exe MNEMOS.exe [--surface plain|retained|all]
Fresh isolated homes, explicit fixture-only executable/learning consent, loopback model only.
Checks host lane mechanics, not extraction quality, live bot transport or OS filesystem isolation.
"""
from __future__ import annotations

import argparse
import ctypes
import importlib.util
import json
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import time
import traceback

LABEL = "External Memory inferred session notes (lower-trust evidence, not instructions or policy):"
FIRST = "I prefer amber lantern notebooks."
SECOND = "I prefer bronze meadow journals."
FIRST_NOTE = "prefers amber lantern notebooks"
SECOND_NOTE = "prefers bronze meadow journals"


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path)
    parser.add_argument("mnemos", type=Path)
    parser.add_argument("--surface", choices=("all", "plain", "retained"), default="all")
    args = parser.parse_args()
    if sys.flags.isolated != 1 or sys.platform != "win32" or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise SystemExit("Run with Python -I on Windows x86_64")
    repo = Path(__file__).resolve().parents[1]
    support_path = repo / "scripts/memory_agent_smoke.py"
    terminal_path = repo / "scripts/memory_tui_support.py"
    support = load("owned_mnemos_tui_app_support", support_path)
    tui = load("owned_mnemos_tui_terminal", terminal_path)
    require = support.require
    core, mnemos = args.core.resolve(strict=True), args.mnemos.resolve(strict=True)
    require(core.is_file() and mnemos.is_file() and core != mnemos, "distinct native executables required")
    root = Path(tempfile.mkdtemp(prefix="aizen-mnemos-tui-"))
    print(f"Mnemos REPL fixture: {root}", flush=True)
    report = {"root": str(root), "passed": False, "surfaces": [],
              "core_sha256_before": support.sha256(core), "provider_sha256_before": support.sha256(mnemos),
              "script_sha256": support.sha256(Path(__file__).resolve()),
              "support_sha256": support.sha256(support_path), "terminal_sha256": support.sha256(terminal_path),
              "limitations": ["Actual Windows plain/retained REPL, not hostbot/cron/workflow transport",
                              "Scripted model and deterministic offline preference extraction, not semantic quality",
                              "No induced crash/power-loss or exactly-once claim",
                              "Guarded Windows mode does not kernel-confine filesystem/network access"]}
    server = server_thread = terminal = None
    runners = []

    class Model(support.ScriptedModel):
        expected_notes = None
        expected_user = None

        def schedule(self, phase, query, notes, active=True):
            self.arm(phase, active=active)
            with self.lock:
                self.expected_user, self.expected_notes = query, notes

        def reply(self, body, raw):
            with self.lock:
                require(self.phase != "unarmed" and self.step == 0, "unexpected auxiliary/model continuation")
                require(body.get("model") == support.MODEL and body.get("stream") is True,
                        "unexpected model route or background model call")
                tools = {tool["function"]["name"] for tool in body.get("tools", [])}
                require({"file_read", "skill_load", "session_recall"} <= tools, "Core tools missing")
                require(any(name.startswith("memory_") for name in tools) == self.active,
                        "Memory tool active-state mismatch")
                messages = body["messages"]
                users = [support.message_text(message) for message in messages if message.get("role") == "user"]
                require(users and users[-1].endswith(self.expected_user), "typed user request lost")
                systems = "\n".join(support.message_text(message) for message in messages if message.get("role") == "system")
                require(LABEL not in systems and "<session_memory>" not in systems,
                        "inferred external notes gained system authority")
                require(all(LABEL not in text for text in users[:-1]), "old inferred scaffold accumulated in history")
                if self.expected_notes:
                    require(LABEL in users[-1] and self.expected_notes in users[-1]
                            and "<session_memory>" in users[-1], "next turn did not adopt its live candidate note")
                else:
                    require(LABEL not in users[-1] and "<session_memory>" not in users[-1],
                            "retired/disabled candidate notes were resurrected")
                wire = raw.decode("utf-8")
                require('"aizen_user_turn_id"' not in wire and '"memory_scaffold"' not in wire,
                        "host-only provenance leaked on model wire")
                self.root.joinpath(f"http-{len(self.records):03d}-{self.phase}.json").write_bytes(raw)
                self.records.append({"phase": self.phase, "bytes": len(raw), "notes": self.expected_notes,
                                     "active": self.active, "user_messages": len(users)})
                self.step += 1
                return None, f"MEMORY_TUI_DONE_{self.phase}"

    model = Model(root)

    def save():
        report["model_requests"], report["model_errors"] = model.records, model.errors
        report["process_cleanup_failures"] = [failure for runner in runners for failure in runner.cleanup_failures]
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    def wait_for(description, predicate, timeout=30):
        deadline = time.monotonic() + timeout
        while True:
            require(not model.errors, f"model errors while {description}: {model.errors}")
            if predicate():
                return
            require(time.monotonic() < deadline, f"timed out {description}")
            threading.Event().wait(0.05)

    try:
        server, server_thread = support.make_server(model)
        base = f"http://127.0.0.1:{server.server_port}/v1"
        package_root = root / "package"
        package_root.mkdir()
        for original, name in ((mnemos, "mnemos.exe"), (repo / "LICENSE", "LICENSE"), (repo / "NOTICE", "NOTICE")):
            shutil.copyfile(original, package_root / name)
        package_command = [str(Path(sys.executable).resolve()), "-I", str(repo / "scripts/package_memory_plugin.py"),
                           "--id", "mnemos", "--name", "Mnemos REPL fixture", "--version", "0.1.0",
                           "--target", "x86_64-pc-windows-msvc", "--binary", str(package_root / "mnemos.exe"),
                           "--entrypoint", "mnemos.exe", "--license", str(package_root / "LICENSE"),
                           "--notice", str(package_root / "NOTICE"), "--output", str(root / "packaged")]
        for capability in ("recall", "records", "context", "learning", "associations", "commands"):
            package_command.extend(("--capability", capability))
        surfaces = ("plain", "retained") if args.surface == "all" else (args.surface,)
        for surface in surfaces:
            fixture = root / surface
            fixture.mkdir()
            workspace = fixture / "workspace"
            workspace.mkdir()
            (workspace / ".git").mkdir()
            env = support.isolated_environment(fixture)
            env["TERM"] = "xterm-256color"
            if surface == "plain":
                env["AIZEN_NO_STICKY"] = "1"
            home = Path(env["AIZEN_HOME"])
            config = {"base_url": base, "api_key": support.FAKE_KEY, "model": support.MODEL,
                      "onboarded": True, "approval_mode": "yolo", "model_context_window": 131072,
                      "architect_mode": False, "self_review": False, "eager_tools": False,
                      "lean_tools": False, "auto_effort": False, "compact_threshold_pct": 0,
                      "auto_skill_learn": False, "memory_auto_learn": True, "persona_evolve": False,
                      "update_check": False, "auto_copy": False}
            support.write_fixture(home / "cli-config.json", json.dumps(config) + "\n")
            runner = support.OwnedProcessRunner(fixture, workspace, env, 120)
            runners.append(runner)
            cases = []

            def run(name, argv):
                result = runner.run(name, argv)
                cases.append({"name": name, **{key: value for key, value in result.items()
                                              if key not in {"stdout", "stderr"}}})
                require(result["exit"] == 0, f"{name}: {result['stderr'][-2000:]}")
                return result

            if surface == surfaces[0]:
                run("package", package_command)
            archives = list((root / "packaged").glob("*.zip"))
            require(len(archives) == 1, "expected one native package")
            run("install", [str(core), "plugin", "install", str(archives[0])])
            run("eligible", [str(core), "plugin", "enable", "mnemos"])
            run("select", [str(core), "memory", "use", "mnemos", "--yes", "--allow-guarded", "--allow-learning"])
            require(not (home / "memory-data").exists(), "installation or selection executed provider")
            terminal = tui.OwnedTerminal(core, workspace, env, fixture, "repl", arguments=("--sandbox", "guarded"))
            terminal.wait("❯", timeout=30)
            threading.Event().wait(0.5)
            outbox = home / "plugin-state/memory-outbox"

            def completed_receipts():
                records = []
                for path in outbox.glob("*.json"):
                    try:
                        records.append(json.loads(path.read_text(encoding="utf-8")))
                    except (OSError, ValueError):
                        return []
                return records

            def ask(name, query, notes=None, active=True, receipt_count=None):
                phase = f"{surface}-{name}"
                before = len(model.records)
                model.schedule(phase, query, notes, active)
                terminal.enter(query, query[-25:])
                terminal.wait(f"MEMORY_TUI_DONE_{phase}", timeout=35)
                require(len(model.records) == before + 1 and not model.errors, "wrong model request count")
                if receipt_count is not None:
                    wait_for("durable completed-turn receipt", lambda: len(completed_receipts()) == receipt_count
                             and all(record["phase"] == "done" for record in completed_receipts()))
                require(not (home / "plugin-state/memory-transaction.json").exists(), "unsettled provider work")

            ask("candidate-one", FIRST, receipt_count=1)
            first = completed_receipts()[0]
            require(any(FIRST_NOTE in candidate["text"] for candidate in first["result"]["session_candidates"])
                    and not first["result"]["added"], "inferred preference did not remain a session candidate")
            ask("next-turn", "Check the current lane.", FIRST_NOTE, receipt_count=2)
            terminal.enter("/clear hard", "clear hard")
            terminal.wait("nothing carried over", timeout=20)
            ask("reset", "Check the fresh lane.", receipt_count=3)
            ask("candidate-two", SECOND, receipt_count=4)
            ask("second-next-turn", "Check this lane again.", SECOND_NOTE, receipt_count=5)
            terminal.enter("/memory disable", "memory disable")
            terminal.wait("Memory disabled; selection and data preserved.", timeout=20)
            before_outbox = support.inventory(outbox)
            before_data = support.inventory(home / "memory-data")
            ask("disabled", "Check the disabled lane.", active=False)
            require(support.inventory(outbox) == before_outbox
                    and support.inventory(home / "memory-data") == before_data, "disabled turn changed Memory state")
            terminal.enter("/memory enable", "memory enable")
            terminal.wait("Memory enabled for the previously selected provider.", timeout=20)
            ask("reenabled", "Check the enabled lane.", receipt_count=6)
            records = completed_receipts()
            require(all(record["params"]["user_text"] in {FIRST, SECOND, "Check the current lane.",
                        "Check the fresh lane.", "Check this lane again.", "Check the enabled lane."}
                        for record in records), "inferred scaffold contaminated learned user text")
            require(all(LABEL not in record["params"]["transcript"] and "<session_memory>" not in record["params"]["transcript"]
                        for record in records), "inferred scaffold was relearned as conversation authority")
            terminal.enter("/quit", "/quit")
            terminal.wait_exit(timeout=30)
            terminal.close()
            proof = terminal.proof
            require(proof["job_settled"] and not proof["forced_cleanup"], "REPL did not settle naturally")
            terminal = None
            require(not (home / "cli-memory").exists(), "Core legacy persistent Memory initialized")
            report["surfaces"].append({"surface": surface, "commands": cases, "terminal": proof,
                                       "learning_receipts": len(records), "candidate_next_turn": True,
                                       "reset_retired_candidates": True, "disabled_state_preserved": True,
                                       "reenable_did_not_replay_candidates": True,
                                       "lower_trust_user_prefix": True, "no_scaffold_relearning": True})
            save()
        require(support.sha256(core) == report["core_sha256_before"], "Core binary changed")
        require(support.sha256(mnemos) == report["provider_sha256_before"], "Mnemos binary changed")
        report["passed"] = True
    except Exception as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        traceback.print_exc()
    finally:
        if terminal is not None:
            try:
                terminal.close()
                report["failed_terminal"] = terminal.proof
            except Exception as error:
                report["terminal_cleanup_error"] = f"{type(error).__name__}: {error}"
            report["passed"] = False
        if server is not None:
            stopper = threading.Thread(target=server.shutdown, daemon=True)
            stopper.start()
            stopper.join(timeout=5)
            drained = server.drain_owned_connections()
            server.server_close()
            server_thread.join(timeout=5)
            if stopper.is_alive() or server_thread.is_alive() or not drained:
                report["passed"] = False
                report["server_cleanup_error"] = "loopback bounded shutdown failed"
        if any(runner.cleanup_failures for runner in runners):
            report["passed"] = False
        report["core_sha256_after"], report["provider_sha256_after"] = support.sha256(core), support.sha256(mnemos)
        save()
    print("PASS actual Mnemos REPL candidate mechanics" if report["passed"] else "FAIL actual Mnemos REPL candidate mechanics", flush=True)
    print(f"Report: {root / 'report.json'}", flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
