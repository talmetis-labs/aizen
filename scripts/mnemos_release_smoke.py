"""Drive the actual Core release CLI/client using disposable Windows homes.

Run with Python -I and an explicit prebuilt Core executable. --live-check makes anonymous
read-only requests to the fixed public Mnemos release channel. Never installs or runs a provider.
"""
from pathlib import Path
import argparse
import importlib.util
import json
import sys
import tempfile
import traceback


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path)
    parser.add_argument("--live-check", action="store_true")
    args = parser.parse_args()
    if sys.flags.isolated != 1 or sys.platform != "win32":
        raise SystemExit("Run with Python -I on Windows")
    repo = Path(__file__).resolve().parents[1]
    spec = importlib.util.spec_from_file_location("owned_release_smoke", repo / "scripts/memory_agent_smoke.py")
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    core = args.core.resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix="aizen-mnemos-release-smoke-"))
    workspace = root / "workspace"
    workspace.mkdir()
    env = support.isolated_environment(root)
    home = Path(env["AIZEN_HOME"])
    runner = support.OwnedProcessRunner(root, workspace, env, 90)
    report = {"passed": False, "root": str(root), "core_sha256": support.sha256(core), "cases": [],
              "limitations": ["CLI/client evidence, not native GUI or provider execution.",
                              "No release ZIP is downloaded and no installation is attempted.",
                              "Public availability may change; no binary provenance or mirror-write test."]}
    print(f"Release smoke fixture: {root}", flush=True)

    def save():
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    def run(name, *command, stdin=None, success=True):
        result = runner.run(name, [str(core), *command], stdin)
        report["cases"].append({"name": name, **{key: value for key, value in result.items()
                                                 if key not in ("stdout", "stderr")}})
        save()
        support.require((result["exit"] == 0) is success,
                        f"{name}: exit {result['exit']}: {result['stderr'][-1500:]}")
        support.require(result["job_settled"] and result["job_processes"] == 1,
                        f"{name}: unexpected provider/child process or unsettled Job")
        support.require(not home.exists(), f"{name}: created configuration or Memory data")
        return result["stdout"]

    def client(name, action):
        request = {"version": 1, "request_id": name, "workspace": str(workspace), "action": action}
        value = json.loads(run(name, "memory", "client", stdin=json.dumps(request).encode()))
        support.require(value["version"] == 1 and value["request_id"] == name and value["ok"],
                        f"{name}: {value}")
        return value["data"]

    try:
        help_text = run("plugin-help", "plugin", "--help")
        support.require("install-release" in help_text and "releases" in help_text, "release commands absent")
        run("install-help", "plugin", "install-release", "--help")
        before = json.loads(run("status-before", "memory", "status", "--json"))
        run("unpinned-refused", "plugin", "install-release", "mnemos", "--yes", success=False)
        snap = client("offline-snapshot", {"type": "snapshot"})
        support.require("mnemos_releases_v1" in snap["client_features"], "release feature absent")
        if args.live_check:
            value = json.loads(run("public-release-json", "plugin", "releases", "mnemos", "--json"))
            support.require(value["repository"] == "talmetis-labs/aizen-mnemos-releases", "wrong public channel")
            support.require(value["installed"] is None, "unexpected installed fixture provider")
            report["public_status"] = value
            if value.get("highest_seen_version") == "1.0.0" and value.get("candidate") is None:
                run("metadata-only-not-installable", "plugin", "install-release", "mnemos",
                    "--version", "1.0.0", "--sha256", "0" * 64, "--yes", success=False)
            checked = client("public-client-check", {"type": "release_check", "binding": snap["binding"]})
            support.require(checked["binding"] == snap["binding"], "release check changed binding")
            support.require(checked["release"]["repository"] == value["repository"], "client channel differs")
        after = json.loads(run("status-after", "memory", "status", "--json"))
        support.require(before == after, "read-only release operations changed provider state")
        support.require(support.sha256(core) == report["core_sha256"], "Core binary changed")
        report["passed"] = True
        save()
        print("PASS " + str(root / "report.json"), flush=True)
        return 0
    except BaseException:
        report["error"] = traceback.format_exc()
        save()
        traceback.print_exc()
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
