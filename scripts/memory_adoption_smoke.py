"""Core/Mnemos adoption integration using only disposable homes and synthetic records.

Run with python -I; --core and --engine must name freshly built Windows MSVC binaries.
No installed package, user config, legacy store, model endpoint or GUI is used.
"""
from pathlib import Path
import argparse
import importlib.util
import json
import shutil
import sys
import tempfile
import traceback

assert sys.flags.isolated == 1 and sys.platform == "win32"
REPO = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("owned_support", REPO / "scripts/memory_agent_smoke.py")
support = importlib.util.module_from_spec(spec)
spec.loader.exec_module(support)
require = support.require
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--core", required=True, type=Path)
parser.add_argument("--engine", required=True, type=Path)
parser.add_argument("--large-backup", action="store_true", help="Exercise a realistic 314+ MiB derived cache using streaming fixture writes")
args = parser.parse_args()
core_source = args.core.resolve(strict=True)
engine_source = args.engine.resolve(strict=True)
root = Path(tempfile.mkdtemp(prefix="aizen-memory-adoption-"))
# Freeze binaries for this run; concurrent builds must not change later client invocations.
core, engine = root / "core.exe", root / "mnemos.exe"
for original, frozen in ((core_source, core), (engine_source, engine)):
    before_copy = support.sha256(original)
    shutil.copyfile(original, frozen)
    require(support.sha256(frozen) == before_copy == support.sha256(original), "binary changed during fixture snapshot")
env = support.isolated_environment(root)
env["AIZEN_GIT"] = str(root / "no-git-discovery.exe")
workspace = root / "workspace"
workspace.mkdir()
home = Path(env["AIZEN_HOME"])
home.mkdir(parents=True, exist_ok=True)
runner = support.OwnedProcessRunner(root, workspace, env, 240)
wire = root / "wire"
wire.mkdir()
report = {"root": str(root), "passed": False, "core_source": str(core_source),
          "engine_source": str(engine_source), "core_sha256": support.sha256(core),
          "engine_sha256": support.sha256(engine), "cases": [], "large_backup": args.large_backup,
          "limitations": ["Synthetic CLI integration, not native GUI E2E",
                          "Host-registration loss is injected in a disposable operation record; no process crash is injected"]}
active = None
authorized_attachments = []
print("Adoption smoke root:", root, flush=True)


def save():
    (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


def run(name, argv, stdin=None, offline=False):
    result = runner.run(name, list(map(str, argv)), stdin)
    report["cases"].append({"name": name, **{k: v for k, v in result.items() if k not in ("stdout", "stderr")}})
    save()
    require(result["exit"] == 0, f'{name}: exit {result["exit"]}: {result["stderr"][-2000:]}')
    require(result["job_settled"], f"{name}: process tree unsettled")
    if offline:
        require(result["job_processes"] == 1, f"{name}: offline request launched child")
    return result["stdout"]


def client(name, action, offline=False, expect=True):
    request = {"version": 1, "request_id": name, "workspace": str(workspace), "action": action}
    raw = run(name, [core, "memory", "client"], json.dumps(request).encode(), offline)
    (wire / (name + ".request.json")).write_text(json.dumps(request), encoding="utf-8")
    (wire / (name + ".response.json")).write_text(raw, encoding="utf-8")
    response = json.loads(raw)
    require(response["request_id"] == name and response["version"] == 1, name + ": envelope mismatch")
    if expect is not None:
        require(response["ok"] == expect, f"{name}: {response}")
    return response["data"] if response["ok"] else {"error": response["error"]}


def adoption(name, request, offline=False, expect=True, binding=None):
    data = client(name, {"type": "adoption", "binding": binding or active["binding"], "request": request}, offline, expect)
    return data.get("adoption", data)


def inspect(name, query):
    return client(name, {"type": "inspect", "binding": active["binding"], "query": query})["result"]


def switch(name, path):
    global active
    review = client(name + "-review", {"type": "review", "binding": active["binding"],
        "change": {"type": "storage", "provider": "mnemos", "root": str(path)}}, offline=True)["review"]
    active = client(name + "-apply", {"type": "apply", "review": review}, offline=True)["snapshot"]


def hashes(path):
    return {p.relative_to(path).as_posix(): support.sha256(p) for p in path.rglob("*") if p.is_file()}


def preview(name, source=None, destination=None):
    return adoption(name, {"action": "preview", "source": str(source) if source else None,
                           "destination_parent": str(destination) if destination else None,
                           "attachments": authorized_attachments, "consent": True})["operation"]


def confirmed(action, operation):
    return {"action": action, "operation_id": operation["operation_id"],
            "reviewed_digest": operation["reviewed_digest"], "consent": True,
            **({"writers_stopped": True} if action in ("apply", "resume") else {})}


try:
    active = client("snapshot-empty", {"type": "snapshot"}, offline=True)
    require("adoption" in active["client_features"], "Core missing adoption feature")
    before = hashes(home)
    defaults = adoption("defaults-empty", {"action": "defaults"}, offline=True)
    require(defaults["kind"] == "defaults" and not defaults["enabled"], "defaults must work without a provider")
    require(Path(defaults["source"]) == home / "cli-memory", "default source ignored isolated AIZEN_HOME")
    require(hashes(home) == before, "defaults wrote files")
    adoption("disabled-preview", {"action": "preview", "source": None, "destination_parent": None, "consent": True}, offline=True, expect=False)

    prebuilt = root / "engine.exe"
    shutil.copyfile(engine, prebuilt)
    run("package", [sys.executable, "-I", "-B", REPO.parent / "aizen-mnemos/scripts/package.py",
                    "--binary", prebuilt, "--target", "x86_64-pc-windows-msvc", "--output", root / "package"])
    archive = next((root / "package").glob("*.zip"))
    installed = client("install", {"type": "install_local", "binding": active["binding"],
                        "source": str(archive), "expected_id": "mnemos"}, offline=True)
    active = installed["snapshot"]
    require(active["selected"] is None, "install silently selected provider")
    pkg = active["packages"][0]
    require({"legacy_adoption_v1", "legacy_adoption_v2"}.issubset(pkg["capabilities"]), "missing versioned adoption declaration")
    grant = {"manifest_sha256": pkg["manifest_sha256"], "guarded_execution": True,
             "network": False, "learning": False, "turn_model_learning": False}
    review = client("activate-review", {"type": "review", "binding": active["binding"],
                    "change": {"type": "activate", "provider": "mnemos", "grant": grant}}, offline=True)["review"]
    active = client("activate", {"type": "apply", "review": review}, offline=True)["snapshot"]
    flt = {"scope": "current", "lifecycle": "all", "query": "", "kind": None}
    refs = []
    for i, scope in enumerate(("user", "project", "user")):
        summary = inspect(f"before-create-{i}", {"action": "summary", "filter": flt})
        draft = {"name": f"adoption-fixture-{i}", "description": "Synthetic migration fixture",
                 "content": f"Synthetic observatory rule {i}.", "kind": "project", "scope": scope}
        result = client(f"create-{i}", {"type": "manage", "binding": active["binding"],
                        "operation_id": f"create-{i}", "command": {"filter": flt,
                        "expected_snapshot": summary["snapshot"], "action": {"action": "create", "draft": draft}}})["result"]
        refs.append(result["created"])
    current = Path(active["packages"][0]["storage"]["root"])
    source = home / "cli-memory"
    (source / "entries").mkdir(parents=True)
    for ref in refs[:2]:
        shutil.copyfile(current / "entries" / (ref["id"] + ".md"), source / "entries" / (ref["id"] + ".md"))
    # An existing marker prevents unrelated engine ID normalization from confusing this fixture.
    for marker in (".id-slug-v2",):
        if (current / marker).is_file():
            shutil.copyfile(current / marker, source / marker)
    (source / "graph.tsv").write_text(
        f'{refs[0]["id"]}\t{refs[1]["id"]}\t1.75\t2026-11-03\n', encoding="utf-8")
    (source / "codebase").mkdir()
    cache_fixture = json.dumps({"version": 2, "root": str(workspace), "built_unix": 0, "files": []}) + "\n"
    (source / "codebase" / "synthetic.json").write_text(cache_fixture, encoding="utf-8")
    backup_only = {"codebase/synthetic.json"}
    if args.large_backup:
        # Reproduce the measured legacy shape, not any real cache or Memory contents.
        # JSON whitespace makes a valid huge derived index without a file-sized allocation.
        large = source / "codebase" / "synthetic-large.json"
        target_bytes = 329_474_395
        prefix = cache_fixture.rstrip().encode("utf-8")
        with large.open("xb") as output:
            output.write(prefix)
            remaining = target_bytes - len(prefix)
            chunk = b" " * (1024 * 1024)
            while remaining:
                count = min(len(chunk), remaining)
                output.write(chunk[:count])
                remaining -= count
        require(large.stat().st_size == target_bytes, "large cache fixture size differs")
        backup_only.add("codebase/synthetic-large.json")
        for name in ("synthetic.json.lock", "synthetic-large.json.lock", "synthetic-old.json.lock"):
            (source / "codebase" / name).write_bytes(b"")
            backup_only.add("codebase/" + name)
    (source / "archive").mkdir()
    for name, payload in (("synthetic-repair.json", b'{"fixture":"backup only"}\n'),
                          ("synthetic-note.txt", b"Synthetic archive attachment; not Memory.\n")):
        path = source / "archive" / name
        path.write_bytes(payload)
        backup_only.add("archive/" + name)
        authorized_attachments.append({"path": "archive/" + name, "bytes": len(payload),
                                       "sha256": support.sha256(path)})
    authorized_attachments.sort(key=lambda item: item["path"])
    source_before, current_before = hashes(source), hashes(current)
    settings_before = (active["binding"]["generation"], active["learning"], active["selected"])
    defaults = adoption("defaults-v2", {"action": "defaults"}, offline=True)
    require(defaults["attachment_review_supported"], "v2 attachment review unavailable")
    unreviewed = adoption("preview-needs-review", {"action": "preview", "source": None,
                         "destination_parent": None, "consent": True})["operation"]
    require(unreviewed["state"] == "needs_review" and not unreviewed["reviewed_digest"], str(unreviewed))
    require(unreviewed["attachments"] == authorized_attachments, "attachment candidates differ")
    require(not Path(unreviewed["data_root"]).exists(), "review request published a store")
    altered = [dict(item) for item in authorized_attachments]
    altered[0]["sha256"] = "0" * 64
    rejected = adoption("preview-tampered-attachment", {"action": "preview", "source": None,
                        "destination_parent": None, "attachments": altered, "consent": True}, expect=None)
    require(rejected.get("operation", {}).get("state") == "check_failed"
            and rejected["operation"].get("diagnostic", {}).get("code") == "attachment_authorization",
            "changed attachment did not produce the closed authorization diagnostic")
    operation = preview("preview-default")
    require(operation["state"] == "preview", str(operation))
    require(not Path(operation["data_root"]).exists(), "preview precreated destination")
    require(hashes(source) == source_before and hashes(current) == current_before, "preview changed stores")
    inventory = adoption("inventory-page", {"action": "inventory", "operation_id": operation["operation_id"], "offset": 0, "limit": 2}, offline=True)
    require(len(inventory["items"]) <= 2 and inventory["total"] > 2, "inventory pagination lost")
    denied = confirmed("apply", operation)
    denied["consent"] = False
    adoption("apply-no-consent", denied, offline=True, expect=False)
    writers_active = confirmed("apply", operation)
    writers_active["writers_stopped"] = False
    adoption("apply-writers-active", writers_active, offline=True, expect=False)
    tampered = confirmed("apply", operation)
    tampered["reviewed_digest"] = "0" * 64
    adoption("apply-wrong-digest", tampered, offline=True, expect=False)
    applied = adoption("apply-default", confirmed("apply", operation))["operation"]
    require(applied["state"] == "published_verified", str(applied))
    imported = Path(applied["data_root"])
    require(imported.is_dir() and Path(applied["backup"]).is_dir(), "missing published store or backup")
    for rel, digest in source_before.items():
        require(support.sha256(Path(applied["backup"]) / rel) == digest, "backup mismatch: " + rel)
        if rel not in backup_only:
            require(support.sha256(imported / rel) == digest, "copied file mismatch: " + rel)
        else:
            require(not (imported / rel).exists(), "backup-only payload imported: " + rel)
    require(not (imported / "codebase").exists(), "Core cache copied into provider store")
    require(hashes(source) == source_before and hashes(current) == current_before, "import changed old stores")
    fresh = client("snapshot-after-import", {"type": "snapshot"}, offline=True)
    require((fresh["binding"]["generation"], fresh["learning"], fresh["selected"]) == settings_before, "import changed config or Learning")
    require(Path(fresh["packages"][0]["storage"]["root"]) == current, "import silently switched storage")
    # Every client call is a fresh Core process: status verifies persisted restart state.
    resumed_status = adoption("restart-status", {"action": "status", "operation_id": applied["operation_id"], "offset": 0, "limit": 10}, offline=True)
    require(resumed_status["operation"]["state"] == "published_verified", "terminal state not durable")
    # Inject only the lost host-registration boundary, after verified provider publication.
    # All files below belong to this disposable fixture; no real config/store is consulted.
    record_path = home / "plugin-state" / "adoption" / applied["operation_id"] / "operation.json"
    record = json.loads(record_path.read_text(encoding="utf-8"))
    record["operation"]["state"] = "interrupted"
    record["published_identity"] = None
    record_path.write_text(json.dumps(record), encoding="utf-8")
    client("unregistered-storage", {"type": "review", "binding": active["binding"],
           "change": {"type": "storage", "provider": "mnemos", "root": str(imported)}}, offline=True, expect=False)
    parked_source = home / "synthetic-source-parked"
    source.rename(parked_source)
    try:
        recovered = adoption("recover-without-source", confirmed("recover", applied))["operation"]
        require(recovered["state"] == "published_verified", str(recovered))
        require(hashes(parked_source) == source_before, "recovery changed parked source")
    finally:
        parked_source.rename(source)
    generation = active["binding"].copy()
    switch("use-import", imported)
    visible = inspect("imported-visible", {"action": "summary", "filter": flt})
    require(visible["counts"]["active"] == 2, str(visible))
    require(visible["nodes"] == 2 and visible["edges"] == 1, "imported graph visibility differs")
    require(active["learning"] == settings_before[1], "switch changed Learning")
    adoption("stale-binding", {"action": "defaults"}, offline=True, expect=False, binding=generation)
    switch("return-previous", current)
    require(inspect("previous-visible", {"action": "summary", "filter": flt})["counts"]["active"] == 3, "previous store lost")

    external = root / "custom-imports"
    external.mkdir()
    custom = preview("preview-custom", source, external)
    require(Path(custom["data_root"]).parent.parent.samefile(external), "custom parent ignored")
    # Even an excluded cache is bound by the immutable preview.
    changed_cache = json.loads(cache_fixture)
    changed_cache["built_unix"] = 1
    (source / "codebase" / "synthetic.json").write_text(json.dumps(changed_cache) + "\n", encoding="utf-8")
    drift = adoption("source-drift", confirmed("apply", custom), expect=None)
    require(drift.get("operation", {}).get("state") == "interrupted"
            and drift["operation"].get("diagnostic", {}).get("code") == "source_changed",
            "source drift did not produce the closed source-changed diagnostic")
    require(not Path(custom["data_root"]).exists(), "drift published destination")
    (source / "codebase" / "synthetic.json").write_text(cache_fixture, encoding="utf-8")
    # Restore identical content, then obtain a fresh plan because file metadata also changed.
    custom2 = preview("preview-custom-fresh", source, external)
    done = adoption("apply-custom", confirmed("apply", custom2))["operation"]
    require(done["state"] == "published_verified", str(done))
    restored = root / "restored-source"
    restore_journal = root / "restore-journal"
    restored_report = json.loads(run("restore-full-source", [engine, "adoption", "restore",
        "--journal", done["journal"], "--reviewed-digest", done["reviewed_digest"],
        "--destination", restored, "--restore-journal", restore_journal, "--machine-v2"]))
    require(restored_report["status"] == "ok" and restored_report["outcome"]["restored_all_source_files"], "restore not verified")
    require(hashes(restored) == source_before, "full-source restore differs")
    history = adoption("history", {"action": "status", "operation_id": None, "offset": 0, "limit": 100}, offline=True)
    require(history["total"] >= 3, "history lost operations")
    require(hashes(source) == source_before, "source bytes changed after test roundtrip")
    report["passed"] = True
except Exception:
    report["error"] = traceback.format_exc()
    raise
finally:
    report["cleanup_failures"] = runner.cleanup_failures
    save()
print("PASS", len(report["cases"]), "commands", root / "report.json", flush=True)
