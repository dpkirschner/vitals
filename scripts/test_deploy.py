"""Tests for deploy.sh's env and try --clean. Synthetic plists only.

They run deploy.sh against a throwaway plist in a temp dir with SUDO=''
and LAUNCHCTL=true, so nothing live is read or changed. macOS only
(PlistBuddy, plutil).
"""

import os
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest

APP = "vitals"  # the only line that differs between the outflow and vitals copies
PREFIX = APP.upper()
HERE = os.path.dirname(os.path.abspath(__file__))
DEPLOY = os.path.join(HERE, "deploy.sh")


def read(path):
    with open(path, "rb") as f:
        return f.read()


@unittest.skipUnless(sys.platform == "darwin" and os.path.exists("/usr/libexec/PlistBuddy"),
                     "needs macOS PlistBuddy")
class DeployEnv(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.data = os.path.join(self.tmp, "data dir")  # a space, like Application Support
        os.makedirs(self.data)
        self.plist = os.path.join(self.tmp, f"com.{APP}.test.plist")
        self.write_plist({f"{PREFIX}_DATA_DIR": self.data, f"{PREFIX}_LISTEN": "127.0.0.1:1"})
        self.build = os.path.join(self.tmp, "build")

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def write_plist(self, env):
        with open(self.plist, "wb") as f:
            plistlib.dump({"Label": f"com.{APP}.test",
                           "ProgramArguments": ["/nonexistent/server"],
                           "EnvironmentVariables": env}, f)

    def env_vars(self):
        with open(self.plist, "rb") as f:
            return plistlib.load(f)["EnvironmentVariables"]

    def run_deploy(self, *args):
        env = dict(os.environ, SUDO="", LAUNCHCTL="true", **{
            f"{PREFIX}_PLIST": self.plist, f"{PREFIX}_BUILD_DIR": self.build})
        return subprocess.run(["bash", DEPLOY, *args], env=env, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True)

    def backups(self):
        d = os.path.join(self.data, "plist-backups")
        return sorted(os.listdir(d)) if os.path.isdir(d) else []

    def test_set_adds_and_replaces_with_backup(self):
        before = read(self.plist)
        r = self.run_deploy("env", "set", "--yes", f"{PREFIX}_PERKS=/a path/perks.json",
                            f"{PREFIX}_LISTEN=127.0.0.1:2")
        self.assertEqual(r.returncode, 0, r.stderr)
        v = self.env_vars()
        self.assertEqual(v[f"{PREFIX}_PERKS"], "/a path/perks.json")
        self.assertEqual(v[f"{PREFIX}_LISTEN"], "127.0.0.1:2")
        self.assertIn("+", r.stdout)  # the diff was shown
        [b] = self.backups()
        self.assertEqual(read(os.path.join(self.data, "plist-backups", b)), before)

    def test_unset_and_restore(self):
        r = self.run_deploy("env", "unset", "--yes", f"{PREFIX}_LISTEN")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertNotIn(f"{PREFIX}_LISTEN", self.env_vars())
        r = self.run_deploy("env", "restore", "--yes")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.env_vars()[f"{PREFIX}_LISTEN"], "127.0.0.1:1")

    def test_unset_missing_is_no_change(self):
        r = self.run_deploy("env", "unset", "--yes", "NOT_THERE")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("isn't set", r.stdout)
        self.assertIn("no change", r.stdout)
        self.assertEqual(self.backups(), [])

    def assert_refused(self, *args, says):
        before = read(self.plist)
        r = self.run_deploy("env", *args)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn(says, r.stderr)
        self.assertEqual(read(self.plist), before)
        self.assertEqual(self.backups(), [])

    def test_refuses_secrets_bad_names_and_values(self):
        self.assert_refused("set", "--yes", f"{PREFIX}_API_TOKEN=abc", says="_FILE")
        self.assert_refused("set", "--yes", "OURA_CLIENT_SECRET=abc", says="_FILE")
        self.assert_refused("set", "--yes", "lower=1", says="isn't an env var name")
        self.assert_refused("set", "--yes", 'X="q"', says="quotes")
        self.assert_refused("set", "--yes", "X=", says="empty value")
        self.assert_refused("set", "--yes", "NOEQUALS", says="KEY=VALUE")

    def test_needs_a_terminal_without_yes(self):
        self.assert_refused("set", "X=1", says="terminal")

    def test_list(self):
        r = self.run_deploy("env", "list")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(f"{PREFIX}_LISTEN=127.0.0.1:1", r.stdout.splitlines())

    def test_verify_needs_releases_layout(self):
        r = self.run_deploy("verify")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("releases/current", r.stderr)

    def test_usage(self):
        self.assertEqual(self.run_deploy().returncode, 2)
        self.assertEqual(self.run_deploy("try").returncode, 2)
        self.assertEqual(self.run_deploy("verify", "--bogus").returncode, 2)

    def test_try_clean_leaves_unrelated_processes(self):
        os.makedirs(os.path.join(self.build, "try-data"))
        other = subprocess.Popen(["sleep", "30"])
        try:
            with open(os.path.join(self.build, "try.pid"), "w") as f:
                f.write(str(other.pid))
            r = self.run_deploy("try", "--clean")
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertFalse(os.path.exists(os.path.join(self.build, "try-data")))
            self.assertFalse(os.path.exists(os.path.join(self.build, "try.pid")))
            self.assertIsNone(other.poll())  # not ours, so not killed
        finally:
            other.kill()
            other.wait()


if __name__ == "__main__":
    unittest.main()
