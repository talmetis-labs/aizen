"""Actual CLI owned-workflow acceptance against a loopback provider and disposable Git.

Run with Python -I and --binary <freshly built aizen>. No external provider, real
user configuration, installed binary, or development-repository Git mutation is used.
Artifacts are retained on both success and failure for inspecting receipts/run logs.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def require(condition, detail):
    if not condition:
        raise AssertionError(detail)


def load_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def run(binary):
    binary = binary.resolve(strict=True)
    git_exe = shutil.which("git")
    require(git_exe, "Git is required for this acceptance fixture")
    artifact = Path(tempfile.mkdtemp(prefix="aizen-workflow-cli-"))
    print(f"ARTIFACTS {artifact}", flush=True)
    root, home, runs, logs = (artifact / name for name in ("repo", "home", "runs", "logs"))
    for directory in (root, home, runs, logs):
        directory.mkdir()
    env = {
        key: value for key, value in os.environ.items()
        if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_", "GIT_"))
        and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY")
    }
    env.update({"AIZEN_HOME": str(home), "AIZEN_RUNS_DIR": str(runs),
                "AIZEN_NO_HOOKS": "1", "AIZEN_ARCHITECT": "0", "NO_PROXY": "127.0.0.1",
                "WORKFLOW_FIXTURE_FAIL": "0"})
    # Isolate fixture Git configuration from global identity, hooks, signing and aliases.
    empty_config = artifact / "empty-git-config"
    empty_config.write_text("", encoding="utf-8")
    env.update({"GIT_CONFIG_GLOBAL": str(empty_config), "GIT_CONFIG_NOSYSTEM": "1"})

    def git(*args):
        result = subprocess.run([git_exe, *args], cwd=root, env=env,
                                capture_output=True, timeout=30)
        require(result.returncode == 0, (args, result.returncode, result.stderr.decode(errors="replace")))
        return result.stdout.decode("utf-8").strip()

    def cli(label, args, success=True, extra_env=None):
        call_env = dict(env)
        call_env.update(extra_env or {})
        print(f"START {label}", flush=True)
        result = subprocess.run([str(binary), "--sandbox", "guarded", *args],
                                cwd=root, env=call_env, input=b"", capture_output=True, timeout=180)
        (logs / f"{label}.stdout").write_bytes(result.stdout)
        (logs / f"{label}.stderr").write_bytes(result.stderr)
        print(f"END {label} exit={result.returncode}", flush=True)
        require((result.returncode == 0) == success,
                (label, result.returncode, result.stdout.decode(errors="replace")[-3000:],
                 result.stderr.decode(errors="replace")[-6000:]))
        require(not state["errors"], state["errors"])
        return result

    git("init", "-q")
    for key, value in (("user.name", "Fixture"), ("user.email", "fixture@example.invalid"),
                       ("commit.gpgsign", "false"), ("core.autocrlf", "false")):
        git("config", key, value)
    (root / ".aizen").mkdir()
    (root / "tracked.txt").write_bytes(b"base\n")
    (root / ".gitignore").write_text("target/\n", encoding="utf-8")
    # A standing fixture grant exercises run-resume's ordinary Ask mode without a TTY.
    (root / ".aizen" / "approvals.json").write_text(
        json.dumps({"allow": [{"tool": "file_edit"}]}), encoding="utf-8")
    verify_command = f'"{sys.executable}" -I verify.py'
    (root / ".aizen" / "verify.json").write_text(
        json.dumps({"commands": [verify_command], "timeout_secs": 30}), encoding="utf-8")
    (root / "verify.py").write_text(
        'import json, os, sys\nfrom pathlib import Path\n'
        'root = Path.cwd().resolve()\n'
        'text = (root / "tracked.txt").read_text(encoding="utf-8")\n'
        'assert text in ("first child\\n", "dependent child\\n"), text\n'
        'if os.environ.get("WORKFLOW_FIXTURE_FAIL") == "1":\n    sys.exit(7)\n'
        'print("WORKFLOW_VERIFY_PROOF " + json.dumps({"root": str(root), "text": text}))\n', encoding="utf-8")
    git("add", "--", "tracked.txt", ".gitignore", "verify.py", ".aizen/verify.json", ".aizen/approvals.json")
    git("commit", "-qm", "base fixture")
    before = git("rev-parse", "HEAD")
    index = (root / ".git" / "index").read_bytes()
    base_files = {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
                  for path in root.rglob("*") if path.is_file() and ".git" not in path.relative_to(root).parts}
    state = {"steps": {"owned-first": 0, "owned-dependent": 0}, "errors": [], "synthesis": 0}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def respond(self, value, stream=False):
            if stream:
                wire = b"data: " + json.dumps(value).encode() + b"\n\ndata: [DONE]\n\n"
            else:
                wire = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream" if stream else "application/json")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

        def do_GET(self):
            require(self.path.startswith("/v1/models"), self.path)
            self.respond({"data": [{"id": model, "context_window": 200000}
                                   for model in ("owned-first", "owned-dependent", "fixture-synthesis")]})

        def do_POST(self):
            try:
                require(self.path == "/v1/chat/completions", self.path)
                length = int(self.headers.get("Content-Length", "0"))
                require(0 < length < 512 * 1024, length)
                body = json.loads(self.rfile.read(length))
                model = body["model"]
                if model == "fixture-synthesis":
                    state["synthesis"] += 1
                    delta, finish = {"content": "Both owned tasks were explicitly integrated and verified."}, "stop"
                else:
                    require(model in state["steps"], model)
                    step = state["steps"][model]
                    state["steps"][model] += 1
                    require(step < 3, ("unexpected retry or redispatch", model, step))
                    names = {tool["function"]["name"] for tool in body["tools"]}
                    require(names and names <= {"file_read", "file_edit", "file_write"}, names)
                    tools = [m for m in body["messages"] if m["role"] == "tool"]
                    require(len(tools) == step, (model, step, tools))
                    require(all(not m["content"].startswith("error:") for m in tools), tools)
                    old, new = ("base", "first child") if model == "owned-first" else ("first child", "dependent child")
                    if step == 0:
                        name, arguments = "file_read", {"path": "tracked.txt"}
                    elif step == 1:
                        require(old in tools[0]["content"], ("wrong child base/root", tools))
                        name, arguments = "file_edit", {"path": "tracked.txt", "old_string": old, "new_string": new}
                    else:
                        require(tools[-1]["content"].startswith("edited tracked.txt"), tools)
                        delta, finish = {"content": "Updated tracked.txt only. Parent verification remains required."}, "stop"
                    if step < 2:
                        delta = {"tool_calls": [{"index": 0, "id": f"{model}-{step}", "type": "function",
                                                  "function": {"name": name, "arguments": json.dumps(arguments)}}]}
                        finish = "tool_calls"
                self.respond({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}, stream=True)
            except Exception as error:
                state["errors"].append(repr(error))
                self.send_error(400, "loopback fixture assertion failed")

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base_url = f"http://127.0.0.1:{server.server_port}/v1"
    (home / "cli-config.json").write_text(json.dumps({
        "base_url": base_url, "api_key": "fixture", "model": "owned-first", "architect_mode": False,
        "self_review": False, "eager_tools": False, "compact_threshold_pct": 0,
    }), encoding="utf-8")
    spec = artifact / "workflow.json"
    spec.write_text(json.dumps({"name": "owned-cli-acceptance", "tasks": [
        {"id": "first", "role": "daedalus", "model": "owned-first", "isolation": "worktree",
         "write_paths": ["tracked.txt"], "max_steps": 4,
         "prompt": "Read tracked.txt, then replace base with first child using only file_edit. Return changed paths; do not verify or integrate."},
        {"id": "dependent", "role": "daedalus", "model": "owned-dependent", "isolation": "worktree",
         "write_paths": ["tracked.txt"], "max_steps": 4, "after": ["first"],
         "prompt": "Read tracked.txt, then replace first child with dependent child using only file_edit. Return changed paths; do not verify or integrate."},
    ], "synthesis": {"model": "fixture-synthesis"}}), encoding="utf-8")

    def events():
        return [json.loads(line) for line in runlog.read_text(encoding="utf-8").splitlines() if line.strip()]

    def handoff(task_id):
        completed = [e for e in events() if e["kind"] == "attempt_completed" and e["key"]["task_id"] == task_id]
        require(len(completed) == 1, completed)
        require(completed[0]["status"] == "git-handoff-pending", completed)
        result = completed[0]["git_handoff"]
        require(not result["verified"] and not result["integrated"] and not result["dirty"], result)
        require(result["paths"] == ["tracked.txt"] and result["commits"] == [result["tip"]], result)
        return result

    def no_settlement(task_id):
        require(not any(e["kind"] == "git_task_accepted" and e["key"]["task_id"] == task_id
                        or e["kind"] == "task_settled" and e["task_id"] == task_id for e in events()), events())

    def resume(label, success=False):
        result = cli(label, ["run-resume", runlog.stem, "--spec", str(spec), "--yes"], success=success)
        if not success:
            require(b"workflow parked:" in result.stderr, (label, result.stderr.decode(errors="replace")))
        return result

    def records(kind):
        candidates = list(home.glob(f"{kind}/*/*/*.json"))
        return [load_json(path) for path in candidates]

    def integrate_and_verify(task_id, h):
        registration = h["registration"]["registration_id"]
        old_head = git("rev-parse", "HEAD")
        old_index = (root / ".git/index").read_bytes()
        cli(f"{task_id}-integration-preview", ["time", "integrate-task", registration])
        require(git("rev-parse", "HEAD") == old_head and (root / ".git/index").read_bytes() == old_index,
                "preview changed destination HEAD/index")
        cli(f"{task_id}-integrate", ["time", "integrate-task", registration, "--yes"])
        require(git("rev-parse", "HEAD") == h["tip"], "integration did not reach the exact child tip")
        integration = [r for r in records("git-operations-v1")
                       if r["change"] == {"action": "integrate_task", "receipt": registration}]
        require(len(integration) == 1, ("missing exact integration", integration, list(home.rglob("*.json"))))
        receipt = integration[0]
        require(receipt["state"] == "settled_unverified" and not receipt["verified"], receipt)
        return receipt

    try:
        cli("trust-fixture", ["mcp", "trust"])
        initial = cli("initial-park", ["workflow", str(spec), "--yes"], success=False)
        require(b"workflow parked:" in initial.stderr, initial.stderr.decode(errors="replace"))
        runlogs = list(runs.glob("*.jsonl"))
        require(len(runlogs) == 1, runlogs)
        runlog = runlogs[0]
        first = handoff("first")
        require(first["base"] == before, first)
        require(git("rev-parse", "HEAD") == before, "child moved parent HEAD")
        require((root / ".git/index").read_bytes() == index, "child changed parent index")
        for name, digest in base_files.items():
            require(hashlib.sha256((root / name).read_bytes()).hexdigest() == digest, name)
        require(state["steps"] == {"owned-first": 3, "owned-dependent": 0}, state)
        no_settlement("first")
        resume("resume-without-integration")
        require(state["steps"] == {"owned-first": 3, "owned-dependent": 0}, state)
        # Dirty destination refusal must retain both pending user bytes and the child's handoff.
        (root / "tracked.txt").write_bytes(b"pending parent edit\n")
        dirty_index = (root / ".git/index").read_bytes()
        dirty = cli("dirty-integration-refused", ["time", "integrate-task", first["registration"]["registration_id"], "--yes"], success=False)
        require(b"worktree/index must be clean" in dirty.stderr, dirty.stderr.decode(errors="replace"))
        require((root / "tracked.txt").read_bytes() == b"pending parent edit\n", "dirty parent overwritten")
        require((root / ".git/index").read_bytes() == dirty_index and git("rev-parse", "HEAD") == before,
                "dirty refusal changed parent index/HEAD")
        # Restore only bytes created by this fixture, in its disposable repository.
        (root / "tracked.txt").write_bytes(b"base\n")
        integration = integrate_and_verify("first", first)
        resume("resume-before-verification")
        require(state["steps"]["owned-dependent"] == 0, "integration alone released dependency")
        no_settlement("first")
        cli("first-verify-fails", ["time", "verify-task", integration["id"], "--yes"],
            extra_env={"WORKFLOW_FIXTURE_FAIL": "1"})
        failed = records("verification-v1")
        require(len(failed) == 1 and not failed[0]["verified"] and failed[0]["state"] == "settled", failed)
        resume("resume-after-failed-verification")
        require(state["steps"]["owned-dependent"] == 0, "failed verification released dependency")
        no_settlement("first")
        cli("first-verify", ["time", "verify-task", integration["id"], "--yes"])
        verified = [r for r in records("verification-v1") if r["verified"]]
        require(len(verified) == 1, verified)
        proof_line = next(line for line in verified[0]["output"].splitlines()
                          if line.startswith("WORKFLOW_VERIFY_PROOF "))
        proof = json.loads(proof_line.removeprefix("WORKFLOW_VERIFY_PROOF "))
        require(Path(proof["root"]).samefile(root) and proof["text"] == "first child\n", proof)
        resume("resume-launches-dependent")
        require(state["steps"] == {"owned-first": 3, "owned-dependent": 3}, state)
        second = handoff("dependent")
        require(second["base"] == first["tip"], "dependent did not use the verified integrated tip")
        require(Path(second["root"]) != Path(first["root"]), "children shared a checkout")
        require((Path(first["root"]) / "tracked.txt").read_bytes() == b"first child\n", "dependent wrote sibling")
        require((Path(second["root"]) / "tracked.txt").read_bytes() == b"dependent child\n", "dependent root wrong")
        require((root / "tracked.txt").read_bytes() == b"first child\n", "dependent wrote parent")
        dependent_dispatch = next(e for e in events() if e["kind"] == "dispatch_intent" and e["intent"]["task"]["id"] == "dependent")
        accepted = next(e for e in events() if e["kind"] == "git_task_accepted" and e["key"]["task_id"] == "first")
        settled = next(e for e in events() if e["kind"] == "task_settled" and e["task_id"] == "first")
        require(accepted["seq"] < settled["seq"] < dependent_dispatch["seq"], "release ordering violated")
        require(dependent_dispatch["intent"]["task"]["git_base"] == first["tip"], dependent_dispatch)
        require(dependent_dispatch["intent"]["task"]["git_prerequisites"] == [first["tip"]], dependent_dispatch)
        no_settlement("dependent")
        second_integration = integrate_and_verify("dependent", second)
        cli("dependent-verify", ["time", "verify-task", second_integration["id"], "--yes"])
        resume("resume-completes", success=True)
        require(len([e for e in events() if e["kind"] == "git_task_accepted"]) == 2, events())
        require(len([e for e in events() if e["kind"] == "task_settled"]) == 2, events())
        require(len([e for e in events() if e["kind"] == "dispatch_intent"]) == 2, "child redispatched")
        require(len(list(runs.glob("*.jsonl"))) == 1, "resume created a different run")
        require(state["synthesis"] == 1, state)
        cli("completed-dry-run", ["run-resume", runlog.stem])
        require(state["steps"] == {"owned-first": 3, "owned-dependent": 3}, state)
        require(git("rev-parse", "HEAD") == second["tip"] and not git("status", "--porcelain"), "destination not clean")
        summary = {"run_id": runlog.stem, "base": before, "first_tip": first["tip"],
                   "dependent_base": second["base"], "dependent_tip": second["tip"],
                   "provider_calls": state["steps"], "synthesis_calls": state["synthesis"],
                   "checks": "park/approval/failing-check/verification/same-run-resume/dependent-root passed"}
        (artifact / "result.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
        print("PASS real CLI workflow with isolated children and verification-gated dependency release", flush=True)
        return artifact
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        (artifact / "provider-state.json").write_text(json.dumps(state, indent=2), encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    run(parser.parse_args().binary)
