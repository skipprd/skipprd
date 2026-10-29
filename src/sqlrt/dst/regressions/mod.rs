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
fn huge_dst1_op_count_is_not_a_schedule() {
    // cargo-fuzz world_schedule crash-7fd1adbc7c3eaf93b76ed47fe59c65e61de266be
    let bytes: &[u8] = &[
        68, 83, 84, 49, 5, 0, 0, 251, 0, 1, 0, 0, 0, 0, 0, 0, 0, 4, 0, 107, 101, 112, 116, 1, 0, 0,
        0, 0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 1, 2,
        0, 0, 0, 0, 0, 0, 0, 6, 0, 112, 97, 117, 115, 101, 100, 2, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 1, 3, 0, 0, 0, 0, 0, 0, 0, 5, 0,
        97, 102, 116, 101, 114, 3, 0, 0, 0, 0, 0, 0, 0, 1, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
    ];
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        0xfb000005
    );
    assert!(super::schedule_from_bytes(bytes).is_empty());
}

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
