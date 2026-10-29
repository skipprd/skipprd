---
name: skippr-beta-release
description: >-
  Cut a clobberable skipprd engine beta: SHA batteries first, then next
  production patch plus -beta, full artifact publish, overwrite that tag on
  every run. Use when the user says /skippr-beta-release, skippr-beta-release,
  or asks to publish a skipprd beta.
disable-model-invocation: true
---

# skippr-beta-release

Opt-in. Do not tag until every SHA battery is **success on this commit**. Stop when **python-publish**, **publish_runtime_plugins**, and **publish_skipprd** for `x.y.z-beta` succeeded. Do **not** run skipprd-docs. Do **not** write `latest` pointers. Do **not** move CHANGELOG Unreleased. Do **not** bump plugin crate versions (beta objects live under the tag prefix, not production crate paths). Do **not** stamp root `Cargo.toml` locally.

There is **no skippr-gate**. Same batteries as skippr-release. Never skip matrix cells.

`*-beta` is a full publish of artifacts: Python wheels on PyPI and skipprd host tarballs (plus runtime plugins). Production `latest/manifest-index.json` and `latest-skipprd.txt` stay untouched. This tag is meant to be deleted and re-pushed.

Repo: `skipprd/skipprd`. Engine tags stay unprefixed (`18.3.10-beta`, not `v18.3.10-beta`).

## Loop (do not skip)

```
skippr-beta-release:
- [ ] Next beta named: last production tag (skip 0.0.0 and *-beta) + patch + -beta
- [ ] No plugin crate bumps; no CHANGELOG Unreleased move; no skipprd-docs; no latest pointers
- [ ] Commit only the product/CI changes that belong on the tag (hooks, never --no-verify)
- [ ] git push origin HEAD
- [ ] SHA batteries green on this commit (rust.yml, skippr-fuzz.yml, skippr-tpc.yml, perf.yml, perf-db.yml)
- [ ] Delete+re-push the same x.y.z-beta tag (clobber GitHub release assets if present)
- [ ] Watch Python CI/CD + Rust CI/CD for that tag
- [ ] Fix + iterate, re-clobbering the same beta tag, until python-publish, publish_runtime_plugins, and publish_skipprd succeeded
```

## SHA batteries

```bash
python3 .github/scripts/release_sha_batteries.py --ensure --sha "$(git rev-parse HEAD)"
```

Skip re-dispatch per workflow when that SHA already succeeded; `rust.yml` also requires the named File smoke + `linux_dst` jobs. Timeout/red is not green. Laptop SF1 TPC is not a SHA substitute.

## Commands

Last production tag: `git tag --list '[0-9]*' --sort=-v:refname` (skip `0.0.0` and `*-beta`).

```bash
python3 -c "import sys; sys.path.insert(0,'.github/scripts'); from engine_tag import next_beta_tag; import subprocess; print(next_beta_tag(subprocess.check_output(['git','tag','--list','[0-9]*'], text=True).split()))"
git push origin HEAD
python3 .github/scripts/release_sha_batteries.py --ensure --sha "$(git rev-parse HEAD)"
git tag -f <x.y.z-beta>
git push origin :refs/tags/<x.y.z-beta> || true
git push origin <x.y.z-beta>
```

Do not stamp root `Cargo.toml` locally (stays `0.1.0`; CI stamps `x.y.z-beta`). Do not bump `pyproject.toml`. Wheel filenames stay the pyproject semver (`python_semver` is independent of engine tags). Do not put `-beta` in `pyproject.toml`.

Scratch `0.0.0` is not required: the beta tag **is** the CI vehicle. Workflow/harness edits ship on this tag.

Watch: `gh run list --repo skipprd/skipprd` / `gh run watch` for **Python CI/CD Pipeline** (`ci.yml`) and **Rust CI/CD Pipeline** (`rust.yml`) on the beta tag. Success is `python-publish` + `publish_runtime_plugins` + `publish_skipprd` (skipprd linux/mac tarballs under `releases/skipprd/<tag>/`, plugins under the tag prefix, wheels on PyPI). GitHub Release is `--prerelease`.

## Iterate

If a job fails: fix, commit with hooks, re-run SHA batteries on the new SHA. Delete and re-push the **same** `x.y.z-beta` tag (and `gh release delete` if the GitHub Release would keep stale notes). Do not force-push `main`. Do not bump the production patch. Do not bump plugin crates to get past immutable-path checks — beta objects are tag-scoped and overwrite.

`aws s3 sync --delete` is still illegal. Overwriting keys under `releases/runtime-plugins/<tag>-beta/` and `releases/skipprd/<tag>-beta/` is required.

Do not ask the user to watch CI.

## Must not

- Compose or publish skippr-web / skipprd-docs
- Write `latest/manifest-index.json` or `latest-skipprd.txt`
- Consume `CHANGELOG` Unreleased into a production heading
- Skip a SHA battery or a matrix cell
