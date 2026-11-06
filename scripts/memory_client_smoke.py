"""Exercise the real Memory client bridge and Mnemos using disposable homes and owned Jobs.

Run: python -I scripts/memory_client_smoke.py <core.exe> <mnemos.exe>
No installed binaries, user Memory, network model calls, or automatic learning are used.
"""
from pathlib import Path
import argparse
import copy
import importlib.util
import json
import shutil
import sys
import tempfile
import traceback


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path)
    parser.add_argument("mnemos", type=Path)
    args = parser.parse_args()
    if sys.flags.isolated != 1 or sys.platform != "win32":
        raise SystemExit("Use Python -I on Windows")
    repo = Path(__file__).resolve().parents[1]
    spec = importlib.util.spec_from_file_location("owned_support", repo / "scripts/memory_agent_smoke.py")
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    require = support.require
    core, mnemos = args.core.resolve(strict=True), args.mnemos.resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix="aizen-memory-client-"))
    env = support.isolated_environment(root)
    workspace = root / "workspace"
    other = root / "other-project"
    workspace.mkdir()
    other.mkdir()
    home = Path(env["AIZEN_HOME"])
    home.mkdir(parents=True, exist_ok=True)
    custom = root / "provider-data"
    default = home / "memory-data/mnemos"
    runner = support.OwnedProcessRunner(root, workspace, env, 90)
    report = {"root": str(root), "passed": False, "cases": [], "core_sha256": support.sha256(core),
              "mnemos_sha256": support.sha256(mnemos), "limitations": ["CLI integration, not native GUI evidence", "No live-model learning or foreign-device fixture"]}
    print(f"Memory client smoke: {root}", flush=True)

    def save():
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    def run(name, command, stdin=None, offline=False):
        result = runner.run(name, list(map(str, command)), stdin)
        report["cases"].append({"name": name, **{k: v for k, v in result.items() if k not in ("stdout", "stderr")}})
        save()
        require(result["exit"] == 0, f"{name}: {result['stderr'][-3000:]}")
        require(result["job_settled"], f"{name}: unsettled Job")
        if offline:
            require(result["job_processes"] == 1, f"{name}: offline request executed provider")
        return result["stdout"]

    def client(name, action, *, workspace_path=workspace, success=True, offline=True):
        request = {"version": 1, "request_id": name, "workspace": str(workspace_path), "action": action}
        response = json.loads(run(name, [core, "memory", "client"], json.dumps(request).encode(), offline))
        require(response["version"] == 1 and response["request_id"] == name, f"{name}: response identity")
        require(response["ok"] is success, f"{name}: {response}")
        return response.get("data") if success else response["error"]

    def snapshot(name, path=workspace):
        return client(name, {"type": "snapshot"}, workspace_path=path)

    def review(name, snap, change):
        return client(name, {"type": "review", "binding": snap["binding"], "change": change})["review"]

    def apply(name, value):
        return client(name, {"type": "apply", "review": value})["snapshot"]

    def change(name, snap, value):
        return apply(name + "-apply", review(name + "-review", snap, value))

    def read(name, snap, query, success=True, path=workspace):
        return client(name, {"type": "read", "binding": snap["binding"], "query": query},
                      workspace_path=path, success=success, offline=False)

    try:
        initial = snapshot("absent")
        require(initial["state"] == "unavailable" and not list(home.iterdir()), "offline snapshot wrote home")
        package = root / "package"
        packager = repo.parent / "aizen-mnemos/scripts/package.py"
        # Cargo debug artifacts can be hardlinked into deps; keep the packager's rejection intact.
        prebuilt = root / "prebuilt-mnemos.exe"
        shutil.copyfile(mnemos, prebuilt)
        require(support.sha256(prebuilt) == report["mnemos_sha256"], "fixture binary copy differs")
        run("package", [sys.executable, "-I", "-B", packager, "--binary", prebuilt,
                        "--target", "x86_64-pc-windows-msvc", "--output", package])
        archives = list(package.glob("*.zip"))
        require(len(archives) == 1, "expected one fixture package")
        run("install", [core, "plugin", "install", archives[0]], offline=True)
        installed = snapshot("installed")
        pkg = installed["packages"][0]
        require(pkg["id"] == "mnemos" and pkg["valid"], "installed package missing")
        grant = {"manifest_sha256": pkg["manifest_sha256"], "guarded_execution": True,
                 "network": False, "learning": False, "turn_model_learning": False}
        activation = review("activation-review", installed, {"type": "activate", "provider": "mnemos", "grant": grant})
        require(snapshot("cancelled-review")["binding"] == installed["binding"], "review mutated selection")
        active = apply("activation", activation)
        require(active["enabled"] and active["selected"] == "mnemos" and not active["learning"]["effective"], "activation granted learning")
        require(not default.exists(), "offline activation initialized data")
        stale = client("stale-activation", {"type": "apply", "review": activation}, success=False)
        require(stale["code"] == "stale", f"unexpected conflict: {stale}")
        active = change("custom-storage", active, {"type": "storage", "provider": "mnemos", "root": str(custom)})
        require(not custom.exists() and not default.exists(), "offline storage initialized data")
        invalid = client("unsafe-storage", {"type": "review", "binding": active["binding"], "change": {"type": "storage", "provider": "mnemos", "root": str(home)}}, success=False)
        require(invalid["code"], "unsafe path accepted")
        record = json.loads(run("seed", [core, "--sandbox", "guarded", "memory", "add", "client-fixture", "--body", "Remember the cobalt observatory project convention."]))["record"]
        listed = read("list", active, {"type": "list", "scope": "project", "limit": 100})
        require(any(r["id"] == record["id"] for r in listed["records"]), "actual record not listed")
        detail = read("get", active, {"type": "get", "scope": "project", "id": record["id"]})
        require(detail["records"][0]["content"] == record["content"], "detail mismatch")
        hits = read("search", active, {"type": "search", "scope": "project", "limit": 100, "query": "cobalt observatory"})
        require(any(r["id"] == record["id"] for r in hits["records"]), "search did not retrieve fixture")
        read("wrong-scope", active, {"type": "get", "scope": "user", "id": record["id"]}, success=False)
        other_snap = snapshot("other-snapshot", other)
        read("foreign-project", other_snap, {"type": "get", "scope": "project", "id": record["id"]}, success=False, path=other)
        read("wrong-workspace-binding", active, {"type": "list", "scope": "project", "limit": 10}, success=False, path=other)
        custom.joinpath("preserve.txt").write_bytes(b"custom canary")
        before = support.inventory(custom)
        reset = change("default-storage", active, {"type": "storage", "provider": "mnemos", "root": None})
        require(support.inventory(custom) == before and not default.exists(), "storage reset changed data")
        read("old-context", active, {"type": "list", "scope": "project", "limit": 10}, success=False)
        active = change("restore-storage", reset, {"type": "storage", "provider": "mnemos", "root": str(custom)})
        off_review = review("off-review", active, {"type": "learning", "enabled": False})
        tampered = copy.deepcopy(off_review)
        tampered["change"]["enabled"] = True
        client("tampered-review", {"type": "apply", "review": tampered}, success=False)
        active = apply("learning-off", off_review)
        require(not active["learning"]["policy"], "policy not persisted")
        disabled = change("disable", active, {"type": "disable"})
        client("disabled-no-launch", {"type": "read", "binding": disabled["binding"], "query": {"type": "list", "scope": "project", "limit": 100}}, success=False)
        run("uninstall", [core, "plugin", "uninstall", "mnemos", "--yes"], offline=True)
        require(custom.joinpath("preserve.txt").read_bytes() == b"custom canary", "uninstall removed data")
        require(not default.exists(), "default root unexpectedly initialized")
        require(support.sha256(core) == report["core_sha256"] and support.sha256(mnemos) == report["mnemos_sha256"], "binaries changed")
        require(not runner.cleanup_failures, "owned cleanup failed")
        report["passed"] = True
    except Exception:
        report["error"] = traceback.format_exc()
        raise
    finally:
        report["cleanup_failures"] = runner.cleanup_failures
        save()
    print(f"PASS: {len(report['cases'])} commands; {root / 'report.json'}", flush=True)


if __name__ == "__main__":
    main()
