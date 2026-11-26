#!/usr/bin/env python3
"""Measure an explicitly selected legacy Aizen binary in disposable repositories.

No installed configuration, repository history or source files are changed. Results identify
binary bytes, not a presumed source revision. Git Trace2 counts include descendant Git starts;
these end-to-end measurements do not pretend to measure internal lock duration or token usage.
Run: python -I scripts/timemachine_baseline.py --binary target/release/aizen.exe
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
import time


def summarize(values):
    ordered = sorted(values)
    return {"samples": len(values), "median_ms": round(statistics.median(values), 2),
            "p95_ms": round(ordered[math.ceil(len(values) * .95) - 1], 2),
            "raw_ms": [round(value, 2) for value in values]}


def isolated_env(base: Path):
    env = {k: v for k, v in os.environ.items()
           if not k.upper().startswith(("GIT_", "AIZEN_"))}
    home = base / "home"
    home.mkdir()
    global_config = base / "gitconfig"
    global_config.write_bytes(b"")
    env.update(HOME=str(home), USERPROFILE=str(home), XDG_CONFIG_HOME=str(home),
               AIZEN_HOME=str(home / "aizen"), GIT_CONFIG_GLOBAL=str(global_config),
               GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0", GIT_OPTIONAL_LOCKS="0",
               NO_COLOR="1", TERM="dumb")
    return env


def run(args, root, env, *, ok=True):
    started = time.perf_counter()
    result = subprocess.run([str(arg) for arg in args], cwd=root, env=env,
                            capture_output=True, timeout=180, stdin=subprocess.DEVNULL)
    elapsed = (time.perf_counter() - started) * 1000
    if ok and result.returncode:
        raise RuntimeError(f"{args}: exit {result.returncode}: "
                           f"{result.stderr.decode('utf-8', 'replace')[-4000:]}")
    return result, elapsed


def folder_bytes(path):
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file()) if path.exists() else 0


def fixture(base, git, files):
    base.mkdir()
    env = isolated_env(base)
    repo = base / "repo"
    repo.mkdir()
    env["AIZEN_WORKSPACE_ROOT"] = str(repo)
    run([git, "init", "-q"], repo, env)
    for name, value in [("user.name", "Fixture"), ("user.email", "fixture@example.invalid"),
                        ("commit.gpgSign", "false"), ("core.autocrlf", "false")]:
        run([git, "config", name, value], repo, env)
    (repo / "src").mkdir()
    (repo / ".gitignore").write_text("target/\n", encoding="utf-8")
    for n in range(files):
        (repo / "src" / f"f{n:05d}.txt").write_bytes(f"base {n}\n".encode())
    run([git, "add", "-A"], repo, env)
    run([git, "commit", "-qm", "fixture baseline"], repo, env)
    ignored = repo / "target"
    ignored.mkdir()
    for n in range(1000):
        (ignored / f"ignored-{n}.txt").write_bytes(b"not checkpoint content\n")
    return repo, env


def save(binary, repo, env, label):
    result, elapsed = run([binary, "time", "save", label], repo, env)
    text = re.sub(r"\x1b\[[0-9;]*m", "", result.stdout.decode("utf-8", "replace"))
    match = re.search(r"checkpoint\s+#(\d+)", text)
    if not match:
        raise RuntimeError(f"cannot resolve checkpoint ID from fixture output: {text!r}")
    return int(match.group(1)), elapsed


def measure(base, binary, git, files, samples):
    repo, env = fixture(base, git, files)
    index = (repo / ".git" / "index").read_bytes()
    head = run([git, "rev-parse", "HEAD"], repo, env)[0].stdout
    trace = base / "git-trace.jsonl"
    env["GIT_TRACE2_EVENT"] = str(trace)
    initial, cold = save(binary, repo, env, "baseline")
    saved_bytes = folder_bytes(repo / ".git" / "objects") + folder_bytes(Path(env["AIZEN_HOME"]))
    timings = {"save_1_changed": [], "save_100_changed": [], "list": [], "diff_one_path": [],
               "restore_baseline": []}
    sizes = {"list": [], "diff_one_path": []}
    for sample in range(samples):
        for count in [1, min(100, files)]:
            for n in range(count):
                (repo / "src" / f"f{n:05d}.txt").write_bytes(f"sample {sample}, changed {count}, file {n}\n".encode())
            checkpoint, elapsed = save(binary, repo, env, f"sample-{sample}-{count}")
            timings[f"save_{count}_changed"].append(elapsed)
        for key, args in [("list", ["list"]),
                          ("diff_one_path", ["diff", str(initial), str(checkpoint), "--patch", "--path", "src/f00000.txt"])]:
            result, elapsed = run([binary, "time", *args], repo, env)
            timings[key].append(elapsed)
            sizes[key].append(len(result.stdout))
        _, elapsed = run([binary, "time", "restore", str(initial)], repo, env)
        timings["restore_baseline"].append(elapsed)
        assert (repo / "src" / "f00000.txt").read_bytes() == b"base 0\n"
        assert (repo / "src" / f"f{min(99, files - 1):05d}.txt").read_bytes() == f"base {min(99, files - 1)}\n".encode()
        assert (repo / ".git" / "index").read_bytes() == index, "real index changed"
        assert run([git, "rev-parse", "HEAD"], repo, env)[0].stdout == head, "HEAD changed"
    trace_starts = 0
    trace_commands = {}
    if trace.exists():
        for line in trace.read_text(encoding="utf-8").splitlines():
            event = json.loads(line)
            trace_starts += event.get("event") == "start"
            if event.get("event") == "cmd_name":
                name = event.get("name", "unknown")
                trace_commands[name] = trace_commands.get(name, 0) + 1
    final_bytes = folder_bytes(repo / ".git" / "objects") + folder_bytes(Path(env["AIZEN_HOME"]))
    return {"tracked_fixture_files": files, "ignored_fixture_files": 1000,
            "first_save_fresh_store_ms": round(cold, 2),
            "cache_note": "Fresh fixture/store, not a flushed OS filesystem cache.",
            "warm": {k: summarize(v) for k, v in timings.items()},
            "stdout_bytes": sizes, "git_trace_start_events": trace_starts,
            "git_trace_commands": trace_commands,
            "git_trace_note": "Includes HEAD checks; tracing enabled for all measured operations.",
            "store_and_home_bytes_after_first_save": saved_bytes,
            "store_and_home_growth_bytes": final_bytes - saved_bytes,
            "head_index_preserved": True}


def characterize(base, binary, git):
    repo, env = fixture(base, git, 1)
    path = repo / "src" / "f00000.txt"
    first, _ = save(binary, repo, env, "first")
    path.write_bytes(b"unsaved user data\n")
    restore, _ = run([binary, "time", "restore", str(first)], repo, env)
    redo, _ = run([binary, "time", "redo"], repo, env, ok=False)
    redo_failed = redo.returncode != 0 and b"newest checkpoint" in redo.stderr
    assert redo_failed, "selected binary no longer reproduces legacy dirty-restore redo mismatch"
    assert path.read_bytes() == b"base 0\n"
    path.write_bytes(b"middle\n")
    middle, _ = save(binary, repo, env, "middle")
    path.write_bytes(b"latest\n")
    latest, _ = save(binary, repo, env, "latest")
    run([binary, "time", "rm", str(middle)], repo, env)
    undo, _ = run([binary, "time", "undo"], repo, env, ok=False)
    assert undo.returncode != 0 and f"no checkpoint #{middle}".encode() in undo.stderr, (
        "expected missing-middle checkpoint failure, not another error: "
        + undo.stderr.decode("utf-8", "replace"))
    assert path.read_bytes() == b"latest\n"
    return {"dirty_restore_promises_redo": b"aizen time redo" in restore.stdout,
            "dirty_restore_redo_exit": redo.returncode,
            "redo_error": redo.stderr.decode("utf-8", "replace").strip(),
            "removed_middle": middle, "cursor": latest, "undo_exit": undo.returncode,
            "undo_error": undo.stderr.decode("utf-8", "replace").strip()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--files", type=int, nargs="+", default=[1000, 10000])
    parser.add_argument("--samples", type=int, default=3)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    git = shutil.which("git")
    if not git or args.samples < 1 or any(n < 100 for n in args.files):
        parser.error("requires Git, samples >= 1 and file counts >= 100")
    result = {"binary": str(binary), "binary_bytes": binary.stat().st_size,
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "source_revision": "unknown: explicitly selected pre-existing binary",
              "scope": "legacy CLI only, not new backend or live source verification",
              "unmeasured": ["lock hold time", "bytes read", "tokenizer counts", "new backend performance"]}
    with tempfile.TemporaryDirectory(prefix="aizen-time-baseline-") as temp:
        base = Path(temp)
        env = isolated_env(base)
        result["version"] = run([binary, "--version"], base, env)[0].stdout.decode().strip()
        result["startup_help"] = summarize([run([binary, "--help"], base, env)[1] for _ in range(10)])
        result["characterization"] = characterize(base / "characterize", binary, git)
        result["fixtures"] = [measure(base / f"files-{n}", binary, git, n, args.samples) for n in args.files]
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
