---
name: skippr-release
description: >-
  Cut a skipprd engine release: SHA batteries first (rust e2e+DST, skippr-fuzz,
  skippr-tpc SF10, skippr-perf, skippr-perf-db), then bump plugin crates,
  CHANGELOG, commit with hooks, tag, publish, skipprd-docs. Use when the user
  says /skippr-release, skippr-release, or asks to tag/publish skipprd.
disable-model-invocation: true
---

# skippr-release

Opt-in. Do not tag until every SHA battery is **success on this commit**. Do not stop because the tag exists. Stop when wheels, host binaries, and runtime plugins for that tag are published and skipprd-docs has run.

There is **no skippr-gate**. This skill is the orchestrator. Never skip matrix cells. `continue-on-error` is illegal. Skip re-dispatch per workflow when this SHA already succeeded; `rust.yml` also requires the named File smoke + `linux_dst` jobs.

Read `AGENTS.md` and skill **`skipprd-docs`** first. Call skipprd-docs; do not duplicate it.

Plugin crate `version` in `plugins/**/Cargo.toml` is the CDN path identity. Version paths are immutable. Root skipprd crate stays `0.1.0` until CI stamps the tag. Engine tags are unprefixed semver (`18.3.6`), not `v18.3.6`. Repo: `skipprd/skipprd`. Immutable pre-release testing is **`/skippr-beta-release`** (`x.y.z-beta-N`), not this skill.

## Loop (do not skip)

```
skippr-release:
- [ ] Next engine tag named (user arg or last unprefixed tag + patch, excluding 0.0.0)
- [ ] Plugin crates bumped for changed checksum inputs
- [ ] Planner dry-run clean (no immutable-path SystemExit)
- [ ] CHANGELOG Unreleased → [tag]; python semver iff python/ or pyproject.toml changed
- [ ] Engine how-tos in skipprd/docs (same commit as the tag; surgical)
- [ ] git commit with hooks (never --no-verify; DST + lib proptest run here)
- [ ] git push origin HEAD
- [ ] SHA batteries green on this commit (skip re-dispatch per workflow if that SHA succeeded; rust.yml also requires File smoke + linux_dst jobs)
- [ ] tag + push (0.0.0 first iff workflows/harness changed)
- [ ] Watch Python CI/CD + Rust CI/CD for that tag
- [ ] Fix + iterate until python-publish, publish_runtime_plugins, and publish_skipprd succeeded
- [ ] skipprd-docs on skippr-web after publish (compose + surgical owned pages). Do not compose skippr-web before publish.
```

## SHA batteries

Batteries (all must pass; never skip a cell):

| Workflow | What it proves |
|---|---|
| `rust.yml` | File smoke e2e + `linux_dst` (`sqlrt::dst::`). CDN e2e still waits for the tag. |
| `skippr-fuzz.yml` | Timeboxed cargo fuzz; timeout fails |
| `skippr-tpc.yml` | TPC-H **SF10** Q1/Q3/Q6 vs DuckDB + vanilla DataFusion + `skipprd query` |
| `perf.yml` | skippr-perf `linux_matrix` (timings/correctness). Not frame-pointer / flamegraph / heaptrack / perf-stat. |
| `perf-db.yml` | skippr-perf-db `linux_matrix`. Not frame-pointer / flamegraph / heaptrack / perf-stat. |

```bash
python3 .github/scripts/release_sha_batteries.py --ensure --sha "$(git rev-parse HEAD)"
# non-zero: missing or red on this SHA. Watch in-flight runs, then re-run --ensure.
# zero: every workflow already succeeded on this SHA — do not re-dispatch; tag.
```

Do not treat a red SHA as green. Do not skip a workflow because a laptop SF1 TPC or a local fuzz timebox passed.

## Commands

Last tag: `git tag --list '[0-9]*' --sort=-v:refname` (skip `0.0.0`).

```bash
python3 .github/scripts/bump_changed_plugin_crates.py --base <last-tag>
python3 .github/scripts/runtime_plugin_release_plan.py
./scripts/install-git-hooks.sh   # if the hook is missing
git add … && git commit …        # hook runs scripts/precommit.sh; never --no-verify
git push origin HEAD
python3 .github/scripts/release_sha_batteries.py --ensure --sha "$(git rev-parse HEAD)"
git tag <tag> && git push origin <tag>
```

Python wheel: bump `pyproject.toml` and `python/Cargo.toml` together **only** when `python/` or `pyproject.toml` changed since `<last-tag>`. Do not bump Python just to clobber PyPI. Do not stamp root `Cargo.toml`.

CHANGELOG: move `## [Unreleased]` bullets into `## [<tag>]`; leave an empty Unreleased.

Scratch `0.0.0` first when `.github/workflows/` or the e2e harness changed; wait until that run is healthy, then push the product tag. `0.0.0` does not publish wheels.

Watch: `gh run list --repo skipprd/skipprd` / `gh run watch` for **Python CI/CD Pipeline** (`ci.yml`) and **Rust CI/CD Pipeline** (`rust.yml`) on the tag. Success is `python-publish` (non-`0.0.0`) + `publish_runtime_plugins` + `publish_skipprd`.

## Iterate

If a job fails **before** those publish jobs: fix, commit with hooks, re-run SHA batteries on the new SHA. If the remote tag has no successful publish yet, delete and re-push the **same** tag. Do not force-push `main`.

If any publish job already uploaded plugins or wheels: do not overwrite. Bump the engine patch and any plugin crate that would clobber; new tag.

`aws s3 sync --delete` is illegal. Do not ask the user to watch CI.

## Docs last

After PyPI / GitHub / CDN are live, follow **`skipprd-docs`**.
