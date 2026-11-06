"""Exercise the smoke harness's real owned Windows Jobs using only synthetic Python children.

Run: python -I scripts/test_memory_smoke_support.py
No Core/provider installation or configuration is needed. Deliberate timeout/exit/output failures
are restricted to processes this fixture creates suspended and assigns to its own Job.
"""
from __future__ import annotations

import ctypes
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("owned_memory_smoke", Path(__file__).with_name("memory_agent_smoke.py"))
support = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(support)


@unittest.skipUnless(sys.platform == "win32" and ctypes.sizeof(ctypes.c_void_p) == 8, "Windows x86_64 Job fixtures")
class OwnedJobTests(unittest.TestCase):
    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="aizen-owned-job-test-")
        self.addCleanup(self.fixture.cleanup)
        self.root = Path(self.fixture.name)
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        self.env = support.isolated_environment(self.root)
        self.runner = support.OwnedProcessRunner(self.root, self.workspace, self.env, 10)
        self.python = str(Path(sys.executable).resolve())

    def run_python(self, name, script):
        return self.runner.run(name, [self.python, "-I", "-c", script])

    def test_single_detached_process_has_no_console_helper(self):
        result = self.run_python("single", "print('owned standalone process')")
        self.assertEqual(result["exit"], 0)
        self.assertTrue(result["job_settled"])
        self.assertEqual(result["job_processes"], 1)
        self.assertIn("owned standalone process", result["stdout"])
        self.assertEqual(self.runner.cleanup_failures, [])

    def test_suspended_create_counts_and_settles_child_tree(self):
        script = "import subprocess,sys; subprocess.run([sys.executable,'-I','-c','print(\"owned grandchild\")'],check=True,creationflags=subprocess.DETACHED_PROCESS,stdin=subprocess.DEVNULL,stdout=sys.stdout,stderr=sys.stderr); print('owned parent')"
        result = self.run_python("tree", script)
        self.assertEqual(result["exit"], 0)
        self.assertTrue(result["job_settled"])
        self.assertEqual(result["job_processes"], 2)
        self.assertIn("owned grandchild", result["stdout"])
        self.assertIn("owned parent", result["stdout"])
        self.assertEqual(self.runner.cleanup_failures, [])

    def test_bounded_stdin_is_delivered_to_owned_child(self):
        result = self.runner.run("stdin", [self.python, "-I", "-c", "import sys; assert sys.stdin.buffer.read() == b'fixture input'; print('input received')"], b"fixture input")
        self.assertEqual(result["exit"], 0)
        self.assertTrue(result["job_settled"])
        self.assertIn("input received", result["stdout"])

    def test_nonzero_exit_is_reported_and_stderr_captured(self):
        result = self.run_python("nonzero", "import sys; print('fixture error',file=sys.stderr); sys.exit(7)")
        self.assertEqual(result["exit"], 7)
        self.assertTrue(result["job_settled"])
        self.assertIn("fixture error", result["stderr"])
        self.assertEqual(self.runner.cleanup_failures, [])

    def test_timeout_terminates_only_the_owned_parent_and_grandchild(self):
        self.runner.timeout = 1
        script = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-I','-c','import time; time.sleep(60)']); print('spawned owned child',flush=True); time.sleep(60)"
        with self.assertRaisesRegex(AssertionError, "own job exceeded"):
            self.run_python("timeout", script)
        self.assertIn("spawned owned child", (self.root / "timeout.stdout").read_text(encoding="utf-8"))
        self.assertEqual(self.runner.cleanup_failures, [], "owned tree must have bounded cleanup proof")

    def test_output_overflow_fails_and_still_settles_owned_job(self):
        script = "import sys,time; sys.stdout.write('x'*5000000); sys.stdout.flush(); time.sleep(60)"
        with self.assertRaisesRegex(AssertionError, "output exceeded"):
            self.run_python("overflow", script)
        self.assertEqual(self.runner.cleanup_failures, [])

    def test_environment_does_not_inherit_credentials_or_toolchain_homes(self):
        forbidden = {"CARGO_HOME", "RUSTUP_HOME", "PYTHONPATH", "HTTP_PROXY", "HTTPS_PROXY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"}
        self.assertFalse(forbidden.intersection(self.env))
        self.assertEqual(self.env["HOME"], str(self.root / "home"))
        self.assertEqual(self.env["AIZEN_HOME"], str(self.root / "home/.aizen"))
        result = self.run_python("environment", "import os; assert os.environ['AIZEN_HOME'].endswith('.aizen'); assert 'CARGO_HOME' not in os.environ; print('owned environment')")
        self.assertEqual(result["exit"], 0)
        self.assertIn("owned environment", result["stdout"])


if __name__ == "__main__":
    if sys.flags.isolated != 1:
        raise SystemExit("Run with Python -I")
    unittest.main()
