"""Read-only actual CLI extension over a retained workflow_git_cli_smoke fixture.

Run only after the coordinator accepts and builds the inspection candidate:
  python -I operation_inspection_cli_smoke.py --binary FRESH_AIZEN --fixture RETAINED_FIXTURE

Exactly four product invocations normally: genuine integration receipt, invalid ID,
missing ID, and the genuine receipt from a foreign child worktree. Optional
--pending-receipt inspects one ALREADY EXISTING genuine pending record; this driver
never creates/edits receipts, invokes a mutation, manufactures an unknown worker,
uses --yes, or changes product policy/timeouts. Without such a record, pending/unknown
CLI coverage is explicitly NOT CLAIMED. Core tests cover their own synthetic cases.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import traceback

MAX_JSON = 4 * 1024 * 1024
MAX_TREE_BYTES = 128 * 1024 * 1024
MAX_TREE_FILES = 16384
VALID_ID = re.compile(r"[A-Za-z0-9_-]{1,96}\Z")


def require(ok, message):
    if not ok:
        raise AssertionError(message)


def digest(data):
    return {"byte_len": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def load(path, limit=MAX_JSON):
    require(path.stat().st_size <= limit, f"oversized fixture JSON: {path}")
    return json.loads(path.read_bytes())


def key(path):
    return os.path.normcase(str(path.resolve(strict=True)).removeprefix("\\\\?\\"))


def inside(path, root):
    return os.path.commonpath([key(path), key(root)]) == key(root)


def valid_id(value):
    return bool(VALID_ID.fullmatch(value)) and not value.isdigit()


def image_tree(root):
    result, count, size = {}, 0, 0
    if not root.exists():
        return {"exists": False}
    for directory, dirs, files in os.walk(root, followlinks=False):
        for path in [Path(directory), *[Path(directory) / n for n in dirs + files]]:
            md = path.lstat()
            require(not path.is_symlink() and not (getattr(md, "st_file_attributes", 0) &
                    getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)), f"fixture link/reparse refused: {path}")
        result[str(Path(directory).relative_to(root)) + "/"] = None
        for name in sorted(files):
            path = Path(directory) / name
            count += 1
            size += path.stat().st_size
            require(count <= MAX_TREE_FILES and size <= MAX_TREE_BYTES, "fixture snapshot bound exceeded")
            result[str(path.relative_to(root))] = digest(path.read_bytes())
    return {"exists": True, "entries": result}


def run(args):
    script = Path(__file__).resolve(strict=True)
    binary = args.binary.resolve(strict=True)
    fixture = args.fixture.resolve(strict=True)
    require(fixture.name.startswith("aizen-workflow-cli-"), "select a retained workflow_git_cli_smoke fixture")
    require(inside(fixture, Path(tempfile.gettempdir())), "fixture must remain in disposable Temp")
    root, home, runs = (fixture / n for n in ("repo", "home", "runs"))
    require(all(p.is_dir() and inside(p, fixture) for p in (root, home, runs)), "fixture layout missing")
    require((root / ".git").is_dir(), "parent fixture must be an ordinary disposable Git repository")
    original_result = load(fixture / "result.json")
    provider = load(fixture / "provider-state.json")
    require(original_result.get("provider_calls") == {"owned-first": 3, "owned-dependent": 3}, "not a completed genuine workflow fixture")
    require(original_result.get("synthesis_calls") == 1 and not provider.get("errors"), "workflow provider did not complete cleanly")
    runlog = runs / (original_result["run_id"] + ".jsonl")
    require(runlog.is_file() and runlog.stat().st_size <= MAX_JSON, "missing bounded workflow journal")
    events = [json.loads(line) for line in runlog.read_text(encoding="utf-8").splitlines() if line.strip()]
    handoffs = [e["git_handoff"] for e in events if e.get("kind") == "attempt_completed" and e.get("git_handoff")]
    require(len(handoffs) == 2, "expected two genuine completed workflow child handoffs")
    children = [Path(h["root"]).resolve(strict=True) for h in handoffs]
    require(all(inside(p, fixture) and key(p) != key(root) and (p / ".git").is_file() for p in children), "foreign child root is not inside the disposable fixture")
    require(len({key(p) for p in children}) == 2, "workflow children share a root")
    receipt_paths = sorted((home / "git-operations-v1").glob("*/*/*.json"))
    require(0 < len(receipt_paths) <= 128, "fixture receipt inventory absent or too large")
    records = [(p, load(p, 128 * 1024)) for p in receipt_paths]
    matches = [(p, r) for p, r in records if r.get("change", {}).get("action") == "integrate_task"
               and r.get("after") == original_result["dependent_tip"] and key(Path(r["root"])) == key(root)]
    if args.receipt:
        matches = [(p, r) for p, r in matches if r.get("id") == args.receipt]
    require(len(matches) == 1, "select the exact genuine final integration receipt")
    receipt_path, receipt = matches[0]
    receipt_id = receipt["id"]
    require(valid_id(receipt_id) and receipt_path.name == receipt_id + ".json", "invalid selected receipt")
    require(receipt["state"] == "settled_unverified" and receipt.get("verified") is True and isinstance(receipt.get("verification"), str), "expected the genuine integration plus destination verification from the completed workflow")
    pending = None
    if args.pending_receipt:
        require(valid_id(args.pending_receipt), "invalid optional pending ID")
        selected = [(p, r) for p, r in records if r.get("id") == args.pending_receipt and key(Path(r["root"])) == key(root)]
        require(len(selected) == 1, "optional pending receipt must already exist in this fixture")
        pending = selected[0]
        require(pending[1]["state"] in ("prepared", "running", "unsettled", "unknown"), "optional receipt is not genuinely recorded pending/unknown")
    missing = "fixture-missing-operation-inspection"
    require(all(r["id"] != missing for _, r in records), "missing-ID test collided with an existing receipt")
    out = Path(tempfile.mkdtemp(prefix="az-op-inspect-"))
    print(f"ARTIFACTS {out}", flush=True)
    logs = out / "logs"
    logs.mkdir()
    profile = out / "isolated-profile"
    profile.mkdir()
    empty = out / "empty-git-config"
    empty.write_bytes(b"")
    env = {k: v for k, v in os.environ.items() if not k.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_", "GIT_"))
           and k.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY")}
    env.update({"AIZEN_HOME": str(home), "AIZEN_RUNS_DIR": str(runs), "GIT_CONFIG_GLOBAL": str(empty),
                "GIT_CONFIG_NOSYSTEM": "1", "GIT_OPTIONAL_LOCKS": "0", "NO_PROXY": "127.0.0.1"})
    for variable in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "TEMP", "TMP", "TMPDIR"):
        directory = profile / variable.lower()
        directory.mkdir()
        env[variable] = str(directory)
    git = shutil.which("git")
    require(git, "Git is required for independent read-only observations")
    calls = []

    def command(label, argv, cwd):
        require("--yes" not in argv and "--force" not in argv, "approval/policy escalation is not allowed")
        print("START " + label, flush=True)
        meta = {"argv": list(map(str, argv)), "cwd": str(cwd)}
        (logs / f"{label}.command.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
        try:
            result = subprocess.run(argv, cwd=cwd, env=env, input=b"", capture_output=True, timeout=180)
        except subprocess.TimeoutExpired as error:
            (logs / f"{label}.stdout").write_bytes(error.stdout or b"")
            (logs / f"{label}.stderr").write_bytes(error.stderr or b"")
            meta.update(direct_exit=None, timeout=True)
            (logs / f"{label}.result.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
            raise
        (logs / f"{label}.stdout").write_bytes(result.stdout)
        (logs / f"{label}.stderr").write_bytes(result.stderr)
        meta.update(direct_exit=result.returncode, stdout=digest(result.stdout), stderr=digest(result.stderr), timeout=False)
        (logs / f"{label}.result.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
        calls.append({"label": label, **meta})
        print(f"END {label} exit={result.returncode}", flush=True)
        return result

    def query(label, *argv):
        result = command(label, [git, "--no-optional-locks", *argv], root)
        require(result.returncode == 0, (label, result.returncode, result.stderr.decode(errors="replace")))
        return result.stdout

    def snapshot():
        return {"parent": image_tree(root), "children": [image_tree(p) for p in children],
                "operation_receipts": image_tree(home / "git-operations-v1"),
                "native_journal": image_tree(home / "native-journal-v1"),
                "workflow_journal": digest(runlog.read_bytes())}

    summary = {"status": "STARTED", "binary": str(binary), "binary_sha256": digest(binary.read_bytes())["sha256"],
               "driver_sha256": digest(script.read_bytes())["sha256"], "fixture": str(fixture), "receipt": receipt_id,
               "fixture_result_sha256": digest((fixture / "result.json").read_bytes())["sha256"],
               "pending_unknown_cli_coverage": "NOT_CLAIMED_NO_GENUINE_PENDING_RECEIPT" if pending is None else "REQUESTED_EXISTING_RECEIPT"}
    try:
        baseline = snapshot()
        (out / "before.json").write_text(json.dumps(baseline, indent=2), encoding="utf-8")
        head = query("git-head", "rev-parse", "HEAD").decode().strip()
        branch = query("git-branch", "symbolic-ref", "HEAD").decode().strip()
        entries = query("git-stage", "ls-files", "--stage", "-z")
        status = query("git-status", "status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignored=matching")
        require(head == original_result["dependent_tip"] and not status, "retained successful workflow fixture drifted")
        require(snapshot() == baseline, "independent read-only Git probes changed fixture bytes")

        def inspect(label, selected, cwd, positive=False, pending_record=None):
            before = snapshot()
            result = command(label, [str(binary), "--sandbox", "guarded", "time", "operations", "--receipt", selected], cwd)
            after = snapshot()
            (out / f"{label}.after.json").write_text(json.dumps(after, indent=2), encoding="utf-8")
            require(after == before == baseline, f"{label} changed source/Git/index/receipt/native-journal/runlog bytes")
            if not positive:
                require(result.returncode != 0, f"{label} unexpectedly accepted a negative selection")
                return
            require(result.returncode == 0, (label, result.returncode, result.stderr.decode(errors="replace")))
            report = json.loads(result.stdout)
            expected_path, expected_record = pending_record or (receipt_path, receipt)
            require(report["schema"] == 1 and report["selected_receipt"] == selected, "wrong selected report")
            require(report["status"] == "observed" and key(Path(report["identity"]["root"])) == key(root), "actual observation was blocked or foreign")
            require(report["receipt"]["record"] == expected_record, "report changed or substituted the selected receipt")
            require(report["receipt"]["fingerprint"] == digest(expected_path.read_bytes()), "selected exact-byte fingerprint mismatch")
            require(report["reconciliation_preview_available"] is False, "inspection unexpectedly offered a reconciliation preview for completion/unknown")
            require(report["execution_barriers_fully_checked"] is False, "observation falsely claimed complete execution-barrier checks")
            current = report["current"]
            require(current["head"] == head and current["branch"] == branch, "actual HEAD/branch observation mismatch")
            require(current["index"] == digest((root / ".git/index").read_bytes()), "raw index digest mismatch")
            require(current["index_entries"] == digest(entries) and current["porcelain_status"] == digest(status), "Git observation digests mismatch")
            require(current["porcelain_clean"] is True and current["sequencer"] is None, "current Git status mismatch")
            require("Observation only" in report["notice"] and "No receipt was acknowledged" in report["notice"], "authority notice missing")
            require("Preview availability is nonbinding" in report["notice"] and "are not fully checked" in report["notice"], "incomplete preview-barrier notice missing")
            disposition = "pending_unknown" if pending_record else "recorded_complete"
            require(report["receipt"]["disposition"] == disposition, "incorrect recorded disposition")

        inspect("selected-real-integration", receipt_id, root, positive=True)
        inspect("invalid-id", "../escape", root)
        inspect("missing-id", missing, root)
        inspect("foreign-worktree-id", receipt_id, children[0])
        if pending:
            inspect("existing-pending-receipt", pending[1]["id"], root, positive=True, pending_record=pending)
            summary["pending_unknown_cli_coverage"] = "PASSED_EXISTING_UNMODIFIED_PENDING_RECORD"
        summary.update(status="PASS", product_invocations=4 + int(pending is not None),
                       checks="exact selected bytes/current Git observation; invalid/missing/foreign refusal; full source/Git/index/receipt/native-journal/runlog preservation",
                       receipt_or_authority_manufactured=False, product_mutations_invoked=False)
        print("PASS exact-receipt CLI observation; pending/unknown coverage: " + summary["pending_unknown_cli_coverage"], flush=True)
    except BaseException as error:
        summary.update(status="FAIL", error=repr(error))
        (out / "failure.txt").write_text(traceback.format_exc(), encoding="utf-8")
        raise
    finally:
        summary["calls"] = calls
        (out / "result.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    return out


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--receipt", help="Optional exact final integration receipt; never selects implicit latest")
    parser.add_argument("--pending-receipt", help="Optional ALREADY EXISTING genuine pending/unknown receipt; never written by this driver")
    run(parser.parse_args())
