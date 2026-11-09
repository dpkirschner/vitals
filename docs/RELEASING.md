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

## Cutting a release

```
make release-draft      # next version's section at the top of CHANGELOG.md
$EDITOR CHANGELOG.md    # tidy it; a big release gets a summary paragraph
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

git-cliff is needed for drafting only (`brew install git-cliff`).

## Deploying

A deploy installs a published release; see
[DEPLOYMENT.md](DEPLOYMENT.md#deploying-a-release). After the release
workflow is green:

```
scripts/deploy.sh build vYYYY.MM.N     # no sudo
scripts/deploy.sh install vYYYY.MM.N   # sudo, in a terminal
```

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
