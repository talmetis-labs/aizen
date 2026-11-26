"""Probe Agent CLI Skill/Persona learning using only fixture homes and a loopback model.

Usage: python -I scripts/local_learning_smoke.py CORE.exe
No builds, downloads, installation, real providers, credentials or user memory. Windows only.
The mock verifies lifecycle/plumbing, not the semantic quality of an actual model's learning.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import traceback

# Load only the repository-owned support file, never a module from the fixture workspace.
_spec = importlib.util.spec_from_file_location("learning_smoke_support", Path(__file__).with_name("memory_agent_smoke.py"))
support = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(support)
require = support.require
SKILL = "fixture-learned-checklist"
STEPS = "1. List acceptance checks.\n2. Run each check.\n3. Report actual results."
EPISODES = (
    "I kept a gentle voice while acknowledging a disagreement about how we collaborate.",
    "I showed curiosity and patience when our conversation shifted into unfamiliar territory.",
)
INSIGHT = "I stay candid without losing warmth, and give uncertainty room in our conversations."


class LearningModel(support.ScriptedModel):
    def reply(self, body: dict, raw: bytes):
        with self.lock:
            require(self.phase != "unarmed", "unscheduled model request")
            self.root.joinpath(f"http-{len(self.records):03d}-{self.phase}.json").write_bytes(raw)
            require(body.get("model") == support.MODEL, "unexpected learning model route")
            systems = "\n".join(support.message_text(message) for message in body["messages"]
                                if message.get("role") == "system")
            text = "\n".join(support.message_text(message) for message in body["messages"])
            names = [tool["function"]["name"] for tool in body.get("tools", [])]
            require(not any(name.startswith("memory_") for name in names), "disabled Memory advertised tools")
            if body.get("stream") is True:
                kind = "agent"
                require("LOCAL_LEARNING_PERSONA" in systems, "persona not injected into Agent")
                if self.phase in ("second-episode", "disabled"):
                    require(SKILL in systems, "learned Skill absent on next CLI invocation")
                    require(EPISODES[0] in systems, "learned episode absent on next conversation")
                if self.phase == "disabled":
                    require(INSIGHT in systems, "durable reflected insight absent from next invocation")
                value = "Fixture turn completed. No production files were changed."
            elif "end-of-turn identity secretary" in systems:
                kind = "secretary"
                require(self.phase in ("first-episode", "second-episode"), "disabled flags still invoked secretary")
                index = int(self.phase == "second-episode")
                require("Remember" in text, "secretary did not receive the actual user turn")
                value = json.dumps({"episode": {"text": EPISODES[index], "importance": 7},
                                    "skill": {"name": SKILL, "when": "checking acceptance criteria",
                                              "steps": STEPS, "action": "new"} if index == 0 else None})
            elif "Step back and REFLECT" in systems:
                kind = "reflection"
                require(self.phase == "second-episode", "reflection fired without two formative episodes")
                require(all(episode in text for episode in EPISODES), "reflection missed formative substrate")
                value = json.dumps({"insights": [{"text": INSIGHT, "importance": 8}]})
            else:
                raise AssertionError("unexpected auxiliary model call")
            require(not any(record["phase"] == self.phase and record["kind"] == kind for record in self.records),
                    f"repeated {kind} call in {self.phase}")
            self.records.append({"phase": self.phase, "kind": kind, "bytes": len(raw)})
            return None, value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("core", type=Path)
    parser.add_argument("--temp-parent", type=Path)
    args = parser.parse_args()
    require(os.name == "nt", "this fixture uses the Windows owned-process Job runner")
    binary = args.core.resolve(strict=True)
    require(binary.is_file(), "Core binary is not a file")
    if args.temp_parent:
        require(args.temp_parent.is_dir(), "temp parent must already exist")
    root = Path(tempfile.mkdtemp(prefix="aizen-local-learning-", dir=args.temp_parent))
    print(f"Fixture: {root}", flush=True)
    workspace = root / "workspace"
    workspace.mkdir()
    env = support.isolated_environment(root)
    home = Path(env["AIZEN_HOME"])
    home.mkdir(parents=True, exist_ok=True)
    env["AIZEN_CHORE_CALL_SECS"] = "5"
    recorder = LearningModel(root)
    server, thread = support.make_server(recorder)
    runner = support.OwnedProcessRunner(root, workspace, env, 45)
    config = {
        "base_url": f"http://127.0.0.1:{server.server_port}/v1", "api_key": support.FAKE_KEY,
        "model": support.MODEL, "model_context_window": 131072,
        "architect_mode": False, "self_review": False, "eager_tools": False, "lean_tools": False,
        "auto_effort": False, "compact_threshold_pct": 0, "auto_skill_learn": True,
        "memory_auto_learn": False, "persona_evolve": True, "persona_for_coding": True,
        "persona": "fixture-guide", "update_check": False, "auto_copy": False,
    }
    report = {"core_sha256": support.sha256(binary), "passed": False,
              "scope": "Agent CLI local learning with Memory disabled, scripted loopback only"}
    try:
        support.write_fixture(home / "cli-config.json", json.dumps(config))
        support.write_fixture(home / "personas/fixture-guide.md",
                              "---\nname: fixture-guide\nrole: a patient guide\nvoice: concise\n---\nLOCAL_LEARNING_PERSONA\n")
        for phase in ("first-episode", "second-episode", "disabled"):
            if phase == "disabled":
                config.update(auto_skill_learn=False, persona_evolve=False)
                support.write_fixture(home / "cli-config.json", json.dumps(config))
                before = {name: support.inventory(home / name) for name in ("skills", "personas")}
            recorder.arm(phase)
            result = runner.run(phase, [str(binary), "--sandbox", "guarded", "agent",
                                       "Remember to remain patient and check acceptance criteria carefully.",
                                       "--output-format", "stream-json", "--design", "off",
                                       "--max-iters", "3", "--yes"])
            require(result["exit"] == 0, f"{phase} failed: {result}")
            require(not recorder.errors, repr(recorder.errors))
            stdout = root.joinpath(f"{phase}.stdout").read_text(encoding="utf-8")
            events = [json.loads(line) for line in stdout.splitlines() if line.strip()]
            require(any(event.get("type") == "result" and not event.get("is_error", False) for event in events),
                    f"{phase}: missing successful structured result")
            kinds = [record["kind"] for record in recorder.records if record["phase"] == phase]
            expected = {"first-episode": ["agent", "secretary"],
                        "second-episode": ["agent", "secretary", "reflection"],
                        "disabled": ["agent"]}[phase]
            require(kinds == expected, f"{phase}: model calls {kinds} != {expected}")
            skill_files = list((home / "skills" / "p").glob(f"*/{SKILL}.md"))
            require(len(skill_files) == 1 and STEPS in skill_files[0].read_text(encoding="utf-8"),
                    "learned Skill is not durable in this project zone")
            self_files = list((home / "personas/fixture-guide.self").glob("*.md"))
            stored = "\n".join(path.read_text(encoding="utf-8") for path in self_files)
            require(EPISODES[0] in stored, "first persona episode not durable")
            if phase != "first-episode":
                require(EPISODES[1] in stored and INSIGHT in stored, "reflection was not durable")
            if phase == "disabled":
                require(before == {name: support.inventory(home / name) for name in before},
                        "disabled learning changed Skills or Persona files")
        require(not (home / "memory-data").exists(), "disabled provider unexpectedly created Memory data")
        report.update(passed=True, requests=recorder.records)
    except Exception:
        report["error"] = traceback.format_exc()
    finally:
        server.shutdown()
        settled = server.drain_owned_connections()
        server.server_close()
        thread.join(timeout=5)
        report["cleanup"] = {"connections_settled": settled, "server_stopped": not thread.is_alive(),
                             "process_errors": runner.cleanup_failures}
        if not settled or thread.is_alive() or runner.cleanup_failures:
            report["passed"] = False
        (root / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
