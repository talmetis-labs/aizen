"""Actual TaskTool isolation=worktree CLI acceptance, not a workflow substitute.

Run only after coordinator GO, with Python -I and --binary <fresh approved binary>.
All new state is disposable and retained in a short system-Temp fixture root. No product
build, install, timeout override, cleanup, integration, or verification is performed.
A loopback provider drives the ordinary agent/task/native-tool path. Fixture-only
standing tool grants use the normal approvals mechanism, without a prompt bypass.
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
import time
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MARKER = "[Git task handoff: UNVERIFIED, NOT INTEGRATED]"
PARENT_MODEL = "task-parent-fixture"
CHILD_MODEL = "task-native-fixture"
CHANGED = ["child-only.txt", "tracked.txt"]
CHILD_TEXT = b"native child edit\n"
NEW_TEXT = b"owned child creation\n"
NATIVE_KINDS = ["read", "edit", "write", "read"]


def require(condition, detail):
    if not condition:
        raise AssertionError(detail)


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def canonical(path):
    # Rust fs::canonicalize returns extended Windows paths; normalize only that
    # spelling for containment checks while preserving real filesystem identity.
    value = str(Path(path).resolve(strict=True))
    if value.startswith("\\\\?\\UNC\\"):
        value = "\\\\" + value[8:]
    elif value.startswith("\\\\?\\"):
        value = value[4:]
    return Path(value)


def save_json(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False), encoding="utf-8")


def handoff_from(text):
    require(MARKER in text, ("missing actual TaskTool handoff", text))
    suffix = text.split(MARKER, 1)[1].lstrip()
    handoff, _ = json.JSONDecoder().raw_decode(suffix)
    require(isinstance(handoff, dict), handoff)
    return handoff


def tool_text(message):
    content = message.get("content", "")
    if isinstance(content, str):
        return content
    require(isinstance(content, list), content)
    return "\n".join(item.get("text", "") for item in content if isinstance(item, dict))


def run(binary):
    binary = binary.resolve(strict=True)
    driver = Path(__file__).resolve(strict=True)
    artifact = Path(tempfile.mkdtemp(prefix="az-task-"))
    print("ARTIFACTS " + str(artifact), flush=True)
    root, home, runs, logs, requests, profile = (artifact / name for name in
        ("repo", "home", "runs", "logs", "provider-requests", "profile"))
    for path in (root, home, runs, logs, requests, profile):
        path.mkdir()
    git_exe = shutil.which("git")
    require(git_exe, "Git must already be available; this driver does not install it")
    env = {key: value for key, value in os.environ.items()
        if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_", "GIT_"))
        and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY")}
    empty_config = artifact / "empty-git-config"
    empty_config.write_bytes(b"")
    env.update(AIZEN_HOME=str(home), AIZEN_RUNS_DIR=str(runs), AIZEN_ARCHITECT="0",
        NO_PROXY="127.0.0.1", GIT_CONFIG_GLOBAL=str(empty_config), GIT_CONFIG_NOSYSTEM="1",
        GIT_OPTIONAL_LOCKS="0", HOME=str(profile), USERPROFILE=str(profile),
        APPDATA=str(profile / "Roaming"), LOCALAPPDATA=str(profile / "Local"),
        XDG_CONFIG_HOME=str(profile / "xdg-config"), XDG_CACHE_HOME=str(profile / "xdg-cache"),
        TEMP=str(profile / "tmp"), TMP=str(profile / "tmp"), TMPDIR=str(profile / "tmp"))
    for key in ("APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "TEMP"):
        Path(env[key]).mkdir()
    # A frozen executable/driver hash is evidence, not a claim that the source compiled.
    hashes = {"binary": str(binary), "binary_sha256": digest(binary),
              "driver": str(driver), "driver_sha256": digest(driver),
              "python": sys.executable, "git": git_exe}
    save_json(artifact / "inputs.json", hashes)
    command_counter = 0
    state = {"parent_calls": 0, "child_calls": 0, "errors": [], "handoff": None,
             "intent_snapshots": [], "events": [], "phase": "initial"}
    state_lock = threading.Lock()

    def command(label, argv, cwd=root, timeout=30):
        nonlocal command_counter
        require(canonical(cwd).is_relative_to(canonical(artifact)), ("non-fixture cwd", str(cwd)))
        command_counter += 1
        prefix = logs / f"{command_counter:03d}-{label}"
        save_json(prefix.with_suffix(".command.json"),
            {"argv": [str(arg) for arg in argv], "cwd": str(cwd), "timeout_seconds": timeout})
        print("START " + label, flush=True)
        try:
            result = subprocess.run([str(arg) for arg in argv], cwd=cwd, env=env,
                input=b"", capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired as error:
            prefix.with_suffix(".stdout").write_bytes(error.stdout or b"")
            prefix.with_suffix(".stderr").write_bytes(error.stderr or b"")
            save_json(prefix.with_suffix(".exit.json"), {"timed_out": True, "timeout_seconds": timeout})
            raise
        prefix.with_suffix(".stdout").write_bytes(result.stdout)
        prefix.with_suffix(".stderr").write_bytes(result.stderr)
        save_json(prefix.with_suffix(".exit.json"), {"exit_code": result.returncode, "timed_out": False})
        print(f"END {label} direct_exit={result.returncode}", flush=True)
        return result

    def git(*args, cwd=root):
        result = command("git-" + str(args[0]), [git_exe, *args], cwd=cwd)
        require(result.returncode == 0,
            (args, result.returncode, result.stderr.decode("utf-8", errors="replace")))
        return result.stdout

    def cli(label, args):
        result = command(label, [binary, "--sandbox", "guarded", *args], timeout=180)
        require(result.returncode == 0, (label, result.returncode,
            result.stdout.decode("utf-8", errors="replace")[-5000:],
            result.stderr.decode("utf-8", errors="replace")[-5000:]))
        require(not state["errors"], state["errors"])
        return result

    def snapshot():
        # Optional Git locks are disabled so read-only snapshots cannot refresh the index.
        return {"head_oid": git("rev-parse", "HEAD").decode().strip(),
            "head_bytes": (root / ".git" / "HEAD").read_bytes().hex(),
            "index_bytes": (root / ".git" / "index").read_bytes().hex(),
            "status": git("status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignored=matching").hex(),
            "staged_diff": git("diff", "--cached", "--binary", "--no-ext-diff", "--no-textconv").hex(),
            "unstaged_diff": git("diff", "--binary", "--no-ext-diff", "--no-textconv").hex(),
            "files": {str(path.relative_to(root)): path.read_bytes().hex()
                for path in root.rglob("*") if path.is_file() and ".git" not in path.relative_to(root).parts}}

    def registry_files():
        return sorted(home.glob("coop/*/sessions/git-task-*.json"))

    def manifest_at_step(step):
        files = registry_files()
        require(len(files) == 1, ("expected one actual task registration", [str(p) for p in files]))
        raw = files[0].read_bytes()
        require(len(raw) <= 131072, ("unbounded registration", len(raw)))
        manifest = json.loads(raw)
        require(manifest["session_id"].startswith("git-task-v2-"), manifest["session_id"])
        native = manifest["git_task"]["native_lifecycle"]
        require((native["version"], native["max_workers"], native["max_manifest_bytes"]) == (1, 128, 131072), native)
        evidence = native["evidence"]
        require(evidence["state"] == "open", evidence)
        workers = evidence["workers"]
        require([worker["kind"] for worker in workers] == NATIVE_KINDS[:step], workers)
        require(all(worker["ordinal"] == index + 1 and worker["closed"] is None
                    and len(worker["prepared_sha256"]) == 32
                    for index, worker in enumerate(workers)), workers)
        target = artifact / f"native-open-step-{step}.json"
        target.write_bytes(raw)
        state["intent_snapshots"].append({"step": step, "path": str(target), "sha256": digest(target)})

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def respond(self, value, stream=False):
            wire = json.dumps(value).encode("utf-8")
            if stream:
                wire = b"data: " + wire + b"\n\ndata: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream" if stream else "application/json")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

        def do_GET(self):
            try:
                require(self.path.startswith("/v1/models"), self.path)
                self.respond({"data": [{"id": model, "context_window": 200000}
                    for model in (PARENT_MODEL, CHILD_MODEL)]})
            except Exception as error:
                state["errors"].append(repr(error))
                self.send_error(400, "fixture assertion failed")

        def do_POST(self):
            try:
                require(self.path == "/v1/chat/completions", self.path)
                length = int(self.headers.get("Content-Length", "0"))
                require(0 < length < 1024 * 1024, length)
                body = json.loads(self.rfile.read(length))
                require(body.get("stream") is True, "unexpected nonstream background/provider call")
                model = body["model"]
                require(model in (PARENT_MODEL, CHILD_MODEL), model)
                key = "parent_calls" if model == PARENT_MODEL else "child_calls"
                with state_lock:
                    step = state[key]
                    state[key] += 1
                    state["events"].append({"model": model, "step": step, "phase": state["phase"], "time": time.monotonic()})
                save_json(requests / f"{model}-{step}.json", body)
                results = [message for message in body["messages"] if message["role"] == "tool"]
                names = {tool["function"]["name"] for tool in body.get("tools", [])}
                if model == PARENT_MODEL:
                    require("task" in names, ("actual TaskTool was not advertised", names))
                    require(step < 3, ("unexpected parent retry or completion loop", step))
                    if step == 0:
                        require(state["phase"] == "initial" and not results, results)
                        name, args = "task", {"role": "daedalus", "model": CHILD_MODEL,
                            "label": "actual-owned-task", "isolation": "worktree", "execution": "direct",
                            "write_paths": CHANGED, "max_steps": 6,
                            "prompt": "Read tracked.txt; replace base with native child edit using file_edit; create child-only.txt with owned child creation plus newline using file_write purpose project; read tracked.txt back. Return exact changed paths, not verified or integrated. Do not use shell or any other tools.",
                            "boundaries": "Only native file tools on tracked.txt and child-only.txt in this owned worktree; do not integrate, verify, restore or retry.",
                            "expected_output": "Changed paths and explicit unverified, not-integrated status."}
                        delta, finish = self.tool_call("parent-task-0", name, args), "tool_calls"
                    else:
                        require(len(results) == 1, ("missing/duplicate task result", results))
                        handoff = handoff_from(tool_text(results[0]))
                        if step == 1:
                            require(state["phase"] == "initial" and state["child_calls"] == 5, state)
                            state["handoff"] = handoff
                            save_json(artifact / "provider-task-handoff.json", handoff)
                        else:
                            require(state["phase"] == "resume", state)
                            require(handoff == state["handoff"], "saved session lost or changed its handoff")
                            require(state["child_calls"] == 5, "resume redispatched the child")
                        delta, finish = {"content": "The owned task handoff is retained. It is UNVERIFIED and NOT INTEGRATED. No integration, verification or retry was performed."}, "stop"
                else:
                    require(state["phase"] == "initial" and step < 5, ("unexpected child retry or redispatch", step))
                    require(names == {"file_read", "file_edit", "file_write"}, names)
                    require(len(results) == step, (step, results))
                    require(all(not tool_text(message).startswith("error:") for message in results), results)
                    manifest_at_step(step)
                    if step == 0:
                        name, args = "file_read", {"path": "tracked.txt"}
                    elif step == 1:
                        content = tool_text(results[-1])
                        require("base" in content and "parent staged" not in content and "parent unstaged" not in content, ("child did not read committed base", content))
                        name, args = "file_edit", {"path": "tracked.txt", "old_string": "base", "new_string": "native child edit"}
                    elif step == 2:
                        require(tool_text(results[-1]).startswith("edited tracked.txt"), results[-1])
                        name, args = "file_write", {"path": "child-only.txt", "content": NEW_TEXT.decode(), "purpose": "project"}
                    elif step == 3:
                        name, args = "file_read", {"path": "tracked.txt"}
                    else:
                        require("native child edit" in tool_text(results[-1]), results[-1])
                        delta, finish = {"content": "Changed tracked.txt and child-only.txt with native file tools. Re-read tracked.txt. Project tests were not run. UNVERIFIED and NOT INTEGRATED."}, "stop"
                    if step < 4:
                        delta, finish = self.tool_call(f"child-native-{step}", name, args), "tool_calls"
                self.respond({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}, stream=True)
            except Exception as error:
                with state_lock:
                    state["errors"].append(repr(error))
                self.send_error(400, "loopback fixture assertion failed")

        @staticmethod
        def tool_call(identifier, name, args):
            return {"tool_calls": [{"index": 0, "id": identifier, "type": "function",
                "function": {"name": name, "arguments": json.dumps(args)}}]}

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    save_json(home / "cli-config.json", {"base_url": f"http://127.0.0.1:{server.server_port}/v1",
        "api_key": "fixture", "model": PARENT_MODEL, "architect_mode": False,
        "self_review": False, "eager_tools": False, "compact_threshold_pct": 0,
        "auto_skill_learn": False, "persona_evolve": False, "memory_auto_learn": False})
    try:
        git("init", "-q")
        for key, value in (("user.name", "Fixture"), ("user.email", "fixture@example.invalid"),
            ("commit.gpgsign", "false"), ("core.autocrlf", "false"), ("gc.auto", "0"), ("maintenance.auto", "false")):
            git("config", key, value)
        (root / ".aizen").mkdir()
        (root / "tracked.txt").write_bytes(b"base\n")
        (root / ".gitignore").write_bytes(b"private.tmp\n")
        save_json(root / ".aizen" / "approvals.json", {"allow": [
            {"tool": "task"}, {"tool": "file_edit"}, {"tool": "file_write"}]})
        git("add", "--", "tracked.txt", ".gitignore", ".aizen/approvals.json")
        git("commit", "-qm", "disposable TaskTool acceptance base")
        base = git("rev-parse", "HEAD").decode().strip()
        cli("trust-disposable-fixture", ["mcp", "trust"])
        # TaskTool creation freezes committed HEAD, unlike workflow's verification preflight.
        # Keep all four kinds of parent WIP in place for the positive test; never reset/restore.
        (root / "tracked.txt").write_bytes(b"parent staged\n")
        git("add", "--", "tracked.txt")
        (root / "tracked.txt").write_bytes(b"parent unstaged\n")
        (root / "pending.txt").write_bytes(b"parent untracked\n")
        (root / "private.tmp").write_bytes(b"private ignored fixture sentinel\n")
        before = snapshot()
        save_json(artifact / "parent-before.json", before)
        prompt = "Use task exactly once with role daedalus and isolation worktree to make the bounded native edits specified by the child. Leave every parent file, parent index and parent HEAD untouched. Return the child's retained handoff as unverified and not integrated. Do not verify, integrate or retry."
        initial = cli("actual-task-worktree", ["agent", prompt, "--model", PARENT_MODEL,
            "--max-iters", "6", "--save-session", "--output-format", "stream-json"])
        events = [json.loads(line) for line in initial.stdout.decode("utf-8").splitlines() if line.strip()]
        require(not any(event.get("type") == "control_request" for event in events), "fixture standing grants were not sufficient")
        task_calls = [block for event in events if event.get("type") == "assistant"
            for block in event.get("message", {}).get("content", [])
            if isinstance(block, dict) and block.get("type") == "tool_use" and block.get("name") == "task"]
        require(len(task_calls) == 1 and task_calls[0]["input"]["isolation"] == "worktree"
            and task_calls[0]["input"]["write_paths"] == CHANGED, task_calls)
        task_results = [event for event in events if event.get("tool_use_result", {}).get("name") == "task"]
        require(len(task_results) == 1, ("expected actual single TaskTool result event", task_results))
        block = task_results[0]["message"]["content"][0]
        require(block["type"] == "tool_result" and not block["is_error"] and not task_results[0]["tool_use_result"]["truncated"], block)
        handoff = handoff_from(block["content"])
        require(handoff == state["handoff"], "stream and provider handoff differ")
        require(handoff["base"] == base and handoff["tip"] != base and handoff["commits"] == [handoff["tip"]], handoff)
        require(handoff["paths"] == CHANGED and not handoff["dirty"] and not handoff["verified"] and not handoff["integrated"]
            and handoff["tests"] == "not_run_native_only", handoff)
        child = canonical(handoff["root"])
        require(child != canonical(root) and child.is_relative_to(canonical(home)), ("wrong owned root", str(child)))
        require((child / "tracked.txt").read_bytes() == CHILD_TEXT and (child / "child-only.txt").read_bytes() == NEW_TEXT, "native child bytes differ")
        require(not (root / "child-only.txt").exists() and not (child / "pending.txt").exists() and not (child / "private.tmp").exists(), "parent WIP copied or child output leaked")
        require(git("status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignored=matching", cwd=child) == b"", "child is not clean")
        require(git("show", handoff["tip"] + ":tracked.txt", cwd=child) == CHILD_TEXT, "committed edit differs")
        require(git("show", handoff["tip"] + ":child-only.txt", cwd=child) == NEW_TEXT, "committed new file differs")
        require(git("rev-list", "--count", base + ".." + handoff["tip"], cwd=child).strip() == b"1", "duplicate child commit")
        require(git("show", "-s", "--format=%P", handoff["tip"], cwd=child).decode().strip() == base, "child commit has incorrect parent")
        after = snapshot()
        save_json(artifact / "parent-after-task.json", after)
        require(after == before, "TaskTool changed parent staged/unstaged/untracked/ignored bytes, index or HEAD")
        manifests = registry_files()
        require(len(manifests) == 1, manifests)
        manifest_bytes = manifests[0].read_bytes()
        manifest = json.loads(manifest_bytes)
        receipt = handoff["registration"]
        require(manifest["session_id"] == receipt["registration_id"] and receipt["registration_id"].startswith("git-task-v2-"), manifest)
        require(list(hashlib.sha256(manifest_bytes).digest()) == receipt["manifest_sha256"], "receipt is not SHA256 of exact durable registration")
        require(manifest["state"] == "finished" and not manifest["coverage_gap"] and manifest["degraded"] is None, manifest)
        task = manifest["git_task"]
        require(task["phase"] == "handed_off" and task["base"] == base and task["tip"] == handoff["tip"]
            and task["paths"] == CHANGED and not task["verified"], task)
        evidence = task["native_lifecycle"]["evidence"]
        require(evidence["state"] == "released" and evidence["driver"] == "completed" and evidence["ownership_released"] is True, evidence)
        binding = evidence["binding"]
        for key in ("repo_id", "worktree_id", "root", "branch", "owner", "base"):
            require(binding[key] == handoff[key], ("binding/handoff mismatch", key, binding, handoff))
        require(binding["registration"] == receipt["registration_id"] and len(binding["scope_sha256"]) == 32, binding)
        require(canonical(binding["git_dir"]) != canonical(root / ".git")
            and canonical(binding["common_dir"]) == canonical(root / ".git"), "child is not a distinct Git-linked worktree")
        workers = evidence["workers"]
        require([worker["kind"] for worker in workers] == NATIVE_KINDS, workers)
        require(all(worker["ordinal"] == i + 1 and len(worker["prepared_sha256"]) == 32
            and worker["closed"] == {"result": "returned", "effect": {"state": "settled"}}
            for i, worker in enumerate(workers)), workers)
        (artifact / "native-final-registration.json").write_bytes(manifest_bytes)
        save_json(artifact / "task-handoff.json", handoff)
        finals = [event for event in events if event.get("type") == "result"]
        require(len(finals) == 1 and not finals[0]["is_error"] and isinstance(finals[0].get("session"), str), finals)
        session = finals[0]["session"]
        inventory = git("worktree", "list", "--porcelain", "-z")
        refs = git("for-each-ref", "--format=%(refname) %(objectname)")
        commits = git("rev-list", "--all")
        child_index = Path(binding["git_dir"]) / "index"
        child_index_bytes = child_index.read_bytes()
        state["phase"] = "resume"
        resumed = cli("resume-retained-task-no-redispatch", ["agent",
            "Report the retained Git task handoff from this conversation. Do not call tools, redispatch, retry, commit, integrate or verify.",
            "--model", PARENT_MODEL, "--resume", session, "--max-iters", "6", "--save-session", "--output-format", "stream-json"])
        resumed_events = [json.loads(line) for line in resumed.stdout.decode("utf-8").splitlines() if line.strip()]
        resumed_finals = [event for event in resumed_events if event.get("type") == "result"]
        require(len(resumed_finals) == 1 and not resumed_finals[0]["is_error"] and resumed_finals[0]["session"] == session, resumed_finals)
        require(not any(event.get("tool_use_result") for event in resumed_events), "resume executed tools")
        require(state["parent_calls"] == 3 and state["child_calls"] == 5 and not state["errors"], state)
        require(registry_files() == manifests and manifests[0].read_bytes() == manifest_bytes, "resume changed or duplicated registration")
        require(git("worktree", "list", "--porcelain", "-z") == inventory and git("for-each-ref", "--format=%(refname) %(objectname)") == refs
            and git("rev-list", "--all") == commits and child_index.read_bytes() == child_index_bytes, "resume allocated, committed or changed child index")
        after_resume = snapshot()
        save_json(artifact / "parent-after-resume.json", after_resume)
        require(after_resume == before, "session resume altered parent state")
        require((child / "tracked.txt").read_bytes() == CHILD_TEXT and (child / "child-only.txt").read_bytes() == NEW_TEXT, "resume changed child bytes")
        require(digest(binary) == hashes["binary_sha256"] and digest(driver) == hashes["driver_sha256"], "binary or driver changed during acceptance")
        summary = {"status": "passed", **hashes, "artifacts": str(artifact), "session": session,
            "handoff": handoff, "provider_calls": {"parent": state["parent_calls"], "child": state["child_calls"]},
            "checks": ["actual advertised TaskTool isolation=worktree path", "committed-base read in distinct owned child",
                "native read/edit/write/read plus durable v2 open intents and physical closure", "exact parent staged/unstaged/untracked/ignored bytes, index and HEAD preserved",
                "single child-only commit and exact typed unverified/unintegrated handoff", "saved session resume preserved handoff without redispatch or duplicate commit"],
            "limits": ["native tools only, not shell/build isolation", "conversation resume only, not crash recovery or registration adoption", "no destination integration or project verification performed"]}
        save_json(artifact / "result.json", summary)
        print(json.dumps(summary, indent=2), flush=True)
        print("PASS actual TaskTool native-owned lifecycle CLI acceptance", flush=True)
    except BaseException as error:
        save_json(artifact / "failure.json", {"type": type(error).__name__, "error": repr(error), "traceback": traceback.format_exc()})
        raise
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)
        save_json(artifact / "provider-state.json", state)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    run(parser.parse_args().binary)
