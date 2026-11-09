#!/usr/bin/env python3
"""release: cut a vitals release. Every deploy is a release.

Versions are calendar based: vYYYY.MM.N, N counting from 0 each month
(local time). Notes live in CHANGELOG.md, newest first, so every leak
scan layer (commit hook, pre-push, CI) checks them like any other file.

  release.py draft [--version V]
      Check main is clean and not behind origin, pick the next version,
      and write its section at the top of CHANGELOG.md: a list drafted
      by git-cliff (cliff.toml) from commit subjects since the last tag.
      Re-running replaces the draft. Edit it before cutting; a big
      release gets a short summary paragraph under the heading.
  release.py cut
      Check the draft, scan it for personal data, commit CHANGELOG.md as
      `release: V` and create the annotated tag V. Pushes nothing:
      `git push --atomic origin main V` publishes (the tag triggers
      .github/workflows/release.yml).
  release.py notes V
      Print V's notes without the heading (the workflow uses this for
      the GitHub Release body).

`make release-draft` runs draft; `make release` runs `make check`, then cut.
Standard library only, Python 3.9+; draft also needs git-cliff on PATH.
"""

import datetime
import os
import re
import shutil
import subprocess
import sys
import tempfile

CHANGELOG = "CHANGELOG.md"
HEADER = """# Changelog

Every deploy of vitals is a release, versioned `vYYYY.MM.N` (N counts
from 0 each month). Each section is drafted from commit subjects by
git-cliff (`cliff.toml`) through `scripts/release.py`; big releases open
with a short summary. See `docs/RELEASING.md`.
"""
VERSION_RE = re.compile(r"^v(\d{4})\.(\d{2})\.(\d+)$")
SECTION_RE = re.compile(r"^## (v\d{4}\.\d{2}\.\d+)\b.*$", re.M)


def fail(msg):
    print(f"release: {msg}", file=sys.stderr)
    sys.exit(1)


def git(*args, check=True, raw=False):
    r = subprocess.run(["git", *args], capture_output=True, text=True)
    if check and r.returncode != 0:
        fail(f"git {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout if raw else r.stdout.strip()


def root():
    return git("rev-parse", "--show-toplevel")


def split_changelog(text):
    """-> [(version, section text)] newest first. The header is dropped:
    it is always rewritten from HEADER."""
    marks = list(SECTION_RE.finditer(text))
    out = []
    for i, m in enumerate(marks):
        end = marks[i + 1].start() if i + 1 < len(marks) else len(text)
        out.append((m.group(1), text[m.start():end].strip() + "\n"))
    return out


def read_sections():
    try:
        return split_changelog(open(CHANGELOG, encoding="utf-8").read())
    except FileNotFoundError:
        return []


def write_sections(sections):
    body = "\n".join(s for _, s in sections)
    with open(CHANGELOG, "w", encoding="utf-8") as f:
        f.write(HEADER + "\n" + body)


def release_tags():
    return [t for t in git("tag", "-l", "v*").split() if VERSION_RE.match(t)]


def next_version(today, tags):
    prefix = f"v{today:%Y.%m}."
    used = [int(t[len(prefix):]) for t in tags if t.startswith(prefix)]
    return f"{prefix}{max(used) + 1 if used else 0}"


def check_branch(allow_changelog=False):
    if git("rev-parse", "--abbrev-ref", "HEAD") != "main":
        fail("releases are cut from main; merge the feature branch first")
    dirty = [l[3:] for l in git("status", "--porcelain", raw=True).splitlines() if l]
    if allow_changelog:
        dirty = [p for p in dirty if p != CHANGELOG]
    if dirty:
        fail(f"working tree not clean: {', '.join(dirty)}")
    git("fetch", "--quiet", "--tags", "origin")
    behind = git("rev-list", "--count", "HEAD..origin/main")
    if behind != "0":
        fail(f"main is {behind} commits behind origin/main; pull first")


def draft(version):
    if not shutil.which("git-cliff"):
        fail("git-cliff isn't installed (brew install git-cliff)")
    check_branch(allow_changelog=True)
    tags = release_tags()
    version = version or next_version(datetime.date.today(), tags)
    if not VERSION_RE.match(version):
        fail(f"{version} isn't vYYYY.MM.N")
    if version in tags:
        fail(f"{version} is already tagged")
    sections = [s for s in read_sections() if s[0] != version]
    if sections and sections[0][0] not in tags:
        fail(f"CHANGELOG.md starts with {sections[0][0]}, which isn't tagged; "
             "cut or remove it first")
    heading = f"## {version} ({datetime.date.today():%Y-%m-%d})"
    if not tags:
        text = (f"{heading}\n\nFirst tagged release. Earlier history is in "
                "`git log`.\n")
    else:
        r = subprocess.run(
            ["git-cliff", "--unreleased", "--tag", version, "--strip", "all"],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            fail(f"git-cliff: {r.stderr.strip()}")
        lines = r.stdout.strip().splitlines()
        # git-cliff dates in UTC; the version uses local time, so the
        # heading does too.
        if lines and lines[0].startswith("## "):
            lines = lines[1:]
        text = heading + "\n" + "\n".join(lines).rstrip() + "\n"
        if "### " not in text:
            text = f"{heading}\n\nMaintenance only: no user-facing changes.\n"
    write_sections([(version, text)] + sections)
    print(text)
    print(f"release: drafted {version} in {CHANGELOG}. Edit it (add a summary "
          "under the heading for a big release), then `make release`.")


def cut():
    check_branch(allow_changelog=True)
    sections = read_sections()
    if not sections:
        fail("CHANGELOG.md has no sections; run `make release-draft` first")
    version, text = sections[0]
    tags = release_tags()
    if version in tags:
        fail(f"{version} is already tagged; run `make release-draft` for the next one")
    if git("ls-remote", "--tags", "origin", f"refs/tags/{version}"):
        fail(f"{version} already exists on origin")
    if not git("status", "--porcelain", "--", CHANGELOG):
        fail("CHANGELOG.md has no new draft; run `make release-draft` first")
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
        f.write(text)
        notes = f.name
    try:
        scan = subprocess.run(
            [sys.executable, "-I", os.path.join("scripts", "leakscan.py"), "--file", notes],
        )
    finally:
        os.unlink(notes)
    if scan.returncode != 0:
        fail("the notes failed the leak scan; edit CHANGELOG.md and retry")
    git("add", CHANGELOG)
    git("commit", "-q", "-m", f"release: {version}")
    git("tag", "-a", version, "-m", f"vitals {version}")
    print(f"release: committed and tagged {version} (not pushed).")
    print(f"release: publish with: git push --atomic origin main {version}")


def notes(version):
    for v, text in read_sections():
        if v == version:
            body = text.split("\n", 1)[1] if "\n" in text else ""
            print(body.strip())
            return
    fail(f"no {version} section in {CHANGELOG}")


def main(argv):
    os.chdir(root())
    if argv[:1] == ["draft"]:
        if len(argv) == 3 and argv[1] == "--version":
            return draft(argv[2])
        if len(argv) == 1:
            return draft(None)
    elif argv == ["cut"]:
        return cut()
    elif len(argv) == 2 and argv[0] == "notes":
        return notes(argv[1])
    elif argv[:1] in (["-h"], ["--help"]):
        print(__doc__)
        return
    fail("usage: release.py draft [--version V] | cut | notes V")


if __name__ == "__main__":
    main(sys.argv[1:])
