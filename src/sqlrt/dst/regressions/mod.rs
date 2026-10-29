//! Shrunk cargo-fuzz crashes become named World scripts in [`super::scripts`].

use std::collections::BTreeSet;

const LANDED: &[&str] = &[
    "a_crash_before_catalog_drops_ingest",
    "a_crash_after_catalog_republishes_offsets",
    "a_deadletter_unreadable_json_does_not_advance_offset",
    "c_crash_before_catalog_commit",
    "c_after_catalog_before_offsets",
    "c_after_offsets_published_crash_reopen",
    "c_before_compaction_sink_aborts_at_boundary",
    "c_release_compaction_then_compact",
];

#[test]
fn regressions_land_as_named_scripts() {
    let names: BTreeSet<_> = super::scripts::all_scripts()
        .into_iter()
        .map(|s| s.name)
        .collect();
    for name in LANDED {
        assert!(names.contains(name), "missing fuzz regression {name}");
    }
}
