# Releasing

Every deploy of vitals is a release. Releases are calendar versioned,
`vYYYY.MM.N`, where N counts from 0 each month (local time): the first
release in November 2026 is `v2026.11.0`, the next `v2026.11.1`. Notes
live in [`CHANGELOG.md`](../CHANGELOG.md), newest first, and each tag
gets a GitHub Release with the same text.

The tooling is a copy of outflow's (`scripts/release.py`,
`scripts/deploy.sh`, `scripts/leakscan.py`, `cliff.toml`, the two
workflows). When one changes, port the fix to the other repo.

## Branches

Work happens on feature branches (`feat/…`, `fix/…`, `docs/…`). When a
change is whole, merge it into `main` locally; there are no PRs.
Releases are cut from `main` only, and `main` can't be force-pushed or
deleted (a GitHub ruleset). Commit subjects become the release notes, so
write them for a reader: `feat(core): …`, `fix: …`, `docs: …`, or an
area prefix (`core:`, `cli:`, `server:`). `chore:`, `style:`, `build:`,
`ci:`, `test:`, merge and `release:` commits are left out of the notes.

## Testing a branch

Before merging, run the branch against a copy of the live data:

```
scripts/deploy.sh try feat/my-change     # no sudo; nothing live changes
```

It builds the branch with the release features (the CLI with `client`
too), copies the live DB into a 0700 scratch dir, starts that build on
`127.0.0.1:8091` (`--port P`) with Oura off, runs the same checks as
`install` (`--check SCRIPT` adds one), and waits. Ctrl-C deletes the copy;
`try --clean` does it after a crash. `--oura` turns Oura on with the live
settings and the live token file, which it shares and never copies (Oura
refresh tokens are single-use).

## Cutting a release

```
make release-draft      # next version's section at the top of CHANGELOG.md
$EDITOR CHANGELOG.md    # tidy it (below)
make release            # make check, leak-scan the notes, commit, tag
git push --atomic origin main vYYYY.MM.N
```

- `make release-draft` (`scripts/release.py draft`) refuses unless you're
  on a clean `main` that isn't behind `origin/main`. It picks the next
  version, asks git-cliff (`cliff.toml`) for the list of commits since
  the last tag (Features, Fixes, Changes, Docs; bead ids from commit
  bodies appended), and writes the section. Running it again replaces
  the draft. `--version V` overrides the number.
- `make release` runs `make check`, then `scripts/release.py cut`: it
  scans the new section with `scripts/leakscan.py`, commits
  `CHANGELOG.md` as `release: V` and creates the annotated tag `V`. It
  pushes nothing.
- The atomic push sends `main` and the tag together. The pre-push hook
  scans both. The tag starts `.github/workflows/release.yml`, which
  checks the tag is `vYYYY.MM.N` and on `main`, rescans all history,
  and publishes the GitHub Release from `scripts/release.py notes V`.

Tidying the draft means:

- A big release opens with a short summary paragraph: what changed for the
  user, in a sentence or three.
- A fix to code that hasn't been released yet is folded into its feature's
  line, not listed as a fix: users never saw the bug.
- A release that bumps the schema says so, and that rolling back needs
  `scripts/deploy.sh rollback --restore-db`.
- Lines are written for a reader; drop noise. No personal data (the cut
  scans for it anyway).

git-cliff is needed for drafting only (`brew install git-cliff`).

## Deploying

A deploy installs a published release; see
[DEPLOYMENT.md](DEPLOYMENT.md#deploying-a-release). After the release
workflow is green:

```
scripts/deploy.sh build vYYYY.MM.N     # no sudo
scripts/deploy.sh env set KEY=VALUE    # only if the release needs it
scripts/deploy.sh install vYYYY.MM.N   # sudo, in a terminal
scripts/deploy.sh verify --log RE      # what the release should log
```

A release that needs a new setting changes the plist with `deploy.sh env`
before `install`, which then starts with it. `verify` reports what it
observed after the install (see DEPLOYMENT.md).

The build compiles with `VITALS_VERSION=<tag>`, so the server answers
`GET /api/version` (and the `version` field of `status`) with the tag
it was built from, and `vitals --version` prints it.
`scripts/deploy.sh rollback` goes back to the previous release; add
`--restore-db` when the live release migrated the schema.

## Mistakes

Release tags can't be moved or deleted (a GitHub ruleset on `v*`). A
release with a problem is fixed by the next one: fix it on `main`, then
cut `vYYYY.MM.N+1`. A local tag that hasn't been pushed can still be
undone with `git tag -d V` and `git reset --soft HEAD~1`. If the release
workflow failed for a transient reason, re-run it from the Actions tab.

## Personal data

This repo is public and the data it handles is health data. Three layers
keep personal data out; see `scripts/leakscan.py` for the rules.

- **Commit hooks** (on the maintainer's machine) block known terms.
- **Pre-push** rescans every pushed commit, ref name and tag message:
  card masks, personal emails, home paths, tailnet hostnames, and for
  health data device names that carry an owner's name, metre-precise
  coordinates, and Me-record values other than the placeholders; plus
  the private denylist. Warnings (dollar figures in messages, a file
  under `examples/` without a "synthetic" label) ask y/N in a terminal;
  a push without a terminal stops until it's re-run with
  `LEAKSCAN_ACK=1`.
- **CI** (`.github/workflows/leakscan.yml`) rescans all history on every
  push and weekly, printing locations only.

A false hit from a shape rule goes in `.leakscan-allow` (rule plus path
glob or `commit:<sha>`). The denylist has no exceptions. If something
real is found in history: fix it forward first, then rewrite history on
a scratch clone (the `main` ruleset has to be switched off for the
force-push, and back on after).
