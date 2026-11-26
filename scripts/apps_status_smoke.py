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
        assert empty == {
            "schema_version": 2, "configuration_scope": "home", "project_overrides_checked": False,
            "live_probe_performed": False, "live_app_state": "unknown", "connectors": [],
        }
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
        assert json.loads(run("fixture", "--json").stdout)["connectors"][0]["key"] == "fixture"
        assert "offline" in run().stdout
        run("missing", "--json", ok=False)

        config.write_text('{"mcpServers":{"fixture":{"enabled":"fixture-secret"}}}', encoding="utf-8")
        run("--json", ok=False)
        config.write_bytes(b" " * (1024 * 1024 + 1))
        run("--json", ok=False)
        config.write_text(json.dumps({"mcpServers": {"fixture": {"command": "never-execute"}}}), encoding="utf-8")
        journal = home / "app-jobs"
        journal.mkdir()
        (journal / "operations.json").write_text("{broken-fixture-secret", encoding="utf-8")
        assert json.loads(run("--json").stdout) == {
            **empty,
            "connectors": [{"key": "fixture", "enabled": True, "transport": "stdio"}],
        }

    print(json.dumps({
        "ok": True,
        "scope": "actual CLI with isolated MCP configuration",
        "checked": ["absent_home_no_writes", "redacted_json", "unknown_live_state",
                    "home_only", "key_filter", "text_output", "missing_key_failure",
                    "malformed_config_no_secret_echo", "bounded_config_read",
                    "legacy_journal_ignored"],
    }, indent=2))


if __name__ == "__main__":
    main()
