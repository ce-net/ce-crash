"""Tests for ce_crash (stdlib only). Run: python3 clients/py/test_ce_crash.py

Covers capsule construction and the fail-open fallback: subprocesses crash with
CE_NODE_URL pointing at a closed port and HOME redirected to a temp dir, so the capsule
must land as ~/.local/share/ce/ce-crash/<app>-<ts>.json with the right shape -- and env
var VALUES must never appear in the capsule (names only).
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent


def run_crasher(code, home, extra_env=None):
    env = dict(os.environ)
    env.update({
        "HOME": str(home),
        "CE_NODE_URL": "http://127.0.0.1:9",  # closed port: mesh is "down"
        "CE_API_TOKEN": "",
        "PYTHONPATH": str(HERE),
    })
    env.update(extra_env or {})
    return subprocess.run(
        [sys.executable, "-c", code], env=env, capture_output=True, text=True, timeout=60
    )


def capsules_in(home):
    d = pathlib.Path(home) / ".local/share/ce/ce-crash"
    return sorted(d.glob("*.json")) if d.is_dir() else []


class TestFallbackCapsule(unittest.TestCase):
    def test_unhandled_exception_writes_capsule(self):
        with tempfile.TemporaryDirectory() as home:
            r = run_crasher(
                "import ce_crash; ce_crash.install('t-app'); "
                "raise ValueError('kaboom 42')",
                home,
                extra_env={"CE_CRASH_TEST_SECRET": "hunter2-do-not-leak"},
            )
            self.assertEqual(r.returncode, 1)
            self.assertIn("ValueError: kaboom 42", r.stderr)  # traceback still printed
            self.assertIn("capsule written to", r.stderr)     # fail-open said so
            files = capsules_in(home)
            self.assertEqual(len(files), 1, r.stderr)
            self.assertTrue(files[0].name.startswith("t-app-"))
            text = files[0].read_text()
            c = json.loads(text)
            self.assertEqual(c["app"], "t-app")
            self.assertEqual(c["exit_kind"], "exception")
            self.assertEqual(c["exc_type"], "ValueError")
            self.assertEqual(c["msg"], "ValueError: kaboom 42")
            self.assertIn("kaboom 42", c["stack"])
            self.assertIsInstance(c["ts_ms"], int)
            self.assertIsInstance(c["argv"], list)
            self.assertTrue(c["python"])
            self.assertTrue(c["platform"])
            self.assertTrue(c["cwd"])
            self.assertEqual(c["recent"], [])
            # env: names only, never values
            self.assertIn("CE_CRASH_TEST_SECRET", c["env_keys"])
            self.assertNotIn("hunter2-do-not-leak", text)

    def test_nonzero_sys_exit_writes_capsule(self):
        with tempfile.TemporaryDirectory() as home:
            r = run_crasher(
                "import sys, ce_crash; ce_crash.install('t-exit'); sys.exit(3)", home
            )
            self.assertEqual(r.returncode, 3)
            files = capsules_in(home)
            self.assertEqual(len(files), 1, r.stderr)
            c = json.loads(files[0].read_text())
            self.assertEqual(c["exit_kind"], "exit")
            self.assertEqual(c["msg"], "exit code 3")
            self.assertIsNone(c["exc_type"])

    def test_clean_exit_writes_nothing(self):
        with tempfile.TemporaryDirectory() as home:
            r = run_crasher(
                "import ce_crash; ce_crash.install('t-ok'); print('fine')", home
            )
            self.assertEqual(r.returncode, 0)
            self.assertEqual(capsules_in(home), [])
            self.assertNotIn("ce-crash", r.stderr)


class TestCapsuleBuild(unittest.TestCase):
    def test_recent_tail_from_debug_buffer(self):
        sys.path.insert(0, str(HERE))
        import ce_crash

        class FakeDbg:
            buf = [{"msg": "e{}".format(i)} for i in range(60)]

        ce_crash._state.update(app="t-recent", debug=FakeDbg())
        c = ce_crash._build_capsule("exception", "X", "m", None)
        self.assertEqual(len(c["recent"]), 50)
        self.assertEqual(c["recent"][-1]["msg"], "e59")
        self.assertEqual(c["recent"][0]["msg"], "e10")


if __name__ == "__main__":
    unittest.main(verbosity=2)
