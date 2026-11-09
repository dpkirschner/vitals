#!/usr/bin/env python3
"""leakscan: find personal data before it goes public.

Two kinds of rule:

  Shape rules (SHAPE_RULES below, public) match what a leak looks like:
  card masks, personal emails, home paths, tailnet hostnames, and for
  health data: device names that carry an owner's name, street-precise
  coordinates, and Me-record characteristics (birth date, sex, ...).
  They block. Dollar figures in commit messages, tag messages and
  release notes only warn, because they are usually thresholds or
  examples, and so does a sample file under examples/ that doesn't say
  "synthetic" near its top (a real capture copied in by mistake).

  Shared with outflow's copy except the health rules and FIXTURE_GLOBS;
  port fixes both ways.

  The denylist (optional, private) blocks known terms. One extended
  regex per line, case-insensitive, `#` comments. It comes from
  --denylist FILE, $LEAKSCAN_DENYLIST_FILE, or $LEAKSCAN_DENYLIST
  (the text itself, for a CI secret).

What to scan (pick one):

  --all            every commit reachable from any ref, plus ref names
                   and tag messages
  --range -- REV…  commits listed by `git log REV…`,
                   e.g. `--range -- HEAD --not --remotes=origin`
  --tree           tracked files as they are now
  --file PATH      one file, read as prose (release notes)

  --ref NAME       also check a ref name and, for an annotated tag, its
                   message (repeatable; the pre-push hook passes these)

Exceptions to shape rules live in .leakscan-allow at the repo root. The
denylist has none: bypass a false hit once with `git push --no-verify`.

--redact (the default when $CI is set) never prints the matched text, so
a public CI log can't repeat the leak. Exit 0 = nothing blocking,
1 = blocking hits, 3 = warnings only with --fail-on-warn, 2 = error.
Standard library only; runs on Python 3.9+.
"""

import fnmatch
import os
import re
import subprocess
import sys

USAGE = (
    "usage: leakscan.py (--all | --tree | --file PATH | --range -- REV...)\n"
    "                   [--ref NAME]... [--denylist FILE] [--redact]\n"
    "                   [--fail-on-warn]"
)

# Kinds of text a hit can come from. Dollar figures only matter in prose.
# AUTHOR is the email only: the author name is public by design.
CONTENT, PATH, MESSAGE, AUTHOR, REF = "content", "path", "message", "author", "ref"


class Rule:
    def __init__(self, name, severity, pattern, allow=None, kinds=None):
        self.name = name
        self.severity = severity  # "block" or "warn"
        self.pattern = pattern
        self.allow = allow  # applied to the matched text
        self.kinds = kinds  # None = every kind

    def hits(self, kind, text):
        if self.kinds is not None and kind not in self.kinds:
            return []
        found = []
        for m in self.pattern.finditer(text):
            if self.allow is None or not self.allow.search(m.group(0)):
                found.append(m.group(0))
        return found


SHAPE_RULES = [
    # x1234, ****1234, …1234, ...1234, ••1234, "ending in 1234", a
    # Plaid-style "mask": "1234" and an account name like "Card (1234)"
    # (capitalized word, so "loosened (0644)" is not one).
    # A letter or digit right before the x (0x1f40, 1920x1080) is not a
    # mask, nor is a year in parentheses. Obviously fake digits are
    # allowed: one digit repeated (0000), 1234, and Stripe's test-card
    # endings (4242, 4444, 0005, 9424).
    Rule(
        "card-mask",
        "block",
        re.compile(
            r"(?<![A-Za-z0-9])(?:[xX]|\*{2,}|…|•{2,}|\.{3})\d{4}(?!\d)"
            r"|\bending(?: in)? \d{4}\b"
            r"|\"mask\"\s*:\s*\"\d{4}\""
            r"|\b[A-Z][A-Za-z]* \((?!(?:19|20)\d\d\))\d{4}\)"
        ),
        allow=re.compile(r"(\d)\1\1\1|1234|4242|4444|0005|9424"),
    ),
    # Any email except GitHub noreply, example domains and noreply@.
    Rule(
        "email",
        "block",
        re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}"),
        allow=re.compile(
            r"(?i)@users\.noreply\.github\.com$"
            r"|@(?:[a-z0-9-]+\.)*example(?:\.(?:com|org|net))?$"
            r"|^noreply@|^git@github\.com$"
            r"|\.(?:png|jpe?g|gif|svg|webp|ico)$"  # icon@2x.png
        ),
    ),
    # A real home directory, either as a path or in the dashed form tools
    # use for per-project folders (-Users-YOU-code-repo). /Users/YOU and
    # /Users/Shared are the documented placeholders.
    Rule(
        "home-path",
        "block",
        re.compile(
            r"/Users/(?!(?:YOU|Shared)\b)[A-Za-z0-9._-]+"
            r"|-Users-(?!(?:YOU|Shared)-)[A-Za-z0-9._]+-"
        ),
    ),
    # A real MagicDNS name: machine.tailnet.ts.net or tailXXXX.ts.net.
    # Placeholders don't match: <mini>.<tailnet>.ts.net has brackets,
    # MINI.TAILNET.ts.net is uppercase, and a literal "tailnet" label
    # (mini.tailnet.ts.net) is allowed.
    Rule(
        "tailnet-host",
        "block",
        re.compile(r"\b[a-z0-9-]+\.[a-z0-9-]+\.ts\.net\b|\btail[0-9a-f]{4,}\.ts\.net\b"),
        allow=re.compile(r"\.tailnet\.ts\.net$"),
    ),
    # Health: Apple names devices after their owner ("Jane’s Apple
    # Watch"), so a real export carries a name in nearly every row's
    # sourceName / device. Placeholder owners are allowed.
    Rule(
        "device-owner",
        "block",
        re.compile(
            r"\b[A-Z][\w.-]*(?:'|’|&apos;|&#39;|&#[xX]27;|&#8217;|&#[xX]2019;)s\s+"
            r"(?:Apple\s+Watch|iPhone|iPad|iPod|Mac(?:Book)?|AirPods|Watch)\b"
        ),
        allow=re.compile(r"^(?:Test|User|Someone|Example|Owner|Your)\W"),
    ),
    # Health: a lat/lon with 5+ significant decimals is metre-precise
    # (real GPS); invented samples use round numbers like 47.6005.
    Rule(
        "precise-coord",
        "block",
        re.compile(
            r"(?i)\b(?:lat|lon|lng|latitude|longitude)\b[\"']?\s*[=:,]\s*[\"']?\s*"
            r"-?\d{1,3}\.\d{4}\d*[1-9]"
        ),
    ),
    # Health: the export's Me record. Only the placeholders are allowed:
    # birth date 2000-01-01, "...NotSet", or empty.
    Rule(
        "me-record",
        "block",
        re.compile(r"HKCharacteristicTypeIdentifier\w+\s*=\s*\"[^\"]*\""),
        allow=re.compile(r"=\s*\"(?:2000-01-01|HK\w*NotSet|)\"$"),
    ),
    # Dollar figures in prose: usually an example, sometimes a real one.
    Rule(
        "dollar-figure",
        "warn",
        re.compile(r"(?<![\w$])\$\d[\d,]*(?:\.\d{2})?"),
        kinds={MESSAGE},
    ),
]


# Sample data lives here. A text file added under one of these globs
# must say "synthetic" in its first LABEL_LINES lines (a comment is
# enough), or it warns: it may be a real capture copied in by mistake.
FIXTURE_GLOBS = ["examples/*"]
LABEL_LINES = 10
SYNTHETIC = re.compile(r"(?i)synthetic")
LABEL_RULE = Rule("synthetic-label", "warn", SYNTHETIC)


def is_fixture(path):
    return any(fnmatch.fnmatch(path, g) for g in FIXTURE_GLOBS)


def fail(msg):
    print(f"leakscan: {msg}", file=sys.stderr)
    sys.exit(2)


def git(*args):
    r = subprocess.run(
        ["git", "-c", "core.quotepath=off", *args],
        capture_output=True,
    )
    if r.returncode != 0:
        fail(f"git {args[0]} failed: {r.stderr.decode('utf-8', 'replace').strip()}")
    return r.stdout.decode("utf-8", "replace")


def load_denylist(path):
    text = None
    if path:
        try:
            text = open(path, encoding="utf-8").read()
        except OSError as e:
            fail(f"can't read denylist: {e.strerror}")
    elif os.environ.get("LEAKSCAN_DENYLIST_FILE"):
        p = os.environ["LEAKSCAN_DENYLIST_FILE"]
        if os.access(p, os.R_OK):
            text = open(p, encoding="utf-8").read()
    elif os.environ.get("LEAKSCAN_DENYLIST", "").strip():
        text = os.environ["LEAKSCAN_DENYLIST"]
    if text is None:
        return None
    rules = []
    for n, line in enumerate(text.splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            pat = re.compile(line, re.IGNORECASE)
        except re.error:
            # Never echo the term: it is the private part.
            fail(f"denylist line {n} is not a valid regex")
        rules.append(Rule(f"denylist:{n}", "block", pat))
    return rules


def load_allow(root):
    """.leakscan-allow: `RULE TARGET` per line. RULE is a shape rule name
    or `*`; TARGET is a path glob or `commit:<sha prefix>`."""
    entries = []
    try:
        lines = open(os.path.join(root, ".leakscan-allow"), encoding="utf-8").read().splitlines()
    except OSError:
        return entries
    for n, line in enumerate(lines, 1):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) != 2:
            fail(f".leakscan-allow line {n}: want `RULE TARGET`")
        if parts[0].startswith("denylist"):
            fail(f".leakscan-allow line {n}: the denylist can't be allowlisted")
        entries.append((parts[0], parts[1]))
    return entries


class Scanner:
    def __init__(self, rules, allow, redact):
        self.rules = rules
        self.allow = allow
        self.redact = redact
        self.hits = []  # (severity, rule, where, match)

    def allowed(self, rule, commit, path):
        if rule.name.startswith("denylist"):
            return False
        for name, target in self.allow:
            if name not in ("*", rule.name):
                continue
            if target.startswith("commit:"):
                if commit and commit.startswith(target[7:]):
                    return True
            elif path and fnmatch.fnmatch(path, target):
                return True
        return False

    def check(self, kind, text, where, commit=None, path=None):
        """Scan one piece of text. Returns True when anything blocked."""
        blocked = False
        for rule in self.rules:
            found = rule.hits(kind, text)
            if found and not self.allowed(rule, commit, path):
                for m in found:
                    self.hits.append((rule.severity, rule.name, where, m))
                blocked = blocked or rule.severity == "block"
        return blocked

    def check_label(self, path, head, where, commit=None):
        """A fixture's first lines must call it synthetic."""
        if not is_fixture(path) or self.allowed(LABEL_RULE, commit, path):
            return
        if not any(SYNTHETIC.search(line) for line in head[:LABEL_LINES]):
            self.hits.append(("warn", LABEL_RULE.name, where, "no 'synthetic' label"))

    def label_path(self, path, commit=None):
        """A path to print. Scans the path itself first; when it is the
        leak and we're redacting, it isn't printed."""
        short = commit[:10] if commit else ""
        bad = self.check(PATH, path, f"{short} file name".strip(), commit, path)
        return "<redacted path>" if bad and self.redact else path

    def scan_commits(self, revargs):
        # Messages and identities.
        out = git("log", "-z", "--format=%H%x1f%ae%x1f%ce%x1f%B", *revargs)
        n = 0
        for rec in out.split("\0"):
            if not rec.strip():
                continue
            sha, author, committer, body = rec.lstrip("\n").split("\x1f", 3)
            n += 1
            for who in {author, committer}:
                self.check(AUTHOR, who, f"{sha[:10]} author", sha)
            for i, line in enumerate(body.splitlines(), 1):
                self.check(MESSAGE, line, f"{sha[:10]} message:{i}", sha)
        # Added lines and file names. Merges show no diff here, which is
        # what we want: their content is already in the parents.
        out = git(
            "log", "-p", "-U0", "--no-color", "--no-ext-diff", "--no-textconv",
            "--format=%x00%H", *revargs,
        )
        for chunk in out.split("\0"):
            if not chunk.strip():
                continue
            lines = chunk.split("\n")
            sha = lines[0].strip()
            path = shown = None
            header = new_file = False
            head = None  # first lines of a new file, for check_label
            lineno = 0

            def finish():
                if head is not None:
                    self.check_label(path, head, f"{sha[:10]} {shown}", sha)

            for line in lines[1:]:
                if line.startswith("diff --git "):
                    finish()
                    header, path, new_file, head = True, None, False, None
                elif header and line == "--- /dev/null":
                    new_file = True
                elif header and line.startswith("+++ "):
                    target = line[4:]
                    if target != "/dev/null":
                        path = target[2:] if target.startswith("b/") else target
                        shown = self.label_path(path, sha)
                        head = [] if new_file else None
                elif line.startswith("@@"):
                    header = False
                    m = re.search(r"\+(\d+)", line)
                    lineno = int(m.group(1)) if m else 0
                elif not header and line.startswith("+") and path:
                    self.check(CONTENT, line[1:], f"{sha[:10]} {shown}:{lineno}", sha, path)
                    if head is not None and len(head) < LABEL_LINES:
                        head.append(line[1:])
                    lineno += 1
            finish()
        return n

    def scan_ref(self, ref, tag_message=None):
        short = re.sub(r"^refs/(heads|tags|remotes/[^/]+)/", "", ref)
        bad = self.check(REF, short, "ref name")
        shown = "<redacted ref>" if bad and self.redact else short
        if tag_message:
            for i, line in enumerate(tag_message.splitlines(), 1):
                self.check(MESSAGE, line, f"tag {shown} message:{i}")

    def scan_refs(self, pattern=None):
        args = ["for-each-ref", "--format=%(refname)%00%(objecttype)%00%(contents)%1e"]
        if pattern:
            args.append(pattern)
        for rec in git(*args).split("\x1e"):
            rec = rec.lstrip("\n")
            if not rec:
                continue
            ref, otype, contents = rec.split("\0", 2)
            if ref.endswith("/HEAD"):
                continue
            self.scan_ref(ref, contents if otype == "tag" else None)

    def scan_text(self, kind, text, path):
        shown = self.label_path(path)
        for i, line in enumerate(text.splitlines(), 1):
            self.check(kind, line, f"{shown}:{i}", None, path)

    def scan_tree(self):
        files = [f for f in git("ls-files", "-z").split("\0") if f]
        for f in files:
            try:
                data = open(f, "rb").read()
            except OSError:
                continue  # deleted in the working tree
            if b"\0" in data[:8000]:
                self.label_path(f)
                continue
            text = data.decode("utf-8", "replace")
            self.scan_text(CONTENT, text, f)
            self.check_label(f, text.splitlines(), f)
        return len(files)

    def report(self, scope, denylist_on):
        blocks = [h for h in self.hits if h[0] == "block"]
        warns = [h for h in self.hits if h[0] == "warn"]
        for sev, rule, where, m in blocks + warns:
            tag = "BLOCK" if sev == "block" else "warn "
            text = "" if self.redact else f"  {m[:80]!r}"
            print(f"leakscan: {tag} {rule:<14} {where}{text}")
        note = "" if denylist_on else " (no denylist: shape rules only)"
        print(f"leakscan: {scope}: {len(blocks)} blocking, {len(warns)} warnings{note}")
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as fh:
                fh.write(f"### leakscan\n\n{scope}: **{len(blocks)} blocking**, "
                         f"{len(warns)} warnings{note}\n\n")
                if blocks or warns:
                    fh.write("| | rule | where |\n|---|---|---|\n")
                    for sev, rule, where, _ in blocks + warns:
                        fh.write(f"| {sev} | {rule} | `{where}` |\n")
        return blocks, warns


def main(argv):
    mode, file_path, revargs, refs = None, None, [], []
    denylist_path, redact, fail_on_warn = None, bool(os.environ.get("CI")), False
    if "--" in argv:
        i = argv.index("--")
        argv, revargs = argv[:i], argv[i + 1:]
    it = iter(argv)
    for a in it:
        if a in ("--all", "--tree", "--range"):
            mode = a[2:]
        elif a == "--file":
            mode, file_path = "file", next(it, None)
        elif a == "--ref":
            refs.append(next(it, ""))
        elif a == "--denylist":
            denylist_path = next(it, None)
        elif a == "--redact":
            redact = True
        elif a == "--fail-on-warn":
            fail_on_warn = True
        elif a in ("-h", "--help"):
            print(__doc__)
            return 0
        else:
            fail(f"unknown argument {a!r}\n{USAGE}")
    if mode is None or (mode == "file" and not file_path) or (mode == "range" and not revargs):
        fail(USAGE)

    root = git("rev-parse", "--show-toplevel").strip()
    denylist = load_denylist(denylist_path)
    scanner = Scanner(SHAPE_RULES + (denylist or []), load_allow(root), redact)

    if mode == "all":
        os.chdir(root)
        n = scanner.scan_commits(["--all"])
        scanner.scan_refs()
        scope = f"{n} commits + refs"
    elif mode == "range":
        n = scanner.scan_commits(revargs)
        scope = f"{n} new commits"
    elif mode == "tree":
        os.chdir(root)
        scope = f"{scanner.scan_tree()} tracked files"
    else:
        try:
            text = open(file_path, encoding="utf-8").read()
        except OSError as e:
            fail(f"can't read {file_path}: {e.strerror}")
        scanner.scan_text(MESSAGE, text, os.path.basename(file_path))
        scope = file_path
    for ref in refs:
        if ref.startswith("refs/tags/"):
            scanner.scan_refs(ref)  # name + annotated message, if local
            if not git("for-each-ref", ref).strip():
                scanner.scan_ref(ref)
        else:
            scanner.scan_ref(ref)

    blocks, warns = scanner.report(scope, denylist is not None)
    if blocks:
        return 1
    if warns and fail_on_warn:
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
