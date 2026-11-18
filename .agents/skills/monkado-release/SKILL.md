---
name: monkado-release
description: Check Monkado's current version and release state before branching, merging, bumping or releasing, and cut a release with the Release workflow. Use whenever work will be merged to main, when the owner asks for a release/package/new version, or when a version number appears in a change (updater, Cargo.toml, changelog).
---

# Monkado versions and releases

Always know the current version before acting. Releases are cut by the GitHub `Release` workflow,
which bumps the version itself; never edit the workspace version by hand for a release.

## Check the current state first

Run these before merging to `main`, proposing a version or starting a release:

```sh
git fetch origin --tags
sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1   # version on this checkout
git show origin/main:Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1
gh release list -R monkey-land/monkado --limit 3          # latest published release
gh run list -R monkey-land/monkado --workflow release.yml --limit 3
git log --oneline -5 origin/main                           # "chore: versão X" marks a release
```

Report them in one line, for example: "main 1.2.0, latest release v1.2.0, no release running".

## Rules

- **A release in progress owns `main`.** The workflow commits `chore: versão X` and pushes to
  `main` at the end; any push to `main` meanwhile makes it fail before publishing. While a run is
  queued or in progress, do not merge or push to `main`; wait or tell the owner.
- **Never start a release someone else already started for the same version.** Check the run list
  and its actor first; two runs for one version race each other.
- **The next version follows semver from the latest release:** fixes only → patch (1.2.0 → 1.2.1);
  any new feature → minor (1.2.1 → 1.3.0); breaking protocol or storage change → state it and ask.
  The workflow rejects a version that is not greater than the one in `Cargo.toml`.
- **The released build is the commit the run checked out.** Merge everything intended for the
  release before starting it, and confirm the run's `head_sha` is the expected `main` commit.
- **Installed clients update through the in-app updater**, which reads the stable GitHub releases
  of `monkey-land/monkado` and the `monkado-<tag>-Windows-X64.zip` + `SHA256SUMS.txt` assets.
  Do not rename those assets or the repository path without updating `apps/desktop/src/updater.rs`.
  Builds 1.0.0 and older cannot self-update (channel and repository-case bugs); tell users to
  install the newer setup manually.

## Cutting a release

Only when the owner asks for a release or package:

1. Check the state above; nothing running, everything merged.
2. Start it: `gh workflow run release.yml -R monkey-land/monkado -f version=X.Y.Z`.
   If the tool permission layer refuses, give the owner this exact command instead.
3. Watch the run until it completes (Windows build takes about 30 minutes). On failure, read the
   failed step's log and report the cause; a rejected push in "Salvar versão no repositório"
   means `main` moved during the run, and the release must be started again from the new `main`.
4. Confirm with `gh release view vX.Y.Z` that the setup, zip and `SHA256SUMS.txt` assets exist,
   then report the release link.
