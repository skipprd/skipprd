---
name: skippr-release
description: >-
  Cut a skipprd engine release: bump changed plugin crates, CHANGELOG, commit
  with pre-commit tests, tag, push, watch GitHub Actions, fix until published,
  then surgical public docs. Use when the user says /skippr-release,
  skippr-release, or asks to tag/publish skipprd.
disable-model-invocation: true
---

# skippr-release

Opt-in. Do not stop because the tag exists. Stop when wheels, host binaries, and runtime plugins for that tag are published and skipprd-docs has run.

Read `AGENTS.md` and skill **`skipprd-docs`** first. Call skipprd-docs; do not duplicate it.

Plugin crate `version` in `plugins/**/Cargo.toml` is the CDN path identity. Version paths are immutable. Root skipprd crate stays `0.1.0` until CI stamps the tag. Engine tags are unprefixed semver (`18.3.6`), not `v18.3.6`. Repo: `skipprd/skipprd`.

## Loop (do not skip)

```
skippr-release:
- [ ] Next engine tag named (user arg or last unprefixed tag + patch, excluding 0.0.0)
- [ ] Plugin crates bumped for changed checksum inputs
- [ ] Planner dry-run clean (no immutable-path SystemExit)
- [ ] CHANGELOG Unreleased → [tag]; python semver iff python/ or pyproject.toml changed
- [ ] skipprd-docs applied (engine how-tos only, surgical)
- [ ] git commit with hooks (never --no-verify)
- [ ] tag + push (0.0.0 first iff workflows/harness changed)
- [ ] Watch Python CI/CD + Rust CI/CD for that tag
- [ ] Fix + iterate until python-publish, publish_runtime_plugins, and publish_skipprd succeeded
- [ ] skipprd-docs on skippr-web after publish (compose + surgical owned pages)
```

## Commands

Last tag: `git tag --list '[0-9]*' --sort=-v:refname` (skip `0.0.0`).

```bash
python3 .github/scripts/bump_changed_plugin_crates.py --base <last-tag>
python3 .github/scripts/runtime_plugin_release_plan.py
./scripts/install-git-hooks.sh   # if the hook is missing
git add … && git commit …        # hook runs scripts/precommit.sh; never --no-verify
git push origin HEAD
git tag <tag> && git push origin <tag>
```

Python wheel: bump `pyproject.toml` and `python/Cargo.toml` together **only** when `python/` or `pyproject.toml` changed since `<last-tag>`. Do not bump Python just to clobber PyPI. Do not stamp root `Cargo.toml`.

CHANGELOG: move `## [Unreleased]` bullets into `## [<tag>]`; leave an empty Unreleased.

Scratch `0.0.0` first when `.github/workflows/` or the e2e harness changed; wait until that run is healthy, then push the product tag. `0.0.0` does not publish wheels.

Watch: `gh run list --repo skipprd/skipprd` / `gh run watch` for **Python CI/CD Pipeline** (`ci.yml`) and **Rust CI/CD Pipeline** (`rust.yml`) on the tag. Success is `python-publish` (non-`0.0.0`) + `publish_runtime_plugins` + `publish_skipprd`.

## Iterate

If a job fails **before** those publish jobs: fix, commit with hooks. If the remote tag has no successful publish yet, delete and re-push the **same** tag. Do not force-push `main`.

If any publish job already uploaded plugins or wheels: do not overwrite. Bump the engine patch and any plugin crate that would clobber; new tag.

`aws s3 sync --delete` is illegal. Do not ask the user to watch CI.

## Docs last

After PyPI / GitHub / CDN are live, follow **`skipprd-docs`**.
