"""Compare explicit prebuilt Core startup artifacts inside one fixture-owned Windows Job.

Run with Python -I. This never builds, installs a provider, loads actual user configuration, or
changes an input binary. Samples measure subprocess creation through exit, not internal main().
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
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


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def worker(spec_path: Path) -> int:
    # The parent creates and hashes this specification before starting the owned Job. No script or
    # module is imported from a package, artifact directory, workspace or configurable search path.
    spec = json.loads(spec_path.read_text(encoding="utf-8"))
    root = spec_path.parent
    result = {"artifacts": [], "pairs": spec["pairs"], "samples": spec["samples"], "passed": False}
    paths = []
    for artifact in spec["artifacts"]:
        path = Path(artifact["path"])
        if sha256(path) != artifact["sha256"]:
            raise ValueError("input artifact changed before sampling")
        paths.append(path)
        result["artifacts"].append({**artifact, "version": None, "warm_version_ms": {"samples": []}})

    for index in range(spec["samples"] + 1):
        # Alternate order to reduce simple cache/load ordering bias; index zero is warmup only.
        order = list(range(len(paths)))
        if index % 2:
            order.reverse()
        for number in order:
            started = time.perf_counter_ns()
            process = subprocess.run([str(paths[number]), "--version"], cwd=root / "workspace",
                                     stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, timeout=10, check=False,
                                     creationflags=subprocess.DETACHED_PROCESS)
            elapsed = (time.perf_counter_ns() - started) / 1_000_000
            if process.returncode != 0 or process.stderr or len(process.stdout) > 1024:
                raise ValueError("version command failed or produced unexpected output")
            version = process.stdout.decode("utf-8").strip()
            if version != "aizen 0.7.3":
                raise ValueError("unexpected Core version")
            artifact = result["artifacts"][number]
            artifact["version"] = version
            if index:
                artifact["warm_version_ms"]["samples"].append(elapsed)

    for path, artifact in zip(paths, result["artifacts"]):
        samples = artifact["warm_version_ms"]["samples"]
        artifact["warm_version_ms"].update({"median": statistics.median(samples),
                                           "min": min(samples), "max": max(samples)})
        artifact["sha256_after"] = sha256(path)
        if artifact["sha256_after"] != artifact["sha256"]:
            raise ValueError("input artifact changed during sampling")
    home = Path(os.environ["AIZEN_HOME"])
    if home.exists():
        raise ValueError("version sampling initialized host state")
    result["no_host_state_created"] = True
    result["passed"] = True
    (root / "samples.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    return 0


def main() -> int:
    if sys.flags.isolated != 1 or sys.platform != "win32" or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise SystemExit("Run with Python -I on Windows x86_64")
    if len(sys.argv) == 3 and sys.argv[1] == "--owned-worker":
        return worker(Path(sys.argv[2]).resolve(strict=True))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pair", nargs=2, action="append", type=Path, required=True,
                        metavar=("BASELINE", "CURRENT"))
    parser.add_argument("--samples", type=int, default=20)
    args = parser.parse_args()
    if not 5 <= args.samples <= 100 or not 1 <= len(args.pair) <= 4:
        parser.error("use 5..100 samples and 1..4 explicit artifact pairs")
    root = Path(tempfile.mkdtemp(prefix="aizen-mnemos-startup-"))
    workspace = root / "workspace"
    workspace.mkdir()
    artifacts, pairs = [], []
    for baseline, current in args.pair:
        indices = []
        for label, supplied in (("baseline", baseline), ("current", current)):
            path = supplied.resolve(strict=True)
            if not path.is_file():
                raise ValueError("artifact must be a regular file")
            indices.append(len(artifacts))
            artifacts.append({"label": label, "path": str(path), "bytes": path.stat().st_size,
                              "sha256": sha256(path)})
        pairs.append(indices)
    spec = {"artifacts": artifacts, "pairs": pairs, "samples": args.samples}
    spec_path = root / "spec.json"
    spec_path.write_text(json.dumps(spec, indent=2), encoding="utf-8")
    support_path = Path(__file__).resolve().with_name("memory_agent_smoke.py")
    module_spec = importlib.util.spec_from_file_location("owned_startup_support", support_path)
    support = importlib.util.module_from_spec(module_spec)
    module_spec.loader.exec_module(support)
    env = support.isolated_environment(root)
    runner = support.OwnedProcessRunner(root, workspace, env, 180)
    report = {"root": str(root), "passed": False, "script_sha256": sha256(Path(__file__).resolve()),
              "support_sha256": sha256(support_path), "spec_sha256": sha256(spec_path),
              "limitations": ["Windows warm subprocess wall time; includes Python/CreateProcess overhead",
                              "Alternating order, not CPU/AV/load isolation or cold-start measurement",
                              "Version entrypoint only; no model, provider-ready or retrieval latency",
                              "Baseline/source/toolchain provenance must be supplied separately"]}
    print(f"Startup measurement root: {root}", flush=True)
    try:
        outcome = runner.run("sample", [sys.executable, "-I", str(Path(__file__).resolve()),
                                        "--owned-worker", str(spec_path)])
        report["job"] = {key: value for key, value in outcome.items() if key not in ("stdout", "stderr")}
        support.require(outcome["exit"] == 0, outcome["stderr"][-2000:])
        support.require(sha256(spec_path) == report["spec_sha256"], "measurement spec changed")
        result = json.loads((root / "samples.json").read_text(encoding="utf-8"))
        expected = 1 + (args.samples + 1) * len(artifacts)
        support.require(outcome["job_settled"] and outcome["job_processes"] == expected,
                        "unexpected/unsettled measurement processes")
        support.require(not runner.cleanup_failures and result["passed"], "measurement failed")
        report.update(result)
        report["comparisons"] = []
        for before, after in pairs:
            baseline, current = result["artifacts"][before], result["artifacts"][after]
            report["comparisons"].append({
                "baseline": baseline["path"], "current": current["path"],
                "bytes_delta": current["bytes"] - baseline["bytes"],
                "median_ms_delta": current["warm_version_ms"]["median"] - baseline["warm_version_ms"]["median"],
            })
    except Exception as error:
        report["passed"] = False
        report["failure"] = f"{type(error).__name__}: {error}"
        traceback.print_exc()
    finally:
        report["process_cleanup_failures"] = runner.cleanup_failures
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"{'PASS' if report['passed'] else 'FAIL'}: {root / 'report.json'}", flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
