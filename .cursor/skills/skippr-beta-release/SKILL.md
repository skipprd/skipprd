---
name: skippr-beta-release
description: >-
  Cut a clobberable skipprd engine beta: next production patch plus -beta, same
  CI as a release except docs and PyPI, overwrite that tag and its published
  host/plugins/manifests on every run. Use when the user says
  /skippr-beta-release, skippr-beta-release, or asks to publish a skipprd beta.
disable-model-invocation: true
---

# skippr-beta-release

Opt-in. Do not stop because the tag exists. Stop when host binaries and runtime plugins for `x.y.z-beta` are published. Do **not** run skipprd-docs. Do **not** publish PyPI. Do **not** move CHANGELOG Unreleased. Do **not** bump plugin crate versions (beta objects live under the tag prefix, not production crate paths).

Production `x.y.z` CDN paths, `latest/manifest-index.json`, and `latest-skipprd.txt` stay untouched. This tag is meant to be deleted and re-pushed.

Repo: `skipprd/skipprd`. Engine tags stay unprefixed (`18.3.9-beta`, not `v18.3.9-beta`).

## Loop (do not skip)

```
skippr-beta-release:
- [ ] Next beta named: last production tag (skip 0.0.0 and *-beta) + patch + -beta
- [ ] No plugin crate bumps; no CHANGELOG Unreleased move; no skipprd-docs
- [ ] Commit only the product/CI changes that belong on the tag (hooks, never --no-verify)
- [ ] Delete+re-push the same x.y.z-beta tag (clobber GitHub release assets if present)
- [ ] Watch Python CI/CD + Rust CI/CD for that tag
- [ ] Fix + iterate, re-clobbering the same beta tag, until publish_runtime_plugins and publish_skipprd succeeded
```

## Commands

Last production tag: `git tag --list '[0-9]*' --sort=-v:refname` (skip `0.0.0` and `*-beta`).

```bash
python3 -c "import sys; sys.path.insert(0,'.github/scripts'); from engine_tag import next_beta_tag; import subprocess; print(next_beta_tag(subprocess.check_output(['git','tag','--list','[0-9]*'], text=True).split()))"
# example: 18.3.9-beta when 18.3.8 is latest production
git push origin HEAD
git tag -f <x.y.z-beta>
git push origin :refs/tags/<x.y.z-beta> || true
git push origin <x.y.z-beta>
```

Do not stamp root `Cargo.toml` locally (stays `0.1.0`; CI stamps `x.y.z-beta`). Do not bump `pyproject.toml`.

Scratch `0.0.0` is not required: the beta tag **is** the CI vehicle. Workflow/harness edits ship on this tag.

Watch: `gh run list --repo skipprd/skipprd` / `gh run watch` for **Python CI/CD Pipeline** (`ci.yml`) and **Rust CI/CD Pipeline** (`rust.yml`) on the beta tag. Success is `publish_runtime_plugins` + `publish_skipprd`. `python-publish` is skipped. GitHub Release is `--prerelease`.

## Iterate

If a job fails: fix, commit with hooks. Delete and re-push the **same** `x.y.z-beta` tag (and `gh release delete` if the GitHub Release would keep stale notes). Do not force-push `main`. Do not bump the production patch. Do not bump plugin crates to get past immutable-path checks — beta objects are tag-scoped and overwrite.

`aws s3 sync --delete` is still illegal. Overwriting keys under `releases/runtime-plugins/<tag>-beta/` and `releases/skipprd/<tag>-beta/` is required.

Do not ask the user to watch CI.

## Must not

- Compose or publish skippr-web / skipprd-docs
- Write `latest/manifest-index.json` or `latest-skipprd.txt`
- Publish wheels to PyPI
- Consume `CHANGELOG` Unreleased into a production heading
