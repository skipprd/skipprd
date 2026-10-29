---
name: skippr-fuzz
description: >-
  Timeboxed cargo fuzz of skipprd codecs and DST World Op schedules. Timeout
  fails. Use when the user says /skippr-fuzz, skippr-fuzz, or asks to fuzz
  WAL/catalog/COPY frames or DST schedules.
disable-model-invocation: true
---

# skippr-fuzz

Opt-in. GitHub workflow `.github/workflows/skippr-fuzz.yml` is `workflow_dispatch` only. Timeout fails the job (`continue-on-error` is illegal). Untrusted `DST1` decode fails closed when op count or CompactThru/WalEvent.seq/ordinal/row_i exceeds 1024. Shrinking crashes become named scripts under `src/sqlrt/dst/regressions/`. Do **not** run skipprd-docs. Do **not** publish. Do **not** write CloudCycle EU buckets.

Targets (production parsers / World checker, never a second oracle):

- `world_schedule` — bytes → `Vec<Op>` → `current_table`
- `copy_frames` — `decode_payload::<HostFrame>` and `PluginFrame`
- `failpoint_names` — `FailpointName::parse` (unknown fails closed)
- `alter_sql` — `SParser`
- `wal_slices` — `decode_wal_slices` and `SegmentFile` SEGF header
- `catalog_row` — `CatalogRow` bincode + `CatalogTxn::put`
- `iceberg_metadata` — Iceberg `TableMetadata` JSON + `skippr.wal-slices`
- `discover_json` — flatten documents
- `migrate_journal` — ALTER SQL + checksum
- `pipeline_yaml` — `Config` YAML
- `checkpoint_envelope` — checkpoint bincode
- `order_tokens` — collapse WAL tokens

Corpus seeds live under `fuzz/corpus/<target>/`. Seed World schedules from named DST scripts.

## Loop

```
skippr-fuzz:
- [ ] Local: cargo +nightly fuzz run world_schedule --fuzz-dir fuzz -- -max_total_time=30
- [ ] gh workflow run skippr-fuzz.yml --ref this branch
- [ ] Watch until success; timeout is red
- [ ] New crash → shrink → named script under `src/sqlrt/dst/regressions/`
```

```bash
cargo +nightly install cargo-fuzz
cargo +nightly fuzz run world_schedule --fuzz-dir fuzz -- -max_total_time=30
gh workflow run skippr-fuzz.yml --ref "$(git branch --show-current)"
```

Laptop timeboxes are for iteration. The SHA battery is the GitHub workflow on this SHA.
