"""Tests for leakscan.py. Every value here is invented (synthetic)."""

import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import leakscan  # noqa: E402

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "leakscan.py")


def rules_hit(kind, text):
    return sorted({r.name for r in leakscan.SHAPE_RULES if r.hits(kind, text)})


class ShapeRules(unittest.TestCase):
    def test_card_masks_block(self):
        for text in [
            "paid with x7731",
            "card ****5820",
            "shown as ••6047",
            "Visa …2291",
            "Amex ending 3318",
            'mask: {"mask": "7062"}',
            "Travel Card (5194)",
        ]:
            self.assertEqual(rules_hit("content", text), ["card-mask"], text)

    def test_not_card_masks(self):
        for text in [
            "0x1f40 and 1920x1080",
            "loosened (0644) file",
            "Last month (1800) vs prior (1233)",
            "Report (2026)",
            '"mask": "0000"',
            "Amex ending 4242",
            "Card (1234)",
        ]:
            self.assertEqual(rules_hit("content", text), [], text)

    def test_email(self):
        self.assertEqual(rules_hit("content", "mail jane.roe@mailhost.test.io"), ["email"])
        for ok in [
            "9169715+someone@users.noreply.github.com",
            "you@example.com",
            "Co-Authored-By: Claude <noreply@anthropic.com>",
            "git@github.com:owner/repo",
            "icon-128x128@2x.png",
        ]:
            self.assertEqual(rules_hit("content", ok), [], ok)

    def test_home_path(self):
        self.assertEqual(rules_hit("content", "at /Users/janeroe/code"), ["home-path"])
        self.assertEqual(rules_hit("content", "-Users-janeroe-code-repo"), ["home-path"])
        for ok in ["/Users/YOU/outflow.db", "/Users/Shared/bin/bd", "/Users/$USER"]:
            self.assertEqual(rules_hit("content", ok), [], ok)

    def test_tailnet(self):
        self.assertEqual(rules_hit("content", "https://box.tail9f3e.ts.net/x"), ["tailnet-host"])
        for ok in [
            "https://<mini>.<tailnet>.ts.net/api",
            "MINI.TAILNET.ts.net",
            "mini.tailnet.ts.net",
        ]:
            self.assertEqual(rules_hit("content", ok), [], ok)

    def test_dollars_warn_only_in_messages(self):
        self.assertEqual(rules_hit("message", "under $20 says dollars"), ["dollar-figure"])
        self.assertEqual(rules_hit("content", "under $20 says dollars"), [])
        self.assertEqual(rules_hit("message", "the $HOME path, a bare $"), [])
        self.assertEqual(rules_hit("message", "about $20k"), ["dollar-figure"])


class HealthRules(unittest.TestCase):
    def test_device_owner_blocks(self):
        for text in [
            'sourceName="Robin’s Apple Watch"',
            "name:Robin's iPhone, manufacturer:Apple Inc.",
            'sourceName="Robin&apos;s iPad"',
            "Kai-Lee’s MacBook",
        ]:
            self.assertEqual(rules_hit("content", text), ["device-owner"], text)

    def test_device_owner_placeholders(self):
        for text in [
            'sourceName="Test Watch"',
            'sourceName="Test’s Apple Watch"',
            "the user's iPhone",
            "Your’s iPhone",
            "Apple Watch and iPhone",
        ]:
            self.assertEqual(rules_hit("content", text), [], text)

    def test_precise_coord_blocks(self):
        for text in [
            '<trkpt lon="-122.31847" lat="47.60152">',
            '{"latitude": 47.612345}',
            "assert_eq!(pts[0].lat, 47.61003);",
        ]:
            self.assertEqual(rules_hit("content", text), ["precise-coord"], text)

    def test_round_coords_pass(self):
        for text in [
            '<trkpt lon="-122.2980" lat="47.6005">',
            "assert_eq!(pts[0].lat, 47.6000);",
            '"lat": 47.600000',
            "latency 12.34567 ms",
        ]:
            self.assertEqual(rules_hit("content", text), [], text)

    def test_me_record(self):
        self.assertEqual(
            rules_hit("content", '<Me HKCharacteristicTypeIdentifierDateOfBirth="1987-06-14"'),
            ["me-record"],
        )
        self.assertEqual(
            rules_hit("content", 'HKCharacteristicTypeIdentifierBloodType="HKBloodTypeONegative"'),
            ["me-record"],
        )
        for ok in [
            'HKCharacteristicTypeIdentifierDateOfBirth="2000-01-01"',
            'HKCharacteristicTypeIdentifierBiologicalSex="HKBiologicalSexNotSet"',
            'HKCharacteristicTypeIdentifierBloodType=""',
        ]:
            self.assertEqual(rules_hit("content", ok), [], ok)

    def test_synthetic_label(self):
        s = leakscan.Scanner([], [], redact=False)
        s.check_label("examples/a.xml", ["<?xml?>", "<!-- Synthetic data -->"], "w")
        s.check_label("src/a.rs", ["fn main() {}"], "w")
        self.assertEqual(s.hits, [])
        s.check_label("examples/b.xml", ["<?xml?>", "<HealthData>"], "w")
        self.assertEqual([h[1] for h in s.hits], ["synthetic-label"])
        s = leakscan.Scanner([], [("synthetic-label", "examples/b.xml")], redact=False)
        s.check_label("examples/b.xml", ["<HealthData>"], "w")
        self.assertEqual(s.hits, [])


class Scanner(unittest.TestCase):
    def test_allowlist_by_path_and_commit(self):
        s = leakscan.Scanner(
            leakscan.SHAPE_RULES,
            [("card-mask", "examples/*"), ("*", "commit:abc123")],
            redact=False,
        )
        self.assertFalse(s.check("content", "x7731", "w", None, "examples/a.json"))
        self.assertFalse(s.check("content", "x7731", "w", "abc1234def", "src/a.rs"))
        self.assertTrue(s.check("content", "x7731", "w", "fff", "src/a.rs"))

    def test_denylist_is_never_allowlisted(self):
        rule = leakscan.Rule("denylist:1", "block", leakscan.re.compile("zorblax", 2))
        s = leakscan.Scanner([rule], [("*", "*")], redact=False)
        self.assertTrue(s.check("content", "a ZORBLAX b", "w", None, "x"))

    def test_redacted_path_is_not_printed(self):
        s = leakscan.Scanner(leakscan.SHAPE_RULES, [], redact=True)
        self.assertEqual(s.label_path("notes/card-x7731.txt"), "<redacted path>")
        self.assertEqual(s.label_path("notes/plain.txt"), "notes/plain.txt")


class EndToEnd(unittest.TestCase):
    """Runs the script on a throwaway repo."""

    def run_scan(self, repo, *args, env=None):
        e = {**os.environ, "CI": "", **(env or {})}
        e.pop("LEAKSCAN_DENYLIST_FILE", None)
        return subprocess.run(
            [sys.executable, "-I", SCRIPT, *args],
            cwd=repo, capture_output=True, text=True, env=e,
        )

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = self.tmp.name
        g = ["git", "-C", self.repo, "-c", "user.name=T", "-c", "user.email=t@example.com",
             "-c", "core.hooksPath=/dev/null"]
        self.git = lambda *a: subprocess.run([*g, *a], check=True, capture_output=True)
        self.git("init", "-q", "-b", "main")
        with open(os.path.join(self.repo, "a.txt"), "w") as f:
            f.write("clean line\nhello zorblax\n")
        self.git("add", "a.txt")
        self.git("commit", "-q", "-m", "first\n\nraised the cap to $40")

    def tearDown(self):
        self.tmp.cleanup()

    def test_history_with_denylist_redacted(self):
        r = self.run_scan(self.repo, "--all", "--redact",
                          env={"LEAKSCAN_DENYLIST": "# terms\nzorblax\n"})
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("denylist:2", r.stdout)
        self.assertIn("a.txt:2", r.stdout)
        self.assertNotIn("zorblax", r.stdout.lower())
        self.assertNotIn("$40", r.stdout)

    def test_warnings_pass_unless_asked(self):
        self.assertEqual(self.run_scan(self.repo, "--all").returncode, 0)
        self.assertEqual(self.run_scan(self.repo, "--all", "--fail-on-warn").returncode, 3)

    def test_range_and_file(self):
        r = self.run_scan(self.repo, "--range", "--", "HEAD", "--not", "HEAD")
        self.assertIn("0 new commits", r.stdout)
        notes = os.path.join(self.repo, "notes.md")
        with open(notes, "w") as f:
            f.write("Fixed the card ••6047 label\n")
        self.assertEqual(self.run_scan(self.repo, "--file", notes).returncode, 1)

    def test_unlabelled_fixture_warns_in_history(self):
        os.makedirs(os.path.join(self.repo, "examples"))
        with open(os.path.join(self.repo, "examples", "real.xml"), "w") as f:
            f.write("<HealthData>\n</HealthData>\n")
        with open(os.path.join(self.repo, "examples", "fake.xml"), "w") as f:
            f.write("<!-- synthetic -->\n<HealthData/>\n")
        self.git("add", "examples")
        self.git("commit", "-q", "-m", "samples")
        r = self.run_scan(self.repo, "--all")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("synthetic-label", r.stdout)
        self.assertIn("examples/real.xml", r.stdout)
        self.assertNotIn("examples/fake.xml", r.stdout)
        r = self.run_scan(self.repo, "--tree", "--fail-on-warn")
        self.assertEqual(r.returncode, 3, r.stdout + r.stderr)

    def test_bad_denylist_line_hides_the_term(self):
        r = self.run_scan(self.repo, "--all", env={"LEAKSCAN_DENYLIST": "ok\nsecret(term\n"})
        self.assertEqual(r.returncode, 2)
        self.assertIn("line 2", r.stderr)
        self.assertNotIn("secret", r.stderr)


if __name__ == "__main__":
    unittest.main()
