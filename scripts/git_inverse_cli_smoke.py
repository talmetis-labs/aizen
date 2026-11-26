"""Drive selected-path undo through the production CLI in a disposable repository/home.

Run with Python -I and --binary target/debug/aizen.exe. No model/provider or installed binary.
All fixture and command evidence is retained on success and failure.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def require(value, message):
    if not value:
        raise AssertionError(message)


def run(binary):
    binary = binary.resolve(strict=True)
    artifact = Path(tempfile.mkdtemp(prefix="aizen-inverse-cli-"))
    print(f"ARTIFACTS {artifact}", flush=True)
    root, home, logs = (artifact / name for name in ("repo", "home", "logs"))
    for directory in (root, home, logs):
        directory.mkdir()
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith(("AIZEN_", "OPENAI_", "ANTHROPIC_", "GIT_"))}
    empty = artifact / "empty-git-config"
    empty.write_bytes(b"")
    env.update({"AIZEN_HOME": str(home), "GIT_CONFIG_GLOBAL": str(empty), "GIT_CONFIG_NOSYSTEM": "1"})
    git_exe = shutil.which("git")
    require(git_exe, "Git required for CLI acceptance")

    def git(*args):
        result = subprocess.run([git_exe, *args], cwd=root, env=env, capture_output=True, timeout=30)
        require(result.returncode == 0, (args, result.returncode, result.stderr.decode(errors="replace")))
        return result.stdout.decode("utf-8").strip()

    def cli(label, args, success=True):
        print(f"START {label}", flush=True)
        result = subprocess.run([str(binary), "--sandbox", "guarded", *args], cwd=root, env=env,
                                input=b"", capture_output=True, timeout=180)
        (logs / f"{label}.stdout").write_bytes(result.stdout)
        (logs / f"{label}.stderr").write_bytes(result.stderr)
        print(f"END {label} exit={result.returncode}", flush=True)
        require((result.returncode == 0) == success,
                (label, result.returncode, result.stderr.decode(errors="replace")[-6000:]))
        return result

    def commit():
        git("add", "--all")
        git("commit", "-qm", "fixture change")
        return git("rev-parse", "HEAD")

    git("init", "-q")
    for key, value in (("user.name", "Fixture"), ("user.email", "fixture@example.invalid"),
                       ("commit.gpgsign", "false"), ("core.autocrlf", "false")):
        git("config", key, value)
    (root / ".aizen").mkdir()
    (root / ".aizen/verify.json").write_text(json.dumps({"commands": ["git diff --exit-code"], "timeout_secs": 30}), encoding="utf-8")
    original = b"old\n1\n2\n3\n4\n5\n6\n7\n8\nend\n"
    selected_bytes = original.replace(b"old", b"new")
    (root / "selected with space.txt").write_bytes(original)
    (root / "other.txt").write_bytes(b"base\n")
    commit()
    (root / "selected with space.txt").write_bytes(selected_bytes)
    (root / "other.txt").write_bytes(b"keep selected other\n")
    selected = commit()
    later = selected_bytes.replace(b"end", b"later")
    (root / "selected with space.txt").write_bytes(later)
    base = commit()
    cli("trust", ["mcp", "trust"])
    (root / "selected with space.txt").write_bytes(b"staged user\n")
    git("add", "--", "selected with space.txt")
    (root / "selected with space.txt").write_bytes(b"unstaged user\n")
    (root / "pending.tmp").write_bytes(b"untracked user\n")
    index = (root / ".git/index").read_bytes()
    inventory = git("worktree", "list", "--porcelain", "-z")
    args = ["time", "undo-paths", selected, "--path", "selected with space.txt"]
    cli("inverse-preview", args)
    require(git("worktree", "list", "--porcelain", "-z") == inventory, "preview allocated checkout")
    cli("inverse-execute", [*args, "--yes"])
    require(git("rev-parse", "HEAD") == base and (root / ".git/index").read_bytes() == index, "inverse moved parent HEAD/index")
    require((root / "selected with space.txt").read_bytes() == b"unstaged user\n", "inverse changed pending parent bytes")
    require((root / "pending.tmp").read_bytes() == b"untracked user\n", "inverse deleted untracked bytes")
    manifests = [json.loads(path.read_text(encoding="utf-8")) for path in home.glob("coop/*/sessions/*.json")]
    candidates = [m for m in manifests if m.get("git_task", {}).get("phase") == "handed_off"]
    require(len(candidates) == 1, candidates)
    manifest = candidates[0]
    child = Path(manifest["root"])
    inverse = manifest["git_task"]
    require(inverse["base"] == base and inverse["inverse"]["commit"] == selected, inverse)
    require((child / "selected with space.txt").read_bytes() == original.replace(b"end", b"later"), "lost later independent content")
    require((child / "other.txt").read_bytes() == b"keep selected other\n", "inverse included unselected file")
    registration = manifest["session_id"]
    refused = cli("dirty-integration-refused", ["time", "integrate-task", registration, "--yes"], success=False)
    require(b"worktree/index must be clean" in refused.stderr, refused.stderr.decode(errors="replace"))
    require((root / ".git/index").read_bytes() == index, "dirty refusal changed index")
    # Remove ONLY this fixture's own pending state, never the developer checkout's files/index.
    git("restore", "--staged", "--", "selected with space.txt")
    (root / "selected with space.txt").write_bytes(later)
    (root / "pending.tmp").unlink()
    cli("integration-preview", ["time", "integrate-task", registration])
    require(git("rev-parse", "HEAD") == base, "integration preview moved HEAD")
    cli("integrate", ["time", "integrate-task", registration, "--yes"])
    receipts = [json.loads(path.read_text(encoding="utf-8")) for path in home.glob("git-operations-v1/*/*/*.json")]
    require(len(receipts) == 1, receipts)
    receipt = receipts[0]
    require(receipt["handoff"]["inverse"] == inverse["inverse"] and not receipt["verified"], receipt)
    cli("verify", ["time", "verify-task", receipt["id"], "--yes"])
    verified_paths = list(home.glob("verification-v1/*/*/*.json"))
    verified = [json.loads(path.read_text(encoding="utf-8")) for path in verified_paths]
    require(len(verified) == 1 and verified[0]["verified"], verified)
    # Reproduce ONLY the fixture metadata state after a crash between durable pass and link.
    receipt_path = next(home.glob("git-operations-v1/*/*/*.json"))
    receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
    pass_bytes = verified_paths[0].read_bytes()
    linked_head, linked_index = git("rev-parse", "HEAD"), (root / ".git/index").read_bytes()
    preview = cli("relink-preview", ["time", "verify-task", receipt["id"]])
    require(b'"mode": "relink_durable_pass"' in preview.stdout, preview.stdout.decode(errors="replace"))
    require(not json.loads(receipt_path.read_text(encoding="utf-8"))["verified"], "preview published verification link")
    cli("relink-execute", ["time", "verify-task", receipt["id"], "--yes"])
    recovered = json.loads(receipt_path.read_text(encoding="utf-8"))
    require(recovered["verified"] and recovered["verification"] == verified[0]["id"], recovered)
    require(verified_paths[0].read_bytes() == pass_bytes and len(list(home.glob("verification-v1/*/*/*.json"))) == 1,
            "relink rewrote evidence or created another attempt")
    require(git("rev-parse", "HEAD") == linked_head and (root / ".git/index").read_bytes() == linked_index,
            "relink changed destination HEAD/index")
    require(git("rev-parse", "HEAD") == inverse["tip"], "integration did not reach inverse tip")
    cli("redo-explicit-inverse", ["time", "undo", inverse["tip"], "--yes"])
    require((root / "selected with space.txt").read_bytes() == later, "redo lost later content")
    require((root / "other.txt").read_bytes() == b"keep selected other\n", "redo changed unselected path")
    require(not git("status", "--porcelain"), "fixture destination is not clean")
    summary = {"base": base, "selected": selected, "inverse": inverse["tip"], "registration": registration,
               "integration": receipt["id"], "verification": verified[0]["id"],
               "checks": "preview/dirty-parent/owned-inverse/separate-integration/destination-verification/explicit-redo passed"}
    (artifact / "result.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print("PASS real CLI selected-path inverse and explicit integration/verification/redo", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    run(parser.parse_args().binary)
