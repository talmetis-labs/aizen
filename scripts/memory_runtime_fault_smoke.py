"""Fault the actual Core Memory runtime with an independent, deliberately faulty executable.

Usage: python -I scripts/memory_runtime_fault_smoke.py CORE.exe FAULT_FIXTURE.exe
Each mode gets a fresh owned HOME/package/data/workspace. No real configuration, store, model,
network, installation, or process is used. All commands run in bounded suspended-create Jobs.
The supplied fault binary is explicit trusted TEST code, not an installable Memory engine.
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
import traceback

MODES = ("conflict", "pipe-child", "ambiguous", "recovery", "crash", "mismatch", "noise",
         "truncated", "oversized", "stderr-flood", "stall")
SETTLED = {"conflict", "pipe-child"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path)
    parser.add_argument("fixture", type=Path)
    args = parser.parse_args()
    if sys.flags.isolated != 1 or sys.platform != "win32" or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise SystemExit("Run with Python -I on Windows x86_64")
    repo = Path(__file__).resolve().parents[1]
    support_path = repo / "scripts/memory_agent_smoke.py"
    spec = importlib.util.spec_from_file_location("owned_memory_fault_support", support_path)
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    require, sha256 = support.require, support.sha256
    core, fixture = args.core.resolve(strict=True), args.fixture.resolve(strict=True)
    require(core.is_file() and fixture.is_file() and core != fixture, "two distinct fixture executables required")
    root = Path(tempfile.mkdtemp(prefix="aizen-memory-runtime-faults-"))
    report = {"root": str(root), "passed": False, "cases": [],
              "core_sha256_before": sha256(core), "fixture_sha256_before": sha256(fixture),
              "script_sha256": sha256(Path(__file__).resolve()), "support_sha256": sha256(support_path),
              "limitations": ["Windows guarded mode only; no kernel filesystem/network isolation claim",
                              "Actual CLI transactions, not actor panic/queue cancellation injection",
                              "No live model, engine parity, admin recovery resolution or non-Windows coverage"]}
    print(f"Runtime fault fixtures: {root}", flush=True)

    def save():
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    for mode in MODES:
        owned = root / mode
        owned.mkdir()
        workspace = owned / "workspace"
        workspace.mkdir()
        (workspace / ".git").mkdir()
        env = support.isolated_environment(owned)
        aizen = Path(env["AIZEN_HOME"])
        runner = support.OwnedProcessRunner(owned, workspace, env, 90)
        case = {"mode": mode, "passed": False, "commands": []}
        report["cases"].append(case)

        def run(name, *arguments, success=True, program=core):
            result = runner.run(name, [str(program), *map(str, arguments)])
            case["commands"].append({"name": name, **{key: value for key, value in result.items()
                                                      if key not in {"stdout", "stderr"}}})
            save()
            require((result["exit"] == 0) == success,
                    f"{mode}/{name}: exit {result['exit']}; {result['stderr'][-1800:]}")
            return result

        try:
            source = owned / "source"
            source.mkdir()
            for original, name in ((fixture, mode + ".exe"), (repo / "LICENSE", "LICENSE"), (repo / "NOTICE", "NOTICE")):
                shutil.copyfile(original, source / name)
            package = owned / "packaged"
            run("package", "-I", repo / "scripts/package_memory_plugin.py", "--id", "fault-memory",
                "--name", "Synthetic runtime fault fixture", "--version", "0.1.0",
                "--target", "x86_64-pc-windows-msvc", "--binary", source / (mode + ".exe"),
                "--entrypoint", mode + ".exe", "--license", source / "LICENSE", "--notice", source / "NOTICE",
                "--capability", "records", "--output", package, program=Path(sys.executable).resolve())
            archives = list(package.glob("*.zip"))
            require(len(archives) == 1, "expected one local fixture archive")
            run("install", "plugin", "install", archives[0])
            run("eligible", "plugin", "enable", "fault-memory")
            run("select", "memory", "use", "fault-memory", "--yes", "--allow-guarded")
            require(not (aizen / "memory-data").exists(), "selection executed fixture")
            result = run("operation", "--sandbox", "guarded", "memory", "add", "synthetic-fault",
                         "--body", "Synthetic owned fault fixture payload", success=False)
            require(result["job_processes"] >= (3 if mode == "pipe-child" else 2), "fixture did not execute owned child")
            data = aizen / "memory-data/fault-memory"
            events = (data / "events.log").read_text(encoding="utf-8").splitlines()
            require(events.count("initialize") == events.count("store") == 1, "operation was replayed")
            operation_id = (data / "operation-id.txt").read_text(encoding="utf-8")
            require(bool(operation_id), "mutation lacks operation ID")
            marker = aizen / "plugin-state/memory-transaction.json"
            status = json.loads(run("status", "memory", "status", "--json")["stdout"])
            if mode in SETTLED:
                require(events.count("shutdown") == 1 and "effect" not in events, "conflict control had effects or missed shutdown")
                require(not marker.exists() and not status["recovery_required"], "clean conflict did not retire settled receipt")
                require("Conflict" in result["stderr"], "semantic conflict was replaced with transport ambiguity")
                run("disable-settled", "memory", "disable")
                require(not json.loads(run("disabled-status", "memory", "status", "--json")["stdout"])["enabled"],
                        "settled transaction kept activation locked")
                if mode == "pipe-child":
                    require(events.count("pipe-child") == 1, "descendant/pipe fixture not reached")
            else:
                require(events.count("effect") == 1, "fault did not cross effect boundary")
                require(marker.is_file() and status["recovery_required"] and status["state"] == "recovery_required",
                        "unresolved outcome lost durable recovery barrier")
                receipt = json.loads(marker.read_text(encoding="utf-8"))
                require(receipt["provider"] == "fault-memory" and receipt["operation_id"] == operation_id
                        and receipt["method"] == "store" and len(receipt["params_sha256"]) == 64,
                        "recovery receipt lost original operation identity")
                require("content" not in receipt and "body" not in receipt, "recovery marker retained raw payload")
                before = support.inventory(data)
                marker_before = sha256(marker)
                run("blocked-replay", "--sandbox", "guarded", "memory", "add", "synthetic-fault",
                    "--body", "Synthetic owned fault fixture payload", success=False)
                run("blocked-disable", "memory", "disable", success=False)
                run("blocked-uninstall", "plugin", "uninstall", "fault-memory", "--yes", success=False)
                require(support.inventory(data) == before and sha256(marker) == marker_before,
                        "blocked operation replayed, retired marker or changed data")
                require((aizen / "plugins/fault-memory").is_dir(), "unresolved package was uninstalled")
                if mode in {"ambiguous", "recovery"}:
                    require(events.count("shutdown") == 1, "semantic uncertain error skipped clean shutdown")
            require(not runner.cleanup_failures, "fixture cleanup settlement unproven")
            require(sha256(core) == report["core_sha256_before"] and sha256(fixture) == report["fixture_sha256_before"],
                    "input executable changed")
            case.update({"passed": True, "events": events, "operation_id": operation_id,
                         "recovery_required": mode not in SETTLED})
        except Exception as error:
            case["failure"] = f"{type(error).__name__}: {error}"
            traceback.print_exc()
        finally:
            case["process_cleanup_failures"] = runner.cleanup_failures
            save()
        print(f"{'PASS' if case['passed'] else 'FAIL'} {mode}", flush=True)
        if runner.cleanup_failures:
            break
    report["core_sha256_after"] = sha256(core)
    report["fixture_sha256_after"] = sha256(fixture)
    report["passed"] = (len(report["cases"]) == len(MODES) and all(case["passed"] for case in report["cases"])
                        and report["core_sha256_after"] == report["core_sha256_before"]
                        and report["fixture_sha256_after"] == report["fixture_sha256_before"])
    save()
    print(f"{'PASS' if report['passed'] else 'FAIL'}: {root / 'report.json'}", flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
