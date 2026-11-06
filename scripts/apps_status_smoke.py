"""Offline Apps CLI smoke. Run with Python -I; uses only fixture-owned HOME/project paths."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def fingerprint(root):
    return {
        str(path.relative_to(root)): ("directory" if path.is_dir() else hashlib.sha256(path.read_bytes()).hexdigest())
        for path in root.rglob("*")
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    binary = parser.parse_args().binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="aizen-apps-status-") as directory:
        root = Path(directory)
        home, project = root / "home", root / "project"
        project.mkdir()
        environment = dict(os.environ, AIZEN_HOME=str(home), AIZEN_PROJECT_ROOT=str(project))

        def run(*args, ok=True):
            before = fingerprint(root)
            result = subprocess.run(
                [str(binary), "apps", "status", *args], cwd=project, env=environment,
                capture_output=True, text=True, timeout=15,
            )
            assert (result.returncode == 0) == ok, (result.returncode, "unexpected exit status")
            assert fingerprint(root) == before, "offline status changed fixture files"
            assert "fixture-secret" not in result.stdout + result.stderr, "secret reached output"
            return result

        empty = json.loads(run("--json").stdout)
        assert empty["schema_version"] == 1
        assert empty["configuration_scope"] == "home"
        assert empty["connectors"] == [] and empty["jobs"] == []
        assert empty["creative_dispatch_enabled"] is False
        assert empty["live_probe_performed"] is False
        assert empty["live_app_state"] == "unknown"
        assert not home.exists(), "status initialized absent HOME"

        home.mkdir()
        config = home / "mcp.json"
        config.write_text(json.dumps({"mcpServers": {
            "fixture": {"command": "never-execute-fixture-secret", "args": ["fixture-secret-args"],
                        "env": {"TOKEN": "fixture-secret-env"}},
            "remote": {"url": "https://example.invalid/fixture-secret-path",
                       "headers": {"Authorization": "fixture-secret-header"}, "enabled": False},
        }}), encoding="utf-8")
        (project / ".aizen").mkdir()
        (project / ".aizen" / "mcp.json").write_text("not valid JSON", encoding="utf-8")
        rows = json.loads(run("--json").stdout)
        assert len(rows["connectors"]) == 2
        assert rows["project_overrides_checked"] is False
        assert rows["job_control_authority"] is False
        assert rows["job_state_observation"] == "last_recorded_not_live"
        assert json.loads(run("fixture", "--json").stdout)["connectors"][0]["key"] == "fixture"
        assert "offline" in run().stdout
        run("missing", "--json", ok=False)

        config.write_text('{"mcpServers":{"fixture":{"enabled":"fixture-secret"}}}', encoding="utf-8")
        run("--json", ok=False)
        config.write_bytes(b" " * (1024 * 1024 + 1))
        run("--json", ok=False)
        config.write_text("{}", encoding="utf-8")

        config.write_text(json.dumps({"mcpServers":{"fixture":{"command":"never-execute"}}}), encoding="utf-8")
        journal = home / "app-jobs"
        journal.mkdir()
        (journal / "operations.lock").write_bytes(b"")
        run("--json", ok=False)  # initialization marker without journal is not an empty history
        (journal / "operations.json").write_text(json.dumps({"version": 1, "records": [{
            "version": 1, "operation_id": "a" * 64, "request_digest": "b" * 64,
            "target": {"adapter_id": "fixture", "adapter_version": "1.0", "app": "blender",
                       "app_version": "4.2", "instance_epoch": "fixture-epoch",
                       "project_root": str(project.resolve()), "document_id": "Private scene",
                       "revision": "fixture-revision", "schema_digest": "c" * 64},
            "canonical_project_root": str(project.resolve()), "owner_scope": "fixture_scope",
            "execution": "unknown", "effects": "unknown", "sequence": 2,
        }]}), encoding="utf-8")
        recorded = json.loads(run("fixture", "--json").stdout)
        assert recorded["jobs"] == [{"operation_id": "a" * 64, "execution": "unknown",
                                      "effects": "unknown", "sequence": 2}]
        assert recorded["job_scope"] == "all_home_operations"
        assert "Private scene" not in json.dumps(recorded)
        assert "fixture_scope" not in json.dumps(recorded)
        (journal / "operations.json").write_text("{broken", encoding="utf-8")
        run("--json", ok=False)
        (journal / "operations.json").write_text('{"version":"fixture-secret","records":[]}', encoding="utf-8")
        run("--json", ok=False)
        (journal / "operations.json").write_bytes(b" " * (1024 * 1024 + 1))
        run("--json", ok=False)

    print(json.dumps({
        "ok": True,
        "scope": "actual CLI; isolated synthetic configuration/journal only, no live adapters",
        "checked": ["absent_home_no_writes", "redacted_json", "unknown_live_state",
                    "home_only", "key_filter", "text_output", "missing_key_failure",
                    "malformed_config_no_secret_echo", "bounded_config_read",
                    "root_free_recorded_job_projection", "journal_error_no_secret_echo",
                    "missing_corrupt_oversize_journal_fail_closed"],
    }, indent=2))


if __name__ == "__main__":
    main()
