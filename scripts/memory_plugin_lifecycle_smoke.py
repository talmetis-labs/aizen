"""Exercise native Memory package lifecycle with a prebuilt, unchanged Windows Core.

Only disposable test-owned HOME/data roots are used. With --exercise-provider, the explicitly
supplied fixture provider runs under the Core's guarded sandbox. No external model/network or
real Memory store is used. Every command is enclosed in its own bounded, owned Windows Job.
Run with Python -I and explicit prebuilt executable arguments.
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path)
    parser.add_argument("example_provider", type=Path)
    parser.add_argument("--exercise-provider", action="store_true", help="Execute the explicitly supplied provider in fixture-owned roots")
    parser.add_argument("--provider-id", default="example-memory", choices=["example-memory", "mnemos"])
    parser.add_argument("--package", type=Path, help="Install an already built ZIP instead of using the Core's generic packager")
    args = parser.parse_args()
    if sys.flags.isolated != 1 or sys.platform != "win32" or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise SystemExit("Run with Python -I on Windows x86_64")
    repo = Path(__file__).resolve().parents[1]
    # Reuse the reviewed suspended-create/Job containment and allowlisted environment, not Popen
    # with a timeout that could leave the provider's descendants behind. This is owned test code.
    support_path = repo / "scripts/memory_agent_smoke.py"
    spec = importlib.util.spec_from_file_location("owned_memory_smoke_support", support_path)
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    require, sha256 = support.require, support.sha256
    provider_id = args.provider_id
    core = args.core.resolve(strict=True)
    provider = args.example_provider.resolve(strict=True)
    require(core.is_file() and provider.is_file() and core != provider, "two distinct binaries required")
    root = Path(tempfile.mkdtemp(prefix="aizen-memory-lifecycle-"))
    workspace = root / "workspace"
    workspace.mkdir()
    (workspace / ".git").mkdir()
    env = support.isolated_environment(root)
    aizen = Path(env["AIZEN_HOME"])
    runner = support.OwnedProcessRunner(root, workspace, env, 90)
    report = {"root": str(root), "passed": False, "provider_id": provider_id,
              "core_sha256_before": sha256(core), "provider_sha256_before": sha256(provider),
              "script_sha256": sha256(Path(__file__).resolve()),
              "support_sha256": sha256(support_path), "cases": [],
              "limitations": ["No Agent/REPL coverage in this harness", "Windows MSVC only",
                              "Guarded Windows mode does not enforce kernel filesystem/network isolation"],
              "provider_executed": args.exercise_provider}
    print(f"Lifecycle fixture: {root}", flush=True)

    def save():
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    def run(name: str, *command: str, success: bool = True, program=core):
        result = runner.run(name, [str(program), *map(str, command)])
        report["cases"].append({"name": name, **{key: value for key, value in result.items()
                                if key not in ("stdout", "stderr")}})
        save()
        require((result["exit"] == 0) == success,
                f"{name}: exit {result['exit']}; {result['stderr'][-2500:]} {result['stdout'][-1000:]}")
        require(sha256(core) == report["core_sha256_before"], f"{name}: Core binary changed")
        return result["stdout"] if success else result["stdout"] + "\n" + result["stderr"]

    def status(name: str):
        return json.loads(run(name, "memory", "status", "--json"))

    try:
        initial = status("absent-status")
        require(initial["state"] == "unavailable" and not initial["enabled"] and initial["selected"] is None,
                "unexpected initial state")
        require(not aizen.exists(), "read-only status initialized config/store/locks")
        require(json.loads(run("empty-list", "plugin", "list", "--json"))["packages"] == {}, "unexpected packages")
        require(not aizen.exists(), "empty listing initialized provider data")
        if provider_id == "mnemos":
            help_text = run("provider-help", "memory", "--help")
            require("no built-in Memory fallback" in help_text, "provider limits absent from help")
            learning_help = " ".join(run("learning-help", "memory", "learn", "--help").split())
            require("STYLE.md" in learning_help and "human management authority" in learning_help,
                    "learning confirmation help omits human-only STYLE promotion")
            model_help = run("model-help", "memory", "model-download", "--help")
            require("refuses downloads" in model_help, "Memory model help still promises Core download")
            require(not aizen.exists(), "help initialized provider/config state")
        if args.package is not None:
            archive = args.package.resolve(strict=True)
            require(provider_id == "mnemos", "supplied-package coverage currently requires Mnemos")
            require(archive.is_file() and archive.suffix == ".zip", "an existing ZIP package is required")
            report["supplied_package"] = str(archive)
            report["supplied_package_sha256"] = sha256(archive)
        else:
            package = root / "example-package"
            # Byte-copy Cargo artifacts because the packager rejects hardlinked inputs.
            source = root / "package-source"
            source.mkdir()
            for original, name in ((provider, "example-memory.exe"), (repo / "LICENSE", "LICENSE"), (repo / "NOTICE", "NOTICE")):
                shutil.copyfile(original, source / name)
            package_command = ["-I", str(repo / "scripts/package_memory_plugin.py"), "--id", provider_id,
                               "--name", "Fixture Memory", "--version", "0.1.0", "--target", "x86_64-pc-windows-msvc",
                               "--binary", str(source / "example-memory.exe"), "--entrypoint", "example-memory.exe",
                               "--license", str(source / "LICENSE"), "--notice", str(source / "NOTICE"),
                               "--capability", "recall", "--capability", "records", "--capability", "context",
                               "--output", str(package)]
            if provider_id == "mnemos":
                package_command.append("--host-llm")
                for capability in ("learning", "associations", "commands", "host_llm", "management"):
                    package_command.extend(("--capability", capability))
            run("package-provider", *package_command, program=Path(sys.executable).resolve())
            archives = list(package.glob("*.zip"))
            require(len(archives) == 1, "expected exactly one package")
            archive = archives[0]
        run("zip-install", "plugin", "install", str(archive))
        installed = status("installed-status")
        require(not installed["enabled"] and installed["selected"] is None and installed["installed"] == 1,
                "install selected/enabled provider")
        require(not (aizen / "memory-data").exists() and not (aizen / "cli-memory").exists(), "install created Memory data")
        if args.package is not None:
            require(sha256(aizen / "plugins" / provider_id / "aizen-mnemos.exe") == report["provider_sha256_before"],
                    "supplied package does not contain the explicitly supplied Mnemos binary")
        run("no-overwrite", "plugin", "install", str(archive), success=False)
        run("verify-info", "plugin", "info", provider_id)
        run("use-ineligible", "memory", "use", provider_id, "--yes", "--allow-guarded", success=False)
        run("eligible", "plugin", "enable", provider_id)
        require(status("eligible-status")["selected"] is None, "eligibility selected provider")
        run("explicit-use", "memory", "use", provider_id, "--yes", "--allow-guarded")
        active = status("selected-status")
        require(active["selected"] == provider_id and active["enabled"], "explicit selection failed")
        require(not (aizen / "memory-data").exists(), "selection alone executed or created data")
        if args.exercise_provider:
            health = json.loads(run("provider-health", "--sandbox", "guarded", "memory", "health"))
            require(health["status"] == "ready", "provider not ready")
            stored = json.loads(run("provider-store", "--sandbox", "guarded", "memory", "add", "fixture-fact", "--body", "Use isolated fixture data"))
            record = stored["record"]
            require(record["content"] == "Use isolated fixture data", "store did not round trip")
            if provider_id == "mnemos":
                def extension(name, *arguments):
                    value = json.loads(run(name, "--sandbox", "guarded", "memory", *arguments))
                    require(not (aizen / "plugin-state/memory-transaction.json").exists(),
                            f"{name}: clean command did not settle")
                    return value

                # No STYLE has been written in this fresh fixture; absence is JSON null, not text.
                require(extension("provider-style", "style") == {"text": None},
                        "fresh fixture unexpectedly contains a STYLE profile")
                require(isinstance(extension("provider-frozen", "frozen").get("text"), str),
                        "frozen did not return typed text")
                require(isinstance(extension("provider-profile", "profile", "--json").get("dims"), list),
                        "profile did not return typed dimensions")
                answer = extension("provider-ask", "ask", "Which package manager?", "--json")
                require(isinstance(answer.get("kind"), dict) and isinstance(answer.get("text"), str),
                        "ask did not return typed answer/abstention")
                historical = extension("provider-as-of", "as-of", "9999-12-31")
                require(any(item["id"] == record["id"] for item in historical["items"]),
                        "as-of lost the visible live record")
                require(extension("provider-review", "review")["items"] == [],
                        "fresh fixture unexpectedly contains review candidates")
                require(isinstance(extension("provider-frozen-rebuild", "frozen", "--rebuild").get("text"), str),
                        "frozen rebuild did not return typed text")
                require(extension("provider-model-list", "model-list")["weights_loaded"] is False,
                        "model inventory unexpectedly loaded weights")
                require(isinstance(extension("provider-doctor", "doctor").get("findings"), list),
                        "doctor did not return typed diagnostics")
                require(extension("provider-neighbors", "neighbors", record["id"], "-k", "1")["items"] == [],
                        "fresh fixture unexpectedly contains association edges")
                require(extension("provider-consolidate-preview", "consolidate")["dry_run"] is True,
                        "consolidation default was not dry run")
                require(extension("provider-consolidate-apply", "consolidate", "--apply")["dry_run"] is False,
                        "explicit consolidation apply was not honored")
                require(isinstance(extension("provider-compact", "compact").get("archived"), list),
                        "compact did not return a typed archive report")

                # Explicitly seed native-format queue records only under this disposable fixture.
                # This avoids depending on probabilistic learning to produce review candidates.
                review_dir = aizen / "memory-data" / provider_id / "review"
                require(review_dir.is_relative_to(root), "queue fixture escaped owned root")
                review_dir.mkdir(parents=True, exist_ok=True)
                for candidate in ("promote", "drop", "clear"):
                    path = review_dir / f"fixture-review-{candidate}.md"
                    require(not path.exists(), "queue fixture already occupied")
                    path.write_text(
                        f"---\nname: Lifecycle review {candidate}\ntype: user\ntier: user\n"
                        f"noCore: true\ncreated: 2026-01-01\n---\n\nCurated {candidate} preference.\n",
                        encoding="utf-8")
                queue = extension("provider-review-seeded", "review")["items"]
                require({item["id"] for item in queue} == {
                    "fixture-review-promote", "fixture-review-drop", "fixture-review-clear"},
                    "review listing lost exact fixture handles")
                promoted = extension("provider-review-promote", "review", "--promote", "fixture-review-promote")
                require(promoted["promoted"]["queued_id"] == "fixture-review-promote",
                        "promotion did not report the exact queue handle")
                require(extension("provider-review-drop", "review", "--drop", "fixture-review-drop")["discarded"]
                        == ["fixture-review-drop"], "drop did not report the exact queue handle")
                require(extension("provider-review-clear", "review", "--clear")["discarded"]
                        == ["fixture-review-clear"], "clear did not discard the remaining queue item")
                require(all((review_dir / ".discarded" / f"fixture-review-{item}.md").is_file()
                            for item in ("drop", "clear")), "review discard deleted recoverable fixture bytes")
                require(extension("provider-review-after-actions", "review")["items"] == [],
                        "review actions left queued candidates")
                data_before = support.inventory(aizen / "memory-data" / provider_id)
                for name, arguments in (
                    ("unsupported-model-download", ("model-download",)),
                ):
                    output = run(name, "--sandbox", "guarded", "memory", *arguments, success=False)
                    require("UnsupportedCapability:" in output,
                            f"{name}: missing typed unsupported error")
                    require(not (aizen / "plugin-state/memory-transaction.json").exists(),
                            f"{name}: semantic refusal poisoned physical settlement")
                data_after = support.inventory(aizen / "memory-data" / provider_id)
                require(all(data_after.get(path) == digest for path, digest in data_before.items()),
                        "unsupported command changed existing provider data")
                added = set(data_after) - set(data_before)
                require(len(added) == 1, "unsupported command added more than its no-effect receipt")
                receipt_path = added.pop()
                require(receipt_path.startswith("rpc-journal/") and receipt_path.endswith(".json")
                        and len(Path(receipt_path).stem) == 64
                        and all(ch in "0123456789abcdef" for ch in Path(receipt_path).stem),
                        "unsupported command wrote outside its operation receipt")
                receipt = json.loads((aizen / "memory-data" / provider_id / receipt_path).read_bytes())
                require(set(receipt) == {"schema_version", "digest", "scope", "phase", "result", "error"}
                        and receipt["schema_version"] == 2 and receipt["phase"] == "done"
                        and receipt["result"] is None
                        and receipt["error"] == {"code": "unsupported_capability",
                                                 "message": "Mnemos is offline; provision local weights explicitly"}
                        and all(isinstance(receipt[key], str) and len(receipt[key]) == 64
                                and all(ch in "0123456789abcdef" for ch in receipt[key])
                                for key in ("digest", "scope")),
                        "unsupported command did not persist the exact typed no-effect receipt")
                # Preparation and submission are distinct real Core invocations. Only the explicit
                # file supplies a completion; neither executable resolves a model route here.
                extension("management-reconcile-seed-target", "add", "fixture-reconcile-target", "--body",
                          "Use zircon zeppelin reconciliation policy")
                candidate = review_dir / "fixture-reconcile.md"
                require(not candidate.exists(), "reconcile fixture already occupied")
                candidate.write_text("---\nname: Reconcile duplicate\ntype: reference\ntier: place\n"
                                     "source: inferred\nconfidence: 0.7\ncreated: 2026-01-01\n---\n\n"
                                     "Use zircon zeppelin reconciliation policy\n", encoding="utf-8")
                plan = extension("management-reconcile-prepare", "reconcile")
                require(plan["phase"] == "prepared" and plan["model_calls"] == 0 and plan["pairs"] >= 1,
                        "reconcile did not prepare offline suspicious pairs")
                reply = root / "explicit-reconcile-reply.json"
                payload = {"binding_sha256": plan["binding_sha256"], "completion": json.dumps({"pairs": [
                    {"n": n, "verdict": "same", "confidence": 0.95} for n in range(1, plan["pairs"] + 1)]})}
                support.write_fixture(reply, json.dumps(payload) + "\n")
                entries_before = support.inventory(aizen / "memory-data" / provider_id / "entries")
                queue_before = support.inventory(review_dir)
                preview = extension("management-reconcile-preview", "reconcile", "--reply", reply)
                require(preview["dry_run"] and preview["model_calls"] == 0
                        and preview["reply_source"] == "explicit_offline", "reconcile preview routed a model")
                require(support.inventory(aizen / "memory-data" / provider_id / "entries") == entries_before
                        and support.inventory(review_dir) == queue_before, "judged preview changed facts or queue")
                applied = extension("management-reconcile-apply", "reconcile", "--reply", reply, "--apply")
                require(not applied["dry_run"] and all(not item["failed"] for item in applied["applied"]),
                        "reconcile apply did not finish")
                require(not candidate.exists() and (review_dir / ".discarded/fixture-reconcile.md").is_file(),
                        "reconcile did not preserve the removed duplicate")
                output = run("management-reconcile-stale", "--sandbox", "guarded", "memory", "reconcile",
                             "--reply", reply, "--apply", success=False)
                require("Conflict:" in output and not (aizen / "plugin-state/memory-transaction.json").exists(),
                        "stale reconcile binding was accepted or poisoned settlement")
            recalled = json.loads(run("provider-search", "--sandbox", "guarded", "memory", "search", "isolated fixture"))
            require(any(hit["record"]["id"] == record["id"] for hit in recalled["items"]), "search lost fixture record")
            fetched = json.loads(run("provider-get", "--sandbox", "guarded", "memory", "show", record["id"]))
            require(fetched["record"]["revision"] == 1, "unexpected initial revision")
            updated = json.loads(run("provider-update", "--sandbox", "guarded", "memory", "edit", record["id"], "--body", "Use revised isolated fixture data"))
            require(updated["record"]["revision"] == 2, "revision did not advance")
            forgotten = json.loads(run("provider-forget", "--sandbox", "guarded", "memory", "forget", record["id"]))
            after_forget = json.loads(run("provider-search-after-forget", "--sandbox", "guarded", "memory", "search", "isolated fixture"))
            require(not any(hit["record"]["id"] == record["id"] for hit in after_forget["items"]),
                    "forgotten record remains live")
            if provider_id == "mnemos":
                archived = forgotten["record"]
                require(archived["id"] == record["id"] and archived["archived"] and archived["revision"] == 3,
                        "forget lost actual archive handle or terminal revision")
                native_prefix = ["--data-dir", str(aizen / "memory-data" / provider_id),
                                 "--workspace-root", str(workspace), "--cwd", str(workspace),
                                 "--device", "fixture-native-device", "memory"]
                run("native-restore-origin", *native_prefix, "restore", archived["id"], program=provider)
                restored = json.loads(run("restored-origin-get", "--sandbox", "guarded", "memory", "show", record["id"]))
                require(restored["record"]["revision"] == 4 and not restored["record"]["archived"],
                        "native restore reused a stale live revision")
                run("native-edit", *native_prefix, "edit", record["id"], "--body", "Native revised fixture data", program=provider)
                native_edit = json.loads(run("native-edit-get", "--sandbox", "guarded", "memory", "show", record["id"]))
                require(native_edit["record"]["revision"] == 5 and native_edit["record"]["content"] == "Native revised fixture data",
                        "native edit bypassed shared CAS counter")
                core_edit = json.loads(run("core-edit-after-native", "--sandbox", "guarded", "memory", "edit", record["id"], "--body", "Core revised fixture data"))
                require(core_edit["record"]["revision"] == 6, "Core/native writes double-counted or reset revisions")
                second = json.loads(run("provider-forget-again", "--sandbox", "guarded", "memory", "forget", record["id"]))["record"]
                require(second["id"] == record["id"] + "-2" and second["revision"] == 7 and second["archived"],
                        "archive reused a restored handle or returned the live ID")
                recreated = json.loads(run("provider-recreate", "--sandbox", "guarded", "memory", "add", "fixture-fact", "--body", "Distinct recreated fixture data"))
                require(recreated["record"]["id"] == record["id"] and recreated["record"]["revision"] == 8,
                        "recreation reset the retired ID's CAS counter")
                archive_list = json.loads(run("archive-after-recreate", "--sandbox", "guarded", "memory", "archive"))
                require(archive_list["items"] == [second], "archive snapshot changed after live-ID recreation")
                run("native-restore-handle", *native_prefix, "restore", second["id"], program=provider)
                distinct = json.loads(run("restored-handle-get", "--sandbox", "guarded", "memory", "show", second["id"]))
                require(distinct["record"]["id"] == second["id"] and distinct["record"]["revision"] == 1
                        and distinct["record"]["content"] == second["content"],
                        "default restore guessed origin from a suffix instead of keeping the actual handle")
                origin = json.loads(run("recreated-origin-get", "--sandbox", "guarded", "memory", "show", record["id"]))
                require(origin == recreated, "restoring a distinct handle changed the occupied origin")

                old = extension("management-store-old", "add", "management-obsolete", "--body",
                                "Obsolete fixture management convention")["record"]
                new = extension("management-store-new", "add", "management-current", "--body",
                                "Current fixture management convention")["record"]
                require(extension("management-supersede", "supersede", old["id"], new["id"])
                        == {"old": old["id"], "new": new["id"]}, "supersede lost exact IDs")
                visible = extension("management-list-live", "list", "current")["items"]
                require(not any(item["record"]["id"] == old["id"] for item in visible),
                        "superseded fact remains in live listing")
                retired = extension("management-list-retired", "list", "--scope", "all", "--superseded")["items"]
                require(any(item["record"]["id"] == old["id"] for item in retired),
                        "human retired inspection lost superseded fact")
                require(extension("management-revive", "revive", old["id"])["id"] == old["id"],
                        "revive lost exact target")
                moved = extension("management-scope-move", "edit", old["id"], "--scope", "user")["record"]
                require(moved["scope"] == "user", "human edit did not move scope")
                require(any(item["record"]["id"] == old["id"] for item in
                            extension("management-list-user", "list", "user")["items"]),
                        "user scope listing lost moved fact")
                require(any(item["record"]["id"] == old["id"] for item in
                            extension("management-search-user", "search", "Obsolete fixture management convention",
                                      "--scope", "user")["items"]), "scoped search lost moved fact")
                archived_management = extension("management-forget", "forget", old["id"])["record"]
                restored_management = extension("management-restore", "restore", archived_management["id"],
                                                 "--as", "fixture-restored-management")
                require(restored_management["id"] == "fixture-restored-management", "restore ignored explicit target")
                require(extension("management-restored-get", "show", restored_management["id"])["record"]["content"]
                        == moved["content"], "restore changed archived content")
                purge_target = extension("management-forget-to-purge", "forget", restored_management["id"])["record"]["id"]
                before_refusal = support.inventory(aizen / "memory-data" / provider_id)
                output = run("management-purge-needs-confirmation", "--sandbox", "guarded", "memory", "purge", purge_target,
                             success=False)
                require("--yes required" in output, "purge did not require explicit confirmation")
                require(support.inventory(aizen / "memory-data" / provider_id) == before_refusal,
                        "unconfirmed purge changed provider data")
                require(extension("management-purge-confirmed", "purge", purge_target, "--yes")["purged"] is True,
                        "confirmed purge did not report deletion")
                require(not any(item["id"] == purge_target for item in
                                extension("management-archive-after-purge", "archive")["items"]),
                        "purged archive handle remains visible")
                extension("management-learn-style", "learn", "please reply in Vietnamese", "--yes")
                require("vietnamese" in extension("management-style-after-learn", "style")["text"].lower(),
                        "human-confirmed learning did not promote STYLE")
            require(not (aizen / "plugin-state/memory-transaction.json").exists(), "settled transaction left recovery barrier")
            require(not (aizen / "cli-memory").exists(), "provider used legacy Core path")
        run("memory-disable", "memory", "disable")
        if args.exercise_provider:
            run("disabled-operation", "--sandbox", "guarded", "memory", "health", success=False)
        disabled = status("disabled-status")
        require(not disabled["enabled"] and disabled["selected"] == active["selected"] and disabled["generation"] > active["generation"], "disable lost selection or generation")
        run("memory-enable", "memory", "enable")
        run("plugin-disable", "plugin", "disable", provider_id)
        require(not status("ineligible-status")["enabled"], "disabled package remains active")
        run("no-ineligible-reenable", "memory", "enable", success=False)
        data = aizen / "memory-data" / provider_id
        data.mkdir(parents=True, exist_ok=True)
        sentinel = data / "fixture-sentinel.json"
        sentinel.write_text('{"preserve":true}\n', encoding="utf-8")
        data_before = support.inventory(data)
        run("uninstall", "plugin", "uninstall", provider_id, "--yes")
        require(support.inventory(data) == data_before, "uninstall changed provider data")
        final = status("uninstalled-status")
        require(final["installed"] == 0 and final["selected"] is None and not final["enabled"], "unexpected uninstalled state")
        require(not (aizen / "plugins" / provider_id).exists(), "package remains after uninstall")
        require(not (aizen / "cli-memory").exists(), "lifecycle used legacy path")
        if provider_id == "mnemos":
            # Reinstall into this same disposable fixture, then simulate damaged package metadata.
            # No provider operation runs here, and unknown remaining files must never be swept.
            recovery_start = len(report["cases"])
            run("recovery-install", "plugin", "install", str(archive))
            run("recovery-eligible", "plugin", "enable", provider_id)
            run("recovery-select", "memory", "use", provider_id, "--yes", "--allow-guarded")
            pins = json.loads(run("recovery-pins-before", "plugin", "list", "--json"))["packages"][provider_id]["pins"]
            directory = aizen / "plugins" / provider_id
            manifest = directory / "plugin.json"
            require(json.loads(manifest.read_bytes())["id"] == provider_id,
                    "unexpected fixture package identity")
            manifest.write_bytes(b"{")
            unknown = directory / "unknown-fixture.txt"
            require(not unknown.exists(), "unknown fixture path already occupied")
            unknown.write_bytes(b"preserve unknown payload\n")
            package_before = support.inventory(directory)
            output = run("corrupt-uninstall", "plugin", "uninstall", provider_id, "--yes", success=False)
            require("plugin disabled, data preserved, host pins retained" in output,
                    "corrupt uninstall did not explain retained recovery state")
            damaged = status("corrupt-uninstall-status")
            require(not damaged["enabled"] and damaged["selected"] == provider_id
                    and damaged["installed"] == 1 and not damaged["recovery_required"],
                    "corrupt uninstall lost its disabled selection or invented physical recovery")
            retained = json.loads(run("corrupt-pins-after", "plugin", "list", "--json"))["packages"][provider_id]
            require(not retained["enabled"] and retained["pins"] == pins,
                    "corrupt uninstall lost pins or left executable eligible")
            run("corrupt-enable-refused", "plugin", "enable", provider_id, success=False)
            require(support.inventory(directory) == package_before,
                    "corrupt uninstall changed unknown/unverified package files")
            require(support.inventory(data) == data_before, "corrupt uninstall changed provider data")
            # Explicit fixture repair moves the inspected tree aside without deleting any payload.
            quarantine = root / "quarantined-package"
            require(not quarantine.exists(), "quarantine fixture path already occupied")
            directory.rename(quarantine)
            require(support.inventory(quarantine) == package_before, "quarantine changed payload")
            run("missing-uninstall", "plugin", "uninstall", provider_id, "--yes")
            missing = status("missing-uninstall-status")
            require(missing["installed"] == 0 and missing["selected"] is None and not missing["enabled"],
                    "missing package could not retire its registration")
            require(support.inventory(data) == data_before, "missing uninstall changed provider data")
            require(support.inventory(quarantine) == package_before, "missing uninstall touched quarantine")
            require(all(case["job_processes"] == 1 for case in report["cases"][recovery_start:]),
                    "package recovery administration started a child/provider")
            report["damaged_package_disabled_pins_retained"] = True
            report["missing_package_registration_removed"] = True
        report["passed"] = True
        report["provider_data_preserved"] = True
    except Exception as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        traceback.print_exc()
    finally:
        report["core_sha256_after"] = sha256(core)
        report["provider_sha256_after"] = sha256(provider)
        report["core_unchanged"] = report["core_sha256_after"] == report["core_sha256_before"]
        report["process_cleanup_failures"] = runner.cleanup_failures
        if not report["core_unchanged"] or report["provider_sha256_after"] != report["provider_sha256_before"] or runner.cleanup_failures:
            report["passed"] = False
        save()
    print(f"{'PASS' if report['passed'] else 'FAIL'} {len(report['cases'])} lifecycle cases: {root / 'report.json'}", flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
