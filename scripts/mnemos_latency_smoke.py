"""Measure actual release Core status/health/search with an explicitly supplied Mnemos ZIP.

Run with Python -I, --core CORE.exe --mnemos MNEMOS.exe --package PACKAGE.zip.
Installation and guarded execution are restricted to a fresh fixture HOME. No model or network.
Times cover each subprocess from creation through exit, not isolated engine or IPC time.
"""
from __future__ import annotations

import argparse
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time
import traceback


def support_module():
    spec = importlib.util.spec_from_file_location(
        "mnemos_latency_support", Path(__file__).with_name("memory_agent_smoke.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def summarize(samples):
    return {"samples_ms": samples, "median_ms": statistics.median(samples),
            "min_ms": min(samples), "max_ms": max(samples)}


def worker(spec_path: Path) -> int:
    support = support_module()
    require = support.require
    spec = json.loads(spec_path.read_text(encoding="utf-8"))
    root, core = spec_path.parent, Path(spec["core"])
    aizen = Path(os.environ["AIZEN_HOME"])
    report = {"passed": False, "commands": [], "measurements": {}, "samples": spec["samples"]}

    def save():
        (root / "measurements.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    def run(name, *arguments, success=True):
        # This worker already belongs to the parent's private Job. Detached descendants inherit
        # it; no breakaway is requested. The parent proves whole-tree settlement after worker exit.
        started = time.perf_counter_ns()
        result = subprocess.run([str(core), *map(str, arguments)], cwd=root / "workspace",
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=60,
                                creationflags=subprocess.DETACHED_PROCESS, check=False)
        elapsed = (time.perf_counter_ns() - started) / 1_000_000
        (root / f"{name}.stdout").write_bytes(result.stdout)
        (root / f"{name}.stderr").write_bytes(result.stderr)
        require(len(result.stdout) + len(result.stderr) <= 1024 * 1024, "unexpected command output size")
        require((result.returncode == 0) == success,
                f"{name}: exit {result.returncode}: {result.stderr[-1500:]!r}")
        require(not (aizen / "plugin-state/memory-transaction.json").exists(),
                f"{name}: runtime did not retire physical transaction marker")
        report["commands"].append({"name": name, "exit": result.returncode, "ms": elapsed})
        return result.stdout, elapsed

    def json_run(name, *arguments):
        raw, elapsed = run(name, *arguments)
        return json.loads(raw), elapsed

    def measure(name, arguments, validate):
        samples = []
        for index in range(spec["samples"] + 1):
            value, elapsed = json_run(f"{name}-{index:02d}", *arguments)
            validate(value)
            if index:  # Discard one warmup for every route/query, not just the first process.
                samples.append(elapsed)
        report["measurements"][name] = summarize(samples)
        save()

    try:
        for key in ("core", "mnemos", "package"):
            require(support.sha256(Path(spec[key])) == spec[key + "_sha256"], "input artifact changed")
        absent, _ = json_run("absent-status", "memory", "status", "--json")
        require(not absent["enabled"] and absent["selected"] is None and not aizen.exists(),
                "read-only unavailable status created host state")
        run("install", "plugin", "install", spec["package"])
        installed, _ = json_run("installed-status", "memory", "status", "--json")
        require(not installed["enabled"] and installed["selected"] is None,
                "installation selected or activated provider")
        require(not (aizen / "memory-data").exists(), "install executed provider")
        require(support.sha256(aizen / "plugins/mnemos/aizen-mnemos.exe") == spec["mnemos_sha256"],
                "package does not contain the supplied Mnemos binary")
        run("eligible", "plugin", "enable", "mnemos")
        run("use", "memory", "use", "mnemos", "--yes", "--allow-guarded")
        require(not (aizen / "memory-data").exists(), "selection alone created provider data")
        for index, entry in enumerate(spec["corpus"]):
            value, _ = json_run(f"seed-{index:02d}", "--sandbox", "guarded", "memory", "add",
                                entry["id"], "--description", entry["description"],
                                "--type", "project", "--body", entry["body"])
            require(value["record"]["id"] == entry["id"]
                    and value["record"]["content"] == entry["body"], "seed round trip failed")
        report["corpus_entries"] = len(spec["corpus"])

        def active_status(value):
            require(value["enabled"] and value["selected"] == "mnemos", "active selection lost")

        def health(value):
            require(value["status"] == "ready", "provider is not ready")

        measure("active-offline-status", ("memory", "status", "--json"), active_status)
        measure("provider-health", ("--sandbox", "guarded", "memory", "health"), health)
        for query in spec["queries"]:
            def search(value, acceptable=query["acceptable"]):
                ids = [item["record"]["id"] for item in value["items"]]
                require(bool(set(ids).intersection(acceptable)) if acceptable else not ids,
                        "fixed query lost expected hit/abstention")
                require(all(0 <= item["score"] <= 1 for item in value["items"]), "score out of bounds")
            measure("search-" + query["id"],
                    ("--sandbox", "guarded", "memory", "search", query["query"], "-k", "10"), search)
        before_disable = support.inventory(aizen / "memory-data/mnemos")
        run("disable", "memory", "disable")

        def disabled_status(value):
            require(not value["enabled"] and value["selected"] == "mnemos", "disabled selection lost")

        measure("disabled-offline-status", ("memory", "status", "--json"), disabled_status)
        run("disabled-health-refusal", "--sandbox", "guarded", "memory", "health", success=False)
        require(support.inventory(aizen / "memory-data/mnemos") == before_disable,
                "disabled status/refusal changed provider data")
        report["disabled_data_unchanged"] = True
        for key in ("core", "mnemos", "package"):
            require(support.sha256(Path(spec[key])) == spec[key + "_sha256"], "input artifact changed")
        report["passed"] = True
    except Exception as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        traceback.print_exc()
    finally:
        save()
    return 0 if report["passed"] else 1


def main() -> int:
    if sys.flags.isolated != 1 or sys.platform != "win32" or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise SystemExit("Run with Python -I on Windows x86_64")
    if len(sys.argv) == 3 and sys.argv[1] == "--owned-worker":
        return worker(Path(sys.argv[2]).resolve(strict=True))
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("core", "mnemos", "package"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--samples", type=int, default=20)
    args = parser.parse_args()
    if not 5 <= args.samples <= 50:
        parser.error("use 5..50 measured samples per route/query")
    support = support_module()
    root = Path(tempfile.mkdtemp(prefix="aizen-mnemos-latency-"))
    workspace = root / "workspace"
    workspace.mkdir()
    (workspace / ".git").mkdir()
    corpus_path = Path(__file__).resolve().parents[2] / "aizen-mnemos/bench-fixtures/memories.jsonl"
    corpus = [json.loads(line) for line in corpus_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    support.require(len(corpus) == 26 and len({e["id"] for e in corpus}) == 26,
                    "unexpected fixed fixture corpus")
    spec = {"samples": args.samples, "corpus": corpus,
            "corpus_sha256": support.sha256(corpus_path),
            "queries": [{"id": "literal", "query": "postgres schemas split", "acceptable": ["router-schemas"]},
                        {"id": "vietnamese", "query": "cà phê đen buổi sáng", "acceptable": ["vn-coffee"]},
                        {"id": "unmatched", "query": "unmatchedquasar zyxwvut", "acceptable": []}]}
    for key in ("core", "mnemos", "package"):
        path = getattr(args, key).resolve(strict=True)
        support.require(path.is_file(), "input artifact must be an existing file")
        spec[key], spec[key + "_sha256"] = str(path), support.sha256(path)
    spec_path = root / "spec.json"
    spec_path.write_text(json.dumps(spec, indent=2) + "\n", encoding="utf-8")
    report = {"root": str(root), "passed": False, "artifacts": {key: value for key, value in spec.items()
              if key not in ("corpus", "queries")},
              "script_sha256": support.sha256(Path(__file__).resolve()),
              "support_sha256": support.sha256(Path(__file__).with_name("memory_agent_smoke.py")),
              "spec_sha256": support.sha256(spec_path),
              "limitations": ["One Windows host; warm sequential CLI subprocess wall time, not isolated IPC/engine time",
                              "Health/search include Core startup, package verification, sandbox/process startup, operation and settlement",
                              "26 synthetic fixture records seeded as Project scope, not actual user data or original mixed-scope corpus",
                              "Dense artifacts run without semantic weights; no semantic quality/model-loading claim",
                              "No cold-start, CPU/AV/load isolation, statistical significance or disabled-idle OS tracing",
                              "Guarded execution and Job containment do not kernel-confine filesystem/network access"]}
    print(f"Provider latency fixture: {root}", flush=True)
    runner = support.OwnedProcessRunner(root, workspace, support.isolated_environment(root), 240)
    try:
        outcome = runner.run("measure", [sys.executable, "-I", str(Path(__file__).resolve()),
                                         "--owned-worker", str(spec_path)])
        report["job"] = {k: v for k, v in outcome.items() if k not in ("stdout", "stderr")}
        support.require(outcome["exit"] == 0, outcome["stderr"][-2500:])
        support.require(outcome["job_settled"] and not runner.cleanup_failures, "owned Job unsettled")
        support.require(support.sha256(spec_path) == report["spec_sha256"], "specification changed")
        report["results"] = json.loads((root / "measurements.json").read_text(encoding="utf-8"))
        for key in ("core", "mnemos", "package"):
            support.require(support.sha256(Path(spec[key])) == spec[key + "_sha256"], "input artifact changed")
        report["passed"] = report["results"]["passed"]
        for name, values in report["results"]["measurements"].items():
            print(f"{name}: median {values['median_ms']:.4f} ms ({args.samples} warm samples)", flush=True)
    except Exception as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        traceback.print_exc()
    finally:
        report["process_cleanup_failures"] = runner.cleanup_failures
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"{'PASS' if report['passed'] else 'FAIL'}: {root / 'report.json'}", flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
