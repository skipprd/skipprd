---
name: skippr-beta-release
description: >-
  Cut an immutable skipprd engine beta: SHA batteries first, then next
  production patch plus -beta-N (N monotonic), full artifact publish, never
  overwrite the tag. Use when the user says /skippr-beta-release,
  skippr-beta-release, or asks to publish a skipprd beta.
disable-model-invocation: true
---

# skippr-beta-release

Opt-in. Do not tag until every SHA battery is **success on this commit**. Stop when **python-publish**, **publish_runtime_plugins**, and **publish_skipprd** for `x.y.z-beta-N` succeeded. Do **not** run skipprd-docs. Do **not** write `latest` pointers. Do **not** move CHANGELOG Unreleased. Do **not** bump plugin crate versions (beta objects live under the tag prefix, not production crate paths). Do **not** stamp root `Cargo.toml` locally.

There is **no skippr-gate**. Same SHA batteries, tag, and GitHub publish jobs as skippr-release. Never skip matrix cells. Tags are **not** clobberable: each run appends a new `-beta-N`. PyPI does not replace a published version, so a new N is a new wheel version (`x.y.zbN`).

`x.y.z-beta-N` is a full publish of artifacts: Python wheels on PyPI and skipprd host tarballs (plus runtime plugins). Production `latest/manifest-index.json` and `latest-skipprd.txt` stay untouched. Never delete or retag.

Repo: `skipprd/skipprd`. Engine tags stay unprefixed (`18.3.10-beta-1`, not `v18.3.10-beta-1`).

## Loop (do not skip)

```
skippr-beta-release:
- [ ] Next beta named: last production tag (skip 0.0.0) + patch + -beta-N (N = max existing N for that x.y.z, else 0, then +1)
- [ ] No plugin crate bumps; no CHANGELOG Unreleased move; no skipprd-docs; no latest pointers
- [ ] Commit only the product/CI changes that belong on the tag (hooks, never --no-verify)
- [ ] git push origin HEAD
- [ ] SHA batteries green on this commit (rust.yml, skippr-fuzz.yml, skippr-tpc.yml, perf.yml, perf-db.yml)
- [ ] git tag + push the new x.y.z-beta-N (never -f, never delete the tag)
- [ ] Watch Python CI/CD + Rust CI/CD for that tag
- [ ] Fix + iterate with a new N on the new SHA until python-publish, publish_runtime_plugins, and publish_skipprd succeeded
```

## SHA batteries

```bash
python3 .github/scripts/release_sha_batteries.py --ensure --sha "$(git rev-parse HEAD)"
```

Skip re-dispatch per workflow when that SHA already succeeded; `rust.yml` also requires the named File smoke + `linux_dst` jobs. `perf.yml` / `perf-db.yml` gate on `linux_matrix`, not the frame-pointer / inferno rebuild. Timeout/red is not green. Laptop SF1 TPC is not a SHA substitute.

## Commands

Last production tag: `git tag --list '[0-9]*' --sort=-v:refname` (skip `0.0.0` and `*-beta-*`).

```bash
python3 -c "import sys; sys.path.insert(0,'.github/scripts'); from engine_tag import next_beta_tag; import subprocess; print(next_beta_tag(subprocess.check_output(['git','tag','--list','[0-9]*'], text=True).split()))"
git push origin HEAD
python3 .github/scripts/release_sha_batteries.py --ensure --sha "$(git rev-parse HEAD)"
git tag <x.y.z-beta-N>
git push origin <x.y.z-beta-N>
```

Do not stamp root `Cargo.toml` locally (stays `0.1.0`; CI stamps `x.y.z-beta-N`). Do not commit a `-beta` version in `pyproject.toml`. CI stamps the skippr wheel to PEP 440 `x.y.zbN` for that tag so PyPI gets a unique pre-release. `pip install skippr` still prefers the last production; `pip install skippr==x.y.zbN` (or `==x.y.z-beta-N`) gets the beta.

Scratch `0.0.0` is not required: the beta tag **is** the CI vehicle. Workflow/harness edits ship on this tag.

Watch: `gh run list --repo skipprd/skipprd` / `gh run watch` for **Python CI/CD Pipeline** (`ci.yml`) and **Rust CI/CD Pipeline** (`rust.yml`) on the beta tag. Success is `python-publish` + `publish_runtime_plugins` + `publish_skipprd` (skipprd linux/mac tarballs under `releases/skipprd/<tag>/`, plugins under the tag prefix, wheels on PyPI). GitHub Release is `--prerelease`.

## Iterate

If a job fails: fix, commit with hooks, re-run SHA batteries on the new SHA. Cut the **next** `x.y.z-beta-N` (`next_beta_tag`). Do not force-update or delete the old tag. Do not force-push `main`. Do not bump the production patch. Do not bump plugin crates — beta objects are tag-scoped and unique per N.

`aws s3 sync --delete` is illegal.

Do not ask the user to watch CI.

## Must not

- Compose or publish skippr-web / skipprd-docs
- Write `latest/manifest-index.json` or `latest-skipprd.txt`
- Consume `CHANGELOG` Unreleased into a production heading
- Skip a SHA battery or a matrix cell
- Clobber a git tag, GitHub Release, PyPI version, or CDN key from a previous N
