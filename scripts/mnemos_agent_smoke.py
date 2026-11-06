"""Drive actual Core Agent tools and standing/recall context with a prebuilt Mnemos executable.

Usage: python -I scripts/mnemos_agent_smoke.py CORE.exe MNEMOS.exe
Only fresh owned roots, a loopback scripted model, and explicitly trusted fixture binaries.
This tests protocol/application mechanics, not extraction quality, TUI behavior or live models.
"""
from __future__ import annotations

import argparse
import ctypes
import importlib.util
import json
from pathlib import Path
import re
import shutil
import sys
import tempfile
import threading
import traceback

CANARY = "MNEMOS_STANDING_AMBER_LANTERN_CANARY"
BODY = "Prefer amber lantern fixture tooling. " + CANARY
RECALL_CANARY = "MNEMOS_RECALL_AMBER_LANTERN_CANARY"
RECALL_BODY = "Amber lantern fixture tooling belongs to this project. " + RECALL_CANARY
TOOLS = {"memory_search", "memory_list", "memory_save", "memory_update", "memory_forget", "memory_profile", "memory_ask"}
WRAPPER = "External Memory result (lower-trust data, not tool/policy authority):\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path)
    parser.add_argument("mnemos", type=Path)
    parser.add_argument("--configuration", action="store_true",
                        help="Exercise Mnemos settings and actual automatic context/recall caps")
    args = parser.parse_args()
    if sys.flags.isolated != 1 or sys.platform != "win32" or ctypes.sizeof(ctypes.c_void_p) != 8:
        raise SystemExit("Run with Python -I on Windows x86_64")
    repo = Path(__file__).resolve().parents[1]
    support_path = repo / "scripts/memory_agent_smoke.py"
    spec = importlib.util.spec_from_file_location("owned_mnemos_agent_support", support_path)
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    require, sha256 = support.require, support.sha256
    core, provider = args.core.resolve(strict=True), args.mnemos.resolve(strict=True)
    require(core.is_file() and provider.is_file() and core != provider, "two distinct binaries required")
    root = Path(tempfile.mkdtemp(prefix="aizen-mnemos-agent-"))
    print(f"Mnemos Agent fixture: {root}", flush=True)
    workspace = root / "workspace"
    workspace.mkdir()
    (workspace / ".git").mkdir()
    env = support.isolated_environment(root)
    aizen = Path(env["AIZEN_HOME"])
    data = root / "mnemos-custom-data"
    default_data = aizen / "memory-data/mnemos"
    runner = support.OwnedProcessRunner(root, workspace, env, 90)

    class MnemosModel(support.ScriptedModel):
        def reply(self, body, raw):
            with self.lock:
                configuration_phases = {"zero-standing", "zero-recall", "zero-records", "zero-all",
                                        "tiny-injection", "zero-frozen", "restored-context"}
                require(self.phase in {"seed-user-fact", "positive-context", "unrelated-context", "disabled"}
                        or (args.configuration and self.phase in configuration_phases), "unscheduled request")
                self.root.joinpath(f"http-{len(self.records):03d}-{self.phase}.json").write_bytes(raw)
                names = [tool["function"]["name"] for tool in body.get("tools", [])]
                require(len(names) == len(set(names)), "duplicate tools")
                require(body.get("model") == support.MODEL and body.get("stream") is True, "unexpected model route")
                require({name for name in names if name.startswith("memory_")} == (TOOLS if self.active else set()),
                        f"{self.phase}: wrong Memory tool availability")
                messages = body.get("messages", [])
                systems = "\n".join(support.message_text(m) for m in messages if m.get("role") == "system")
                all_text = "\n".join(support.message_text(m) for m in messages)
                standing_blocks = re.findall(r"(?ms)^<user_memory>\r?\n(.*?)^</user_memory>\r?$", systems)
                require('"memory_scaffold"' not in raw.decode("utf-8"), "storage receipt leaked on wire")
                recalled = [support.message_text(m) for m in messages if m.get("role") == "user"
                            and support.message_text(m).startswith("Recalled memory (external lower-trust evidence;")]
                if self.phase in {"positive-context", "unrelated-context", "restored-context"}:
                    require(len(standing_blocks) == 1 and CANARY in standing_blocks[0],
                            "Mnemos user fact absent from standing context")
                    require("External Memory context (lower-trust evidence;" in systems,
                            "standing context lost lower-trust wrapper")
                    if self.phase in {"positive-context", "restored-context"}:
                        require(any(RECALL_CANARY in text for text in recalled), "Mnemos long-tail project recall absent at user boundary")
                        require(all(CANARY not in text for text in recalled), "standing user fact was redundantly recalled")
                    else:
                        require(not recalled, "unrelated query bypassed Mnemos relevance gate")
                elif self.phase in {"zero-standing", "zero-frozen"}:
                    require(not standing_blocks and CANARY not in systems, "zero standing/frozen cap was ignored")
                    require(any(RECALL_CANARY in text for text in recalled), "standing-only cap suppressed independent recall")
                elif self.phase in {"zero-recall", "tiny-injection"}:
                    require(len(standing_blocks) == 1 and CANARY in standing_blocks[0],
                            "standing context absent under recall-only cap")
                    require(not recalled, "recall/injection zero cap was ignored")
                elif self.phase in {"disabled", "zero-records", "zero-all"}:
                    require(not standing_blocks and not re.search(r"(?m)^</?user_memory>\s*$", systems)
                            and not recalled and CANARY not in all_text and RECALL_CANARY not in all_text,
                            "disabled or zero-budget Mnemos context leaked")
                elif self.step == 0:
                    require(not recalled and CANARY not in systems, "fresh provider already has fixture data")
                self.records.append({"phase": self.phase, "step": self.step, "bytes": len(raw),
                                     "tools": sorted(names), "recalled_prefixes": len(recalled)})
                if self.pending:
                    call_id, name = self.pending
                    value = support.matching_tool_result(body, call_id, name)
                    require(value.startswith(WRAPPER), f"{name}: failed tool or missing lower-trust wrapper")
                    result = json.loads(value[len(WRAPPER):])
                    if name == "memory_save":
                        record = result["record"]
                        require(record["content"] == BODY and record["scope"] == "user" and record["revision"] == 1,
                                "actual memory_save did not persist scoped user fact")
                        self.record_id = record["id"]
                    else:
                        require(any(hit["record"]["content"] == RECALL_BODY and hit["record"]["scope"] == "project"
                                    for hit in result["items"]), "actual Mnemos memory_search lost long-tail project fact")
                    self.pending = None
                step = self.step
                self.step += 1
                if step == 0 and self.phase in {"seed-user-fact", "positive-context"}:
                    name, arguments = ("memory_save", {"name": "fixture-standing-user", "body": BODY, "scope": "global", "type": "user"}) if self.phase == "seed-user-fact" else ("memory_search", {"query": "amber lantern fixture tooling", "limit": 5})
                    call_id = "mnemos-fixture-" + self.phase
                    self.pending = (call_id, name)
                    self.phase_tools.append(name)
                    return (call_id, name), arguments
                require(step == (1 if self.phase in {"seed-user-fact", "positive-context"} else 0), "unexpected continuation/retry")
                return None, "Synthetic Mnemos application mechanics observed; no semantic-quality claim."

    recorder = MnemosModel(root)
    server = server_thread = None
    report = {"root": str(root), "passed": False, "cases": [],
              "core_sha256_before": sha256(core), "provider_sha256_before": sha256(provider),
              "script_sha256": sha256(Path(__file__).resolve()), "support_sha256": sha256(support_path),
              "limitations": ["Windows MSVC Agent CLI only; no retained TUI/hostbot/cron coverage",
                              "Scripted mechanics, not extraction, learning quality or semantic model quality",
                              "No completed-turn learning permission or host model sharing granted",
                              "Guarded Windows mode does not enforce kernel filesystem/network isolation"]}

    def save():
        report["model_requests"], report["model_errors"] = recorder.records, recorder.errors
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    def run(name, *arguments, program=core):
        result = runner.run(name, [str(program), *map(str, arguments)])
        report["cases"].append({"name": name, **{key: value for key, value in result.items()
                                if key not in {"stdout", "stderr"}}})
        save()
        require(result["exit"] == 0, f"{name}: {result['stderr'][-2500:]} {result['stdout'][-1000:]}")
        require(sha256(core) == report["core_sha256_before"], "Core executable changed")
        return result

    def agent(phase, active):
        recorder.arm(phase, active=active)
        before = len(recorder.records)
        # Recall gates the complete user query, not the later memory_search arguments. Keep the
        # positive case on-topic and explicitly verify an unrelated turn produces no recall.
        query = "orbital basalt quasar zeppelin" if phase == "unrelated-context" else "amber lantern fixture tooling"
        result = run(phase, "--sandbox", "guarded", "agent", query,
                     "--base-url", base, "--api-key", support.FAKE_KEY, "--model", support.MODEL,
                     "--design", "off", "--output-format", "stream-json", "--max-iters", "4", "--yes")
        launches_provider = active and phase not in {"zero-records", "zero-all"}
        require(result["job_processes"] > 1 if launches_provider else result["job_processes"] == 1,
                f"{phase}: unexpected child activity")
        require(not recorder.errors, f"loopback assertions failed: {recorder.errors}")
        events = [json.loads(line) for line in result["stdout"].splitlines() if line.strip()]
        initial = [event for event in events if event.get("type") == "system" and event.get("subtype") == "init"]
        final = [event for event in events if event.get("type") == "result"]
        require(len(initial) == len(final) == 1 and not final[0].get("is_error") and not final[0].get("permission_denials"),
                f"{phase}: missing/failed Agent result")
        require({name for name in initial[0]["tools"] if name.startswith("memory_")} == (TOOLS if active else set()),
                "init tool availability mismatch")
        expected = {"seed-user-fact": ["memory_save"], "positive-context": ["memory_search"]}.get(phase, [])
        require(recorder.phase_tools == expected and recorder.pending is None, "tool sequence incomplete")
        require(len(recorder.records) - before == len(expected) + 1, "unexpected model request count")

    def configure(name, *flags):
        result = run(name, "plugin", "configure", "mnemos", *flags)
        require(result["job_processes"] == 1, "offline configuration launched a provider")
        text = result["stdout"]
        return json.loads(text[text.index("{"):])

    def storage(name, *flags, automatic):
        result = run(name, "plugin", "storage", "mnemos", *flags)
        require(result["job_processes"] == 1, "offline storage launched a provider")
        value = json.loads(result["stdout"])
        expected = default_data if automatic else data
        # Canonical paths from Rust can use the Windows verbatim prefix.
        actual = Path(value["data_root"].removeprefix("\\\\?\\")).resolve()
        require(value["provider"] == "mnemos" and value["automatic"] is automatic
                and value["data_moved"] is False and actual == expected.resolve(),
                f"{name}: unexpected storage response: {value}")
        package_state = json.loads((aizen / "cli-config.json").read_bytes())["memory_plugins"]["packages"]["mnemos"]
        saved = package_state.get("data_root")
        require(saved is None if automatic else saved is not None
                and Path(saved.removeprefix("\\\\?\\")).resolve() == data.resolve(),
                "saved Mnemos storage does not match inspected root")

    try:
        initial = json.loads(run("initial-status", "memory", "status", "--json")["stdout"])
        require(initial["state"] == "unavailable" and not aizen.exists(), "status created state")
        server, server_thread = support.make_server(recorder)
        base = f"http://127.0.0.1:{server.server_port}/v1"
        support.write_fixture(aizen / "cli-config.json", json.dumps({
            "base_url": base, "api_key": support.FAKE_KEY, "model": support.MODEL,
            "model_context_window": 131072, "architect_mode": False, "self_review": False,
            "eager_tools": False, "lean_tools": False, "auto_effort": False, "compact_threshold_pct": 0,
            "auto_skill_learn": False, "memory_auto_learn": False, "persona_evolve": False,
            "update_check": False, "auto_copy": False,
        }) + "\n")
        source = root / "package-source"
        source.mkdir()
        for original, name in ((provider, "mnemos.exe"), (repo / "LICENSE", "LICENSE"), (repo / "NOTICE", "NOTICE")):
            shutil.copyfile(original, source / name)
        package = root / "packaged"
        command = ["-I", str(repo / "scripts/package_memory_plugin.py"), "--id", "mnemos", "--name", "Mnemos fixture",
                   "--version", "0.1.0", "--target", "x86_64-pc-windows-msvc", "--binary", str(source / "mnemos.exe"),
                   "--entrypoint", "mnemos.exe", "--license", str(source / "LICENSE"), "--notice", str(source / "NOTICE"),
                   "--output", str(package), "--host-llm"]
        # Match the executable's declared surface without granting model sharing. The separate
        # saved --allow-learning/--allow-turn-model consents remain absent in this fixture.
        for capability in ("recall", "records", "context", "learning", "associations", "commands", "host_llm", "management"):
            command.extend(("--capability", capability))
        if args.configuration:
            packager = repo.parent / "aizen-mnemos/scripts/package.py"
            report["packager_sha256"] = sha256(packager)
            command = ["-I", "-B", str(packager), "--binary", str(source / "mnemos.exe"),
                       "--target", "x86_64-pc-windows-msvc", "--output", str(package)]
        run("package-mnemos", *command, program=Path(sys.executable).resolve())
        archives = list(package.glob("*.zip"))
        require(len(archives) == 1, "expected exactly one package")
        run("install-mnemos", "plugin", "install", str(archives[0]))
        storage("inspect-mnemos-default-storage", automatic=True)
        storage("save-mnemos-custom-storage", "--path", data, automatic=False)
        storage("inspect-mnemos-custom-storage", automatic=False)
        require(not data.exists() and not (aizen / "memory-data").exists(), "offline storage created a data root")
        run("eligible-mnemos", "plugin", "enable", "mnemos")
        run("select-mnemos", "memory", "use", "mnemos", "--yes", "--allow-guarded")
        require(not data.exists() and not (aizen / "memory-data").exists(), "selection executed provider")
        require(json.loads(run("custom-root-health", "--sandbox", "guarded", "memory", "health")["stdout"])["status"] == "ready",
                "custom-root Mnemos health failed")
        require(data.is_dir() and not default_data.exists(), "runtime health initialized the wrong root")
        agent("seed-user-fact", True)
        # Frozen user facts are deliberately excluded from long-tail retrieval. Seed a separate
        # project reference through the actual CLI rather than expecting duplicate user recall.
        project = json.loads(run("seed-project-fact", "--sandbox", "guarded", "memory", "add",
                                 "fixture-project-reference", "--body", RECALL_BODY)["stdout"])["record"]
        require(project["content"] == RECALL_BODY and project["scope"] == "project", "project seed failed")
        require(json.loads(run("read-custom-project-fact", "--sandbox", "guarded", "memory", "show", project["id"])["stdout"])["record"]["content"] == RECALL_BODY,
                "custom-root project read failed")
        require(not default_data.exists(), "custom runtime wrote to default storage")
        sentinel = data / "preserve-custom.txt"
        sentinel.write_bytes(b"mnemos custom storage canary")
        custom_before = support.inventory(data)
        storage("reset-mnemos-storage", "--default", automatic=True)
        require(support.inventory(data) == custom_before and not default_data.exists(),
                "storage reset moved/copied/deleted custom data")
        require(json.loads(run("default-mnemos-is-empty", "--sandbox", "guarded", "memory", "list")["stdout"])["items"] == [],
                "default Mnemos adopted custom records")
        require(support.inventory(data) == custom_before, "default Mnemos modified custom data")
        default_sentinel = default_data / "preserve-default.txt"
        default_sentinel.write_bytes(b"mnemos default storage canary")
        default_before = support.inventory(default_data)
        storage("restore-mnemos-custom-storage", "--path", data, automatic=False)
        require(support.inventory(data) == custom_before and support.inventory(default_data) == default_before,
                "storage restore moved/copied/deleted provider data")
        # Preserve the existing model phases: this positive turn now proves restored custom
        # standing context, boundary recall and an actual memory_search still see both seeds.
        agent("positive-context", True)
        agent("unrelated-context", True)
        if args.configuration:
            defaults = configure("inspect-mnemos-defaults")
            require(defaults["effective_settings"] == {"enable_fuzzy": True,
                    "recency_half_life_days": 30.0, "frozen_core_max_tokens": 800},
                    "standalone Mnemos package defaults mismatch")
            custom = configure("configure-mnemos-settings", "--set", "enable_fuzzy=false",
                               "--set", "recency_half_life_days=45.5",
                               "--set", "frozen_core_max_tokens=900")
            require(custom["effective_settings"] == {"enable_fuzzy": False,
                    "recency_half_life_days": 45.5, "frozen_core_max_tokens": 900},
                    "Mnemos custom settings were not saved")
            configure("zero-standing-cap", "--context-tokens", "0")
            agent("zero-standing", True)
            configure("zero-recall-cap", "--context-tokens", "1024", "--recall-tokens", "0")
            agent("zero-recall", True)
            configure("zero-record-cap", "--recall-tokens", "1024", "--max-records", "0")
            agent("zero-records", True)
            configure("zero-automatic-caps", "--max-records", "100", "--context-tokens", "0",
                      "--recall-inject-tokens", "0")
            agent("zero-all", True)
            configure("tiny-recall-injection", "--context-tokens", "1024", "--recall-inject-tokens", "1")
            agent("tiny-injection", True)
            configure("zero-mnemos-frozen-cap", "--recall-inject-tokens", "300",
                      "--set", "frozen_core_max_tokens=0")
            agent("zero-frozen", True)
            reset = configure("reset-mnemos-settings", "--reset", "enable_fuzzy",
                              "--reset", "recency_half_life_days", "--reset", "frozen_core_max_tokens")
            require(reset["effective_settings"] == defaults["effective_settings"],
                    "reset did not restore Mnemos defaults")
            agent("restored-context", True)
            report["configuration"] = {"standalone_schema_accepted": True,
                "custom_settings_accepted_by_rpc": True, "zero_standing_tested": True,
                "zero_recall_tested": True, "zero_records_no_launch_tested": True,
                "zero_automatic_no_launch_tested": True, "tiny_injection_tested": True,
                "mnemos_zero_frozen_effect_tested": True, "reset_restores_context_tested": True}
        data_before = support.inventory(data)
        run("disable-mnemos", "memory", "disable")
        agent("disabled", False)
        require(support.inventory(data) == data_before and support.inventory(default_data) == default_before,
                "disable/disabled Agent changed provider data")
        run("uninstall-mnemos", "plugin", "uninstall", "mnemos", "--yes")
        require(support.inventory(data) == data_before and support.inventory(default_data) == default_before,
                "uninstall moved/copied/deleted custom or default Mnemos data")
        require(sentinel.read_bytes() == b"mnemos custom storage canary"
                and default_sentinel.read_bytes() == b"mnemos default storage canary",
                "disable/uninstall erased storage canaries")
        require(not (aizen / "plugin-state/memory-transaction.json").exists(), "transaction did not settle")
        require(not (aizen / "cli-memory").exists(), "Core legacy Memory initialized")
        report.update({"passed": True, "standing_context_positive_tested": True,
                       "recall_positive_tested": True, "unrelated_recall_rejected": True,
                       "actual_memory_save_verified": True,
                       "actual_memory_search_verified": True, "disabled_provider_data_preserved": True,
                       "storage": {"default_initially_automatic": True, "offline_no_launch_or_root_creation": True,
                                   "custom_health_save_read": True, "reset_default_empty_restore_recall": True,
                                   "disable_uninstall_preserve_both_roots": True}})
    except Exception as error:
        report["failure"] = f"{type(error).__name__}: {error}"
        traceback.print_exc()
    finally:
        if server is not None:
            stop = threading.Thread(target=server.shutdown, daemon=True)
            stop.start()
            stop.join(timeout=5)
            drained = server.drain_owned_connections()
            server.server_close()
            server_thread.join(timeout=5)
            if stop.is_alive() or server_thread.is_alive() or not drained:
                report["passed"] = False
                report["server_cleanup_error"] = "loopback server failed bounded shutdown"
        report["core_sha256_after"] = sha256(core)
        report["provider_sha256_after"] = sha256(provider)
        report["core_unchanged"] = report["core_sha256_after"] == report["core_sha256_before"]
        report["process_cleanup_failures"] = runner.cleanup_failures
        if not report["core_unchanged"] or report["provider_sha256_after"] != report["provider_sha256_before"] or recorder.errors or runner.cleanup_failures:
            report["passed"] = False
        save()
    print(f"{'PASS' if report['passed'] else 'FAIL'}: {root / 'report.json'}", flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
