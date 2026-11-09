"""Tests for release.py. Synthetic repos and data only."""

import datetime
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import release  # noqa: E402


class Versions(unittest.TestCase):
    def test_next_version(self):
        d = datetime.date(2026, 11, 8)
        self.assertEqual(release.next_version(d, []), "v2026.11.0")
        self.assertEqual(release.next_version(d, ["v2026.11.0", "v2026.11.9"]), "v2026.11.10")
        self.assertEqual(release.next_version(d, ["v2026.10.4"]), "v2026.11.0")
        self.assertEqual(release.next_version(datetime.date(2027, 1, 2), ["v2026.12.3"]),
                         "v2027.01.0")


class Changelog(unittest.TestCase):
    TEXT = (release.HEADER + "\n## v2026.11.1 (2026-11-09)\n\nSummary.\n\n### Fixes\n\n- a\n"
            "\n## v2026.11.0 (2026-11-08)\n\nFirst.\n")

    def test_split(self):
        s = release.split_changelog(self.TEXT)
        self.assertEqual([v for v, _ in s], ["v2026.11.1", "v2026.11.0"])
        self.assertTrue(s[0][1].startswith("## v2026.11.1"))
        self.assertIn("- a", s[0][1])
        self.assertNotIn("v2026.11.0", s[0][1])


@unittest.skipUnless(shutil.which("git-cliff"), "git-cliff not installed")
class EndToEnd(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        t = self.tmp.name
        self.env = {**os.environ, "GIT_AUTHOR_NAME": "T", "GIT_AUTHOR_EMAIL": "t@example.com",
                    "GIT_COMMITTER_NAME": "T", "GIT_COMMITTER_EMAIL": "t@example.com",
                    "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.hooksPath",
                    "GIT_CONFIG_VALUE_0": "/dev/null", "CI": ""}
        self.env.pop("LEAKSCAN_DENYLIST_FILE", None)
        self.repo = os.path.join(t, "r")
        self.sh("git", "init", "-q", "--bare", "-b", "main", os.path.join(t, "o.git"), cwd=t)
        self.sh("git", "init", "-q", "-b", "main", self.repo, cwd=t)
        self.sh("git", "remote", "add", "origin", "../o.git")
        os.mkdir(os.path.join(self.repo, "scripts"))
        for f in ("release.py", "leakscan.py"):
            shutil.copy(os.path.join(HERE, f), os.path.join(self.repo, "scripts", f))
        shutil.copy(os.path.join(HERE, "..", "cliff.toml"), self.repo)
        self.sh("git", "add", "-A")
        self.sh("git", "commit", "-qm", "chore: scaffold")
        self.sh("git", "push", "-q", "origin", "main")

    def tearDown(self):
        self.tmp.cleanup()

    def sh(self, *args, cwd=None, ok=True):
        r = subprocess.run(args, cwd=cwd or self.repo, env=self.env,
                           capture_output=True, text=True)
        if ok and r.returncode != 0:
            self.fail(f"{args}: {r.stdout}{r.stderr}")
        return r

    def rel(self, *args, ok=True):
        return self.sh(sys.executable, "-I", "scripts/release.py", *args, ok=ok)

    def test_baseline_then_generated_release(self):
        v0 = release.next_version(datetime.date.today(), [])
        self.assertIn("First tagged release", self.rel("draft").stdout)
        self.rel("cut")
        self.assertEqual(self.sh("git", "tag").stdout.split(), [v0])
        self.assertEqual(self.sh("git", "log", "-1", "--format=%s").stdout.strip(),
                         f"release: {v0}")
        self.sh("git", "push", "-q", "--atomic", "origin", "main", v0)

        self.sh("git", "commit", "-q", "--allow-empty", "-m", "feat(core): a new thing",
                "-m", "Part of ho-zz1.2.")
        self.sh("git", "commit", "-q", "--allow-empty", "-m", "chore: noise")
        out = self.rel("draft").stdout
        v1 = v0[:-1] + "1"
        self.assertIn(f"## {v1}", out)
        self.assertIn("core: A new thing (ho-zz1.2)", out)
        self.assertNotIn("noise", out)
        self.rel("draft")  # re-drafting replaces, doesn't stack
        text = open(os.path.join(self.repo, "CHANGELOG.md")).read()
        self.assertEqual(text.count(f"## {v1}"), 1)
        self.assertEqual(text.count(f"## {v0}"), 1)
        self.rel("cut")
        notes = self.rel("notes", v1).stdout
        self.assertTrue(notes.startswith("### Features"), notes)

    def test_cut_refuses_leaky_notes_and_dirty_tree(self):
        self.rel("draft")
        with open(os.path.join(self.repo, "CHANGELOG.md"), "a") as f:
            f.write("\nFixed the label for card ••6047.\n")
        r = self.rel("cut", ok=False)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("leak scan", r.stderr)
        self.assertEqual(self.sh("git", "tag").stdout.strip(), "")
        open(os.path.join(self.repo, "stray.txt"), "w").close()
        self.assertIn("not clean", self.rel("cut", ok=False).stderr)

    def test_off_main_and_behind_refused(self):
        self.sh("git", "checkout", "-q", "-b", "feat/x")
        self.assertIn("from main", self.rel("draft", ok=False).stderr)


if __name__ == "__main__":
    unittest.main()
