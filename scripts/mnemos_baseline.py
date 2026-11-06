"""Capture a PRE-EXTRACTION, source-matched offline Memory baseline in owned temporary roots.

Historical utility: requires a checkout that still exposes `aizen bench memory/profile/dialectic`.
Those benchmarks now belong to the standalone provider; this is not a current-Core smoke script.
No installed binary, user configuration, live endpoint, model weights or stored memories are used.
Run from the historical Aizen checkout with Python -I. Outputs remain in the printed temporary root.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def source_hashes(repo: Path) -> dict[str, str]:
    paths = [repo / "Cargo.toml", repo / "Cargo.lock"]
    paths.extend(sorted((repo / "src").rglob("*.rs")))
    paths.extend(sorted((repo / "bench-fixtures").rglob("*.json*")))
    return {str(path.relative_to(repo)): sha256(path) for path in paths}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--cargo", type=Path, required=True)
    parser.add_argument("--git", type=Path, required=True)
    parser.add_argument("--toolchain", default="stable-x86_64-pc-windows-msvc")
    parser.add_argument("--resume", type=Path, help="Resume an owned baseline with identical source hashes")
    parser.add_argument("--continue-after-unit-failure", action="store_true",
                        help="Retain failed unit evidence but continue collecting build/bench measurements")
    args = parser.parse_args()
    repo = args.repo.resolve(strict=True)
    if not (repo / "src/memory/mod.rs").is_file():
        parser.error("pre-extraction baseline only: use an original checkout with the built-in Memory benchmarks; current Core has no built-in engine")
    cargo = args.cargo.resolve(strict=True)
    git = args.git.resolve(strict=True)
    previous = None
    if args.resume:
        root = args.resume.resolve(strict=True)
        if not root.name.startswith("aizen-mnemos-baseline-"):
            raise ValueError("not an owned baseline root")
        previous = json.loads((root / "report.json").read_text(encoding="utf-8"))
        initial = json.loads((root / "source-hashes.json").read_text(encoding="utf-8"))
        if previous["root"] != str(root) or previous["toolchain"] != args.toolchain:
            raise ValueError("baseline identity/toolchain mismatch")
        if source_hashes(repo) != initial:
            raise ValueError("cannot resume baseline after source changes")
    else:
        root = Path(tempfile.mkdtemp(prefix="aizen-mnemos-baseline-"))
        initial = source_hashes(repo)
    print(f"Baseline outputs: {root}", flush=True)
    home = root / "home"
    workspace = root / "workspace"
    home.mkdir(exist_ok=True)
    workspace.mkdir(exist_ok=True)
    isolated_temp = root / "temp"
    isolated_temp.mkdir(exist_ok=True)

    original = dict(os.environ)
    env = {
        key: value for key, value in original.items()
        if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_", "AWS_", "AZURE_", "GOOGLE_", "GEMINI_"))
        and not any(part in key.upper() for part in ("TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "API_KEY", "AUTH_SOCK"))
        and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY")
    }
    original_home = Path(original.get("USERPROFILE", original.get("HOME", str(cargo.parents[2]))))
    env.update({
        "CARGO_HOME": original.get("CARGO_HOME", str(original_home / ".cargo")),
        "RUSTUP_HOME": original.get("RUSTUP_HOME", str(original_home / ".rustup")),
        "RUSTUP_TOOLCHAIN": args.toolchain,
        "HOME": str(home),
        "USERPROFILE": str(home),
        "APPDATA": str(home / "AppData" / "Roaming"),
        "LOCALAPPDATA": str(home / "AppData" / "Local"),
        "XDG_CONFIG_HOME": str(home / "config"),
        "XDG_CACHE_HOME": str(home / "cache"),
        "AIZEN_HOME": str(home / ".aizen"),
        "AIZEN_NO_HOOKS": "1",
        "AIZEN_NO_UPDATE": "1",
        "AIZEN_NO_TIPS": "1",
        "AIZEN_NO_WARMUP": "1",
        "CARGO_NET_OFFLINE": "true",
        "TEMP": str(isolated_temp),
        "TMP": str(isolated_temp),
        "TMPDIR": str(isolated_temp),
        "PATH": os.pathsep.join((str(cargo.parent), str(git.parent), original.get("PATH", ""))),
    })
    for key in ("APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_CACHE_HOME"):
        Path(env[key]).mkdir(parents=True, exist_ok=True)

    steps: list[dict[str, object]] = previous["steps"] if previous else []
    report: dict[str, object] = previous or {"root": str(root), "toolchain": args.toolchain, "steps": steps}
    if not previous:
        (root / "source-hashes.json").write_text(json.dumps(initial, indent=2), encoding="utf-8")

    def save() -> None:
        (root / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")

    def run(label: str, command: list[str], *, cwd: Path = repo, seconds: int = 2400) -> bool:
        earlier = next((step for step in steps if step["label"] == label), None)
        if earlier is not None:
            if earlier["command"] != command:
                raise ValueError(f"resumed command changed: {label}")
            print(f"RETAIN {label}: {earlier['exit']}", flush=True)
            return earlier["exit"] == 0
        path = root / f"{label}.log"
        start = time.perf_counter()
        print(f"START {label}", flush=True)
        with path.open("wb") as output:
            try:
                result = subprocess.run(command, cwd=cwd, env=env, stdout=output, stderr=subprocess.STDOUT,
                                        timeout=seconds, check=False)
                code = result.returncode
            except subprocess.TimeoutExpired:
                code = "timeout"
        steps.append({"label": label, "command": command, "exit": code,
                      "seconds": round(time.perf_counter() - start, 3), "log": str(path)})
        save()
        print(f"END {label}: {code} ({steps[-1]['seconds']} s)", flush=True)
        return code == 0

    def unchanged() -> None:
        if source_hashes(repo) != initial:
            raise RuntimeError("Source/fixture files changed during baseline capture; measurements invalid")

    def capture(label: str, binary: Path) -> Path:
        unchanged()
        copied = root / (label + binary.suffix)
        shutil.copy2(binary, copied)
        samples = []
        for index in range(21):
            start = time.perf_counter_ns()
            result = subprocess.run([str(copied), "--version"], cwd=workspace, env=env,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15, check=True)
            elapsed = (time.perf_counter_ns() - start) / 1_000_000
            if index:
                samples.append(elapsed)
        report[label] = {"bytes": copied.stat().st_size, "sha256": sha256(copied),
                         "version": result.stdout.decode("utf-8", errors="replace").strip(),
                         "warm_version_ms": {"median": statistics.median(samples), "min": min(samples),
                                             "max": max(samples), "samples": samples},
                         "path": str(copied)}
        save()
        return copied

    try:
        if not run("git-head", [str(git), "rev-parse", "HEAD"], seconds=30):
            return 1
        if not run("cargo-version", [str(cargo), "--version"], seconds=30):
            return 1
        # Do not run integration targets: eval.rs can discover a live model endpoint.
        units_passed = run("core-unit", [str(cargo), "test", "--offline", "--bin", "aizen"])
        if not units_passed:
            report["unit_baseline_failed"] = True
            report["status"] = "unit_failed"
            save()
            if not args.continue_after_unit_failure:
                return 1
            # This does not erase/replace the original full-suite failure.
            run("core-unit-lsp-isolated", [str(cargo), "test", "--offline", "--bin", "aizen",
                "agent::lsp::discovery::tests::detect_by_dir_and_by_file", "--", "--exact", "--nocapture"], seconds=120)
        unchanged()
        if not run("core-debug", [str(cargo), "build", "--offline", "--bin", "aizen"]):
            return 1
        extension = ".exe" if os.name == "nt" else ""
        debug = capture("debug", repo / "target" / "debug" / ("aizen" + extension))
        benches = [
            ("recall", ["memory", "--split", "all", "--fuzzy", "--hybrid"]),
            ("evolution", ["memory", "--evolution"]),
            ("profile", ["profile"]), ("dialectic", ["dialectic"]), ("health", ["health"]),
        ]
        for label, command in benches:
            if not run("bench-" + label, [str(debug), "bench", *command], cwd=workspace, seconds=120):
                return 1
        if not run("core-release", [str(cargo), "build", "--offline", "--release", "--bin", "aizen"]):
            return 1
        capture("release", repo / "target" / "release" / ("aizen" + extension))
        if not run("core-dense", [str(cargo), "build", "--offline", "--release", "--features", "dense", "--bin", "aizen"]):
            return 1
        capture("dense", repo / "target" / "release" / ("aizen" + extension))
        unchanged()
        report["status"] = "passed" if units_passed else "measurements_complete_unit_failed"
        save()
        print("BASELINE PASS; no live model or user Memory used" if units_passed
              else "BASELINE MEASUREMENTS COMPLETE; full-suite failure retained", flush=True)
        return 0 if units_passed else 1
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
        save()
        raise
    finally:
        print(f"Full logs and report retained: {root}", flush=True)


if __name__ == "__main__":
    raise SystemExit(main())
