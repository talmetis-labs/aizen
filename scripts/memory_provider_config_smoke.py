"""Drive real Core provider configuration with disposable HOME and a synthetic custom package.

Run Python -I with explicit prebuilt Core/example-provider binaries. No actual user configuration,
external model, download or adoption is used. Each command has an owned bounded Windows Job.
"""
from pathlib import Path
import argparse
import ctypes
import importlib.util
import json
import shutil
import sys
import tempfile
import traceback


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path)
    parser.add_argument("provider", type=Path)
    args = parser.parse_args()
    if sys.flags.isolated != 1 or sys.platform != "win32" or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise SystemExit("Run with Python -I on Windows x86_64")
    repo = Path(__file__).resolve().parents[1]
    spec = importlib.util.spec_from_file_location("owned_config_support", repo / "scripts/memory_agent_smoke.py")
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    require = support.require
    core, provider = args.core.resolve(strict=True), args.provider.resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix="aizen-provider-config-smoke-"))
    env = support.isolated_environment(root)
    env["HF_HOME"] = str(root / "external-hf")
    env["AIZEN_EMBED_MODEL"] = str(root / "external-embed-model")
    workspace = root / "workspace"
    workspace.mkdir()
    (workspace / ".git").write_text("gitdir: ../main/.git/worktrees/fixture\n", encoding="utf-8")
    home = Path(env["AIZEN_HOME"])
    data = root / "custom-provider-data"
    default_data = home / "memory-data/example-memory"
    runner = support.OwnedProcessRunner(root, workspace, env, 90)
    report = {"root":str(root), "passed":False, "cases":[], "core_sha256":support.sha256(core),
              "provider_sha256":support.sha256(provider), "script_sha256":support.sha256(Path(__file__)),
              "limitations":["CLI application coverage, not interactive TUI navigation", "Windows guarded does not confine filesystem/network"]}
    print(f"Provider configuration application fixture: {root}",flush=True)

    def save():
        (root / "report.json").write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    def run(name, *command, success=True):
        result = runner.run(name,[str(core),*map(str,command)])
        report["cases"].append({"name":name,**{k:v for k,v in result.items() if k not in ("stdout","stderr")}})
        save()
        require((result["exit"] == 0) == success, f"{name}: {result['exit']}: {result['stdout'][-1000:]} {result['stderr'][-2000:]}")
        require(support.sha256(core) == report["core_sha256"], "Core changed during smoke")
        if command[:2] in (("plugin", "configure"), ("plugin", "storage")):
            require(result["job_processes"] == 1, f"{name}: offline configuration launched a provider")
        if name == "select":
            require(f"SHA256 {report['provider_sha256']}" in result["stderr"],
                    "consent disclosure omitted the executable hash")
            require(f"Pinned manifest SHA256: {support.sha256(source / 'plugin.json')}" in result["stderr"],
                    "consent disclosure confused manifest and executable hashes")
        return result["stdout"]
    def state():
        return json.loads((home / "cli-config.json").read_bytes())["memory_plugins"]
    def configure(name, *flags):
        text = run(name,"plugin","configure","example-memory",*flags)
        return json.loads(text[text.index("{"):])
    def same_path(actual, expected):
        # Rust canonical paths may carry Windows' verbatim prefix; compare the same disk path.
        return Path(str(actual).removeprefix("\\\\?\\")).resolve() == expected.resolve()
    def storage(name, *flags, provider_id="example-memory", expected=None):
        value = json.loads(run(name,"plugin","storage",provider_id,*flags))
        automatic = expected is None
        expected_root = home / "memory-data" / provider_id if automatic else expected
        require(value["provider"] == provider_id and value["automatic"] is automatic
                and value["data_moved"] is False and same_path(value["data_root"], expected_root),
                f"{name}: unexpected storage response: {value}")
        saved = state()["packages"][provider_id].get("data_root")
        require(saved is None if automatic else saved is not None and same_path(saved, expected_root),
                f"{name}: saved per-package storage mismatch")
        return value
    def health(name):
        return json.loads(run(name,"--sandbox","guarded","memory","health"))
    try:
        run("config-memory-help","config","memory","--help")
        run("absent-status","memory","status","--json")
        require(not home.exists(), "offline help/status created HOME config")
        source = root / "source"
        source.mkdir()
        shutil.copyfile(provider,source / "provider.exe")
        schema = {
            "enable_fuzzy":{"label":"Fuzzy matching","description":"Fixture option","type":"boolean","default":True},
            "days":{"label":"Days","description":"Fixture tuning","type":"number","default":30.0,"min":1.0,"max":3650.0},
            "count":{"label":"Count","description":"Fixture count","type":"integer","default":12,"min":0,"max":100},
            "mode":{"label":"Mode","description":"Nonsecret fixture mode","type":"enum","default":"local","choices":["local","off"]},
        }
        manifest = {"schema_version":1,"id":"example-memory","name":"Configuration fixture","version":"0.1.0","kind":"memory",
            "protocol":{"major":1,"min_minor":0,"max_minor":0},"target":"x86_64-pc-windows-msvc","entrypoint":"provider.exe",
            "capabilities":["records","recall","context"],"permissions":{"network":False,"host_llm":False},"settings":schema,
            "files":[{"path":"provider.exe","bytes":(source / "provider.exe").stat().st_size,"sha256":support.sha256(source / "provider.exe")}]}
        (source / "plugin.json").write_text(json.dumps(manifest),encoding="utf-8")
        run("install-custom","plugin","install",source)
        initial = configure("inspect-defaults")
        run("learning-policy-off", "config", "set", "--memory-auto-learn", "false")
        policy_off = json.loads((home / "cli-config.json").read_bytes())
        require(policy_off["memory_auto_learn"] is False, "explicit Learning Off did not persist")
        run("unrelated-config-preserves-learning", "config", "set", "--compact-threshold", "0")
        unrelated = json.loads((home / "cli-config.json").read_bytes())
        require(unrelated["memory_auto_learn"] is False
                and unrelated["memory_plugins"] == policy_off["memory_plugins"],
                "unrelated config save changed Learning policy or generation")
        run("learning-policy-on", "config", "set", "--memory-auto-learn", "true")
        policy_on = json.loads((home / "cli-config.json").read_bytes())
        require(policy_on["memory_auto_learn"] is True
                and policy_on["memory_plugins"]["generation"] > policy_off["memory_plugins"]["generation"],
                "explicit Learning On did not invalidate stale work")
        require(not policy_on["memory_plugins"]["enabled"]
                and policy_on["memory_plugins"]["selected"] is None
                and policy_on["memory_plugins"]["packages"]["example-memory"]["grant"] is None,
                "policy-only writer granted execution or activated a provider")
        require(initial["effective_settings"] == {"enable_fuzzy":True,"days":30.0,"count":12,"mode":"local"}, "defaults mismatch")
        require(initial["budgets"] == {"context_tokens":1024,"recall_tokens":1024,"recall_inject_tokens":300,"recall_limit":12,"max_records":100}, "host defaults mismatch")
        storage("inspect-default-storage")
        storage("save-custom-storage", "--path", data, expected=data)
        storage("inspect-custom-storage", expected=data)
        storage("reset-unused-storage", "--default")
        storage("restore-unused-custom-storage", "--path", data, expected=data)
        require(not data.exists() and not (home / "memory-data").exists(), "offline storage created a provider root")
        edited = configure("edit-custom", "--set","enable_fuzzy=false","--set","days=45.5","--set","count=3","--set",'mode="off"',
            "--context-tokens","600","--recall-tokens","400","--recall-inject-tokens","200","--recall-limit","5","--max-records","20")
        require(edited["effective_settings"]["days"] == 45.5, "edit did not save")
        before = (home / "cli-config.json").read_bytes()
        for name,flags in [
            ("reserved",["--set","prepared_job=true"]),("unknown",["--set","unknown=true"]),
            ("wrong-type",["--set",'count="3"']),("range",["--set","count=101"]),
            ("duplicate",["--set","count=2","--set","count=3"]),("conflicting",["--set","count=2","--reset","count"]),
            ("object",["--set","days={}"]),("budget-range",["--context-tokens","4097"]),
        ]:
            run("reject-"+name,"plugin","configure","example-memory",*flags,success=False)
            require((home / "cli-config.json").read_bytes() == before, "invalid edit changed saved config")
        for name,flags in [
            ("relative", ["--path", "relative-provider-data"]),
            ("workspace", ["--path", workspace]), ("aizen-home", ["--path", home]),
            ("home", ["--path", Path(env["HOME"])]),
            ("workspace-git", ["--path", workspace / ".git/records"]),
            ("workspace-control", ["--path", workspace / ".aizen/records"]),
            ("packages", ["--path", home / "plugins"]),
            ("package", ["--path", home / "plugins/example-memory"]),
            ("package-child", ["--path", home / "plugins/example-memory/data"]),
            ("control", ["--path", home / "plugin-state"]),
            ("control-child", ["--path", home / "plugin-state/data"]),
            ("private-temp", ["--path", home / "plugin-temp"]),
            ("models", ["--path", home / "models"]),
            ("models-child", ["--path", home / "models/data"]),
            ("external-model", ["--path", Path(env["AIZEN_EMBED_MODEL"])]),
            ("external-model-child", ["--path", Path(env["AIZEN_EMBED_MODEL"]) / "data"]),
            ("hf-model", ["--path", Path(env["HF_HOME"]) / "hub"]),
            ("hf-model-child", ["--path", Path(env["HF_HOME"]) / "hub/data"]),
            ("runtime", ["--path", Path(env["SystemRoot"]) / "provider-data"]),
            ("traversal", ["--path", str(workspace) + "/../escape"]),
            ("device", ["--path", "\\\\.\\C:\\provider-data"]),
            ("reserved-name", ["--path", workspace / "NUL"]),
            ("ads", ["--path", str(workspace) + "/data:stream"]),
            ("conflicting-flags", ["--path", data, "--default"]),
        ]:
            run("reject-storage-"+name,"plugin","storage","example-memory",*flags,success=False)
            require((home / "cli-config.json").read_bytes() == before, "invalid storage changed saved config")
            require(not data.exists() and not (home / "memory-data").exists(), "rejected storage initialized data")
        run("eligible","plugin","enable","example-memory")
        run("no-guarded-consent","memory","use","example-memory","--yes",success=False)
        run("unsupported-model-consent","memory","use","example-memory","--yes","--allow-guarded","--allow-learning","--allow-turn-model",success=False)
        run("select","memory","use","example-memory","--yes","--allow-guarded")
        require(not data.exists() and not (home / "memory-data").exists(), "selection executed provider")
        actual = health("verify-initialize")["configuration"]
        require(data.is_dir() and not default_data.exists(), "runtime health initialized the wrong storage root")
        require(actual["settings"]["days"] == 45.5 and actual["settings"]["enable_fuzzy"] is False, "initialize options mismatch")
        require(actual["budgets"] == {"context_tokens":600,"recall_tokens":400,"max_records":20}, "initialize budgets mismatch")
        require("prepared_job" not in actual["settings"], "configuration minted authority")
        body = "CUSTOM_STORAGE_AMBER_LANTERN_CANARY"
        record = json.loads(run("save-at-custom-root","--sandbox","guarded","memory","add",
                                "storage-fixture","--body",body))["record"]
        require(record["content"] == body and body in (data / "records.json").read_text(encoding="utf-8"),
                "runtime save did not use custom data root")
        require(json.loads(run("read-at-custom-root","--sandbox","guarded","memory","show",record["id"]))["record"] == record,
                "runtime read lost custom-root record")
        require(not default_data.exists(), "custom runtime initialized the default root")
        sentinel = data / "preserve.txt"
        sentinel.write_bytes(b"provider data canary")
        custom_before = support.inventory(data)
        storage("reset-populated-storage", "--default")
        require(support.inventory(data) == custom_before and not default_data.exists(), "reset moved/copied/deleted custom data")
        runner.workspace = Path(env["HOME"])
        try:
            health("default-storage-from-home-workspace")
        finally:
            runner.workspace = workspace
        require(json.loads(run("default-store-is-empty","--sandbox","guarded","memory","list"))["items"] == [],
                "reset copied custom records into default storage")
        require(support.inventory(data) == custom_before, "default runtime changed custom data")
        default_sentinel = default_data / "preserve-default.txt"
        default_sentinel.write_bytes(b"default data canary")
        default_before = support.inventory(default_data)
        storage("restore-populated-custom-storage", "--path", data, expected=data)
        require(support.inventory(data) == custom_before and support.inventory(default_data) == default_before,
                "storage restore moved/copied/deleted data")
        require(json.loads(run("read-restored-custom-root","--sandbox","guarded","memory","show",record["id"]))["record"] == record,
                "custom record did not survive default/reset cycle")
        run("disable","memory","disable")
        require(sentinel.read_bytes() == b"provider data canary" and default_sentinel.read_bytes() == b"default data canary",
                "disable changed storage canaries")
        disabled = state()
        configure("edit-while-disabled","--reset","days")
        require(not state()["enabled"] and state()["selected"] == "example-memory", "edit reactivated Memory")
        run("disabled-health","--sandbox","guarded","memory","health",success=False)
        run("reenable","memory","enable")
        require(health("reset-reaches-initialize")["configuration"]["settings"]["days"] == 30.0, "reset not propagated")
        run("clear-selection","memory","unselect")
        cleared = state()
        require(cleared["selected"] is None and not cleared["enabled"] and cleared["packages"]["example-memory"]["grant"] is not None, "clear erased grant or left active")
        run("no-selection-enable","memory","enable",success=False)
        run("reselect","memory","use","example-memory","--yes","--allow-guarded")
        # A second package is inactive; switching never copies options or chooses a fallback.
        other = root / "other-source"
        other.mkdir()
        shutil.copyfile(provider,other / "provider.exe")
        manifest["id"] = "other-memory"
        (other / "plugin.json").write_text(json.dumps(manifest),encoding="utf-8")
        run("install-other","plugin","install",other)
        storage("inspect-other-default-storage", provider_id="other-memory")
        other_data = root / "other-provider-data"
        storage("save-other-custom-storage", "--path", other_data, provider_id="other-memory", expected=other_data)
        storage("original-storage-is-independent", expected=data)
        before = (home / "cli-config.json").read_bytes()
        for name, candidate in (("other-default", home / "memory-data/other-memory"),
                                ("other-custom", other_data), ("other-child", other_data / "nested")):
            run("reject-storage-"+name,"plugin","storage","example-memory","--path",candidate,success=False)
            require((home / "cli-config.json").read_bytes() == before, "overlapping storage changed saved config")
            require(not other_data.exists(), "offline other storage was created")
        run("eligible-other","plugin","enable","other-memory")
        run("select-other","memory","use","other-memory","--yes","--allow-guarded")
        current = state()
        require(current["packages"]["other-memory"].get("settings", {}) == {}, "other inherited overrides")
        require(current["packages"]["example-memory"]["settings"]["count"] == 3, "switch lost previous options")
        require(not other_data.exists(), "selecting another package executed it")
        run("clear-other","memory","unselect")
        run("reselect-original","memory","use","example-memory","--yes","--allow-guarded")
        payload = home / "plugins/example-memory/provider.exe"
        with payload.open("ab") as stream:
            stream.write(b"fixture corruption")
        run("revoke-damaged","plugin","revoke","example-memory")
        revoked = state()
        require(not revoked["enabled"] and revoked["selected"] == "example-memory" and revoked["packages"]["example-memory"]["grant"] is None,"damaged revocation failed")
        run("revoked-enable-refused","memory","enable",success=False)
        run("damaged-uninstall-refused","plugin","uninstall","example-memory","--yes",success=False)
        require(sentinel.read_bytes() == b"provider data canary", "data canary changed")
        # Restore only the exact test-owned executable bytes to permit conservative removal.
        payload.write_bytes(provider.read_bytes())
        custom_before, default_before = support.inventory(data), support.inventory(default_data)
        run("uninstall-original","plugin","uninstall","example-memory","--yes")
        run("uninstall-other","plugin","uninstall","other-memory","--yes")
        require(state()["packages"] == {} and state()["selected"] is None, "uninstall left active selection")
        require(support.inventory(data) == custom_before and support.inventory(default_data) == default_before,
                "uninstall moved/copied/deleted custom or default provider data")
        require(sentinel.read_bytes() == b"provider data canary" and default_sentinel.read_bytes() == b"default data canary",
                "uninstall erased storage canaries")
        require(not (home / "cli-memory").exists(), "legacy store initialized")
        require(not (home / "plugin-state/memory-transaction.json").exists(), "settlement barrier left behind")
        require(not runner.cleanup_failures, "owned process cleanup failed")
        report["storage"] = {"default_per_package":True,"offline_no_launch_or_root_creation":True,
            "custom_runtime_save_read":True,"reset_default_empty_restore_preserved":True,
            "unsafe_and_overlapping_roots_rejected":True,"disable_uninstall_preserve_both_roots":True}
        report["passed"] = True
    except Exception:
        report["error"] = traceback.format_exc()
        print(report["error"],flush=True)
    finally:
        report["cleanup_failures"] = runner.cleanup_failures
        report["core_unchanged"] = support.sha256(core) == report["core_sha256"]
        report["provider_unchanged"] = support.sha256(provider) == report["provider_sha256"]
        save()
        print(f"{'PASS' if report['passed'] else 'FAIL'}: {root / 'report.json'}",flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
