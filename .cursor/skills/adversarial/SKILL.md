---
name: adversarial
description: >-
  Combative plan → red-team → implement → prove → hostile audit loop until the
  scoped work is actually complete. Use when the user says /adversarial,
  adversarial, red-team loop, work until clean, or asks for tireless refactor
  against drift, stringly APIs, dual paths, or incomplete cutovers. Do not use
  for one-shot questions or trivial edits.
disable-model-invocation: true
---

# Adversarial

Default agent work is one-shot. This skill is **opt-in**. Once invoked, do not
stop because the named patch compiles. Stop only when a hostile audit of the
**current slice** returns zero in-scope findings and mechanical proofs are green.

You are the implementer. You are not the auditor of your own work. Same-turn
self-review is how leftover forks survive.

## Load the repo first

Read `AGENTS.md`, `.cursor/rules/`, and project skills **before** planning.
Those files are the local dialect (Tiger Style, specs, TDD, naming). This skill
is the **process**. Do not bake product-specific identifiers into greps. Derive
prove-greps from the chosen SoT in *this* repo.

If project skills exist (`hard-cutover`, `tdd-*`, style/spec skills), **follow
them**. Do not restate them.

## Scope

| User said | Mode |
|-----------|------|
| `/adversarial <target>` | **Slice:** finish that job, including same-class drift it illuminates |
| `/adversarial` on a messy tree, or “until the project is clean” | **Campaign:** inventory → cut one fork completely → re-inventory → next. Do not ask for the next prompt |

**Follow the thread.** If fixing stringly dispatch in one module reveals the same
class next door, that is in scope. Do not file it as “follow-up.”

**Out of scope** only when the user excluded it, or the repo’s own docs mark it
as future / not-shipping / must-not-touch. “Locked decision” is not a reason to
keep a fork — amend the lock in the same change if lock text *is* the fork.

## Loop (do not skip)

Copy and keep this list current. Do not mark the slice done with open boxes.

```
Adversarial:
- [ ] SoT named (one type / one module / one CLI / one spec authority)
- [ ] Plan red-teamed (subagent) before code
- [ ] Failing test first (compile-fail / typed reject / behaviour)
- [ ] Old path deleted in the same change (no shim, flag, adapter, “keep for now”)
- [ ] Callers, tests, specs/docs moved together
- [ ] Mechanical prove (grep + compile + tests for the slice)
- [ ] Live classify (restartable host process vs signed guest/host image) when metal is in play
- [ ] Hostile audit (subagent that did not write the patch)
- [ ] Audit findings implemented; prove re-run
- [ ] Second audit: zero in-scope findings
- [ ] Signed roll only after the second audit, and only if the bytes live in a guest or host image
```

### 1. Name the SoT

One sentence: *this job is decided by type/module X; Y is illegal.* Prefer
enums, newtypes, trait bounds, and catalog/generated types so the compiler
rejects the old way. **No strings where a type would be safer.** No
`rsplit`/`split`/`contains` to recover identity the type already has. No
JSON/`Value` hops when a typed record exists. No one-implementor trait
hierarchy. No second loader/parser/planner for the same job.

### 2. Red-team the plan

Launch a **read-only** subagent (Task) that did not see your intended patch as
“done.” It returns findings only: remaining dual paths, stringly surfaces, spec
vs code disagreement, DRY breaks, style-guide misses, incomplete graph (tests
and docs still on the old way). Fix the plan. Do not implement the discarded
shape.

### 3. Implement the cutover

TDD: failing test that encodes the SoT (compile-fail is the right test when the
bug is “this should not type-check”). Then delete the old path. Collapse
abstractions. Put shared logic in one place. Match existing formatting; do not
invent a parallel style.

### 4. Prove (mechanical)

Build greps from the SoT you named, not from memory of another repo. Examples of
*classes* (instantiate per slice):

- String literals that name operations, resources, or modes the type already has
- Parse/split of those names at a trust boundary
- `legacy` / `compat` / `v1`/`v2` dual modules for the same job
- `serde_json::Value` / untyped maps at an API that has a struct
- Duplicate validation/serialization
- Spec/lock/plan still describing the deleted path

Run the slice’s compile and tests. Prove-greps still hitting in-scope code means
**not done**.

### 5. Hostile audit

New subagent. Give it the diff, the SoT sentence, the prove-greps, and the
repo’s AGENTS/style/spec skills. Instruct: *you did not write this; find
inconsistencies, leftover strings, dual paths, missing tests, spec drift, DRY
and style misses; do not implement.*

If it finds anything in scope → implement → prove → audit again.

**Second consecutive audit with zero in-scope findings** is the only green
light for this slice. Then, in campaign mode, inventory the next fork.

## Live apply (when the repo has metal / a running cluster)

Read that repo’s `AGENTS.md` for the concrete SSH/bake/roll recipe (Cloud:
`docs/internal/live-debug.md`). Do not invent a second lifecycle controller.

Add these boxes to the slice checklist when the change must run on live
machines. Do not mark the slice done with them open.

```
- [ ] Classified: restartable host process vs catalog guest vs extra-runtime IsolatedGuest image vs host image
- [ ] Restartable host process: cattle `cargo test` + musl build + SSH install (Development includes `fleet-agent`)
- [ ] Catalog guest (Development): cattle guest-image + `metal guest replace` — not a bake per compile
- [ ] Extra-runtime IsolatedGuest image (Development): loop-mount `/var/lib/cloud/images/…/rootfs.ext4`, replace `/sbin/init` from the bake script, `scp` to the other hosts — not `metal guest replace`, not jailer
- [ ] Host image / Production: two consecutive audits green, then one bake+roll
```

**Inner loop** is TDD and compile **on the live linux host** using that
repo’s packed toolchain (Cloud: D18 builder on `cloud-host-0`,
`docs/internal/live-debug.md`). Copying a Darwin-cross binary is not the
inner loop. **Catalog-guest apply** on Development is `metal guest replace`
(content-addressed overlay; not unsigned jailer copies). **Outer loop** for
host image and Production is the signed roll (Cloud: `metal release bake` +
`ClusterConverge`).

MUST NOT:

- Treat the signed roll as the compile/test inner loop
- Cross-compile linux crates on a developer Mac when a Ready linux host with
  the packed toolchain is available
- `rustup` / distro rustc on Ready (use the packed builder)
- Patch unsigned bytes into jailer (that is a second roll path; Development
  guest apply is `metal guest replace`)
- Skip the hostile audits because “we need to flash to find out”

SSH debug is not the lifecycle controller. If project docs say operators MAY
SSH to test fixes, that is the host-process inner loop — use it. Catalog
guests on Development apply through `metal guest replace`. Host-image and
Production still require signed `guest_rootfs` after the second audit.

## Campaign inventory

When the user wants the whole tree clean, start with a read-only explore pass
ranked by:

1. Trust boundary (authz, tenancy, crypto, wire admission)
2. Compiler cannot catch it (strings, runtime fallbacks, silent defaults)
3. Two authorities for one job (spec vs code, two CLIs, two journals)
4. Copy-paste that will diverge

Take **one** top item to a full slice loop. Do not parallelize five half-cutovers.
Re-inventory from the tree, not from the first list. Continue until a campaign
inventory returns nothing actionable.

Plans must be implementation specs (tasks, types, delete list), not discussion
of rejected options. If a plan drifted from the code, rewrite it against HEAD
before cutting.

## Forbidden stops

Do not:

- Ask the user to queue “red team” / “check completeness” / “keep going”
- End with leftover in-scope work labeled remaining / follow-up / later
- Treat “tests pass” as SoT if string dispatch still compiles
- Keep the old function “for callers”
- Add `#[deprecated]`, feature flags, or adapters instead of delete
- Soften a MUST in specs to protect a lock that describes the discarded path
- Declare the campaign done because the chat is long

You may pause only for a true blocker: missing secret, explicit user stop, or
an out-of-scope decision the repo forbids (do not implement a future engine the
docs say is not live). Say the blocker in one sentence and what is already cut.

## Done message

Slice: SoT sentence, what was deleted, prove-greps that are clean, tests run.
Campaign: slices completed this session, next inventory empty or the one
blocker. No laundry list of optional chores that are still this job.
