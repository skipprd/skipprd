# Skippr patch on iceberg 0.7.0

Upstream `iceberg` 0.7.0 exposes `Transaction::fast_append()` only. MoR equality-delete commits
(ReplacePartition, MergeByKey, CDC final-state deletes) require data plus delete manifests in one
snapshot.

This vendored copy adds:

- `SnapshotProducer::new_with_deletes()` and delete-manifest writing
- `Transaction::equality_delta_append()` (`EqualityDeltaAppendAction`)

Wired from `skippr-plugin-data-sink-iceberg` via `[patch.crates-io]` in the workspace root
`Cargo.toml`.
