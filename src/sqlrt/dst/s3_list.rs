#[test]
fn j_s3_list_closed_skip() {
    let page = vec![
        ("closed/a".into(), 10),
        ("open/b".into(), 20),
        ("closed/c".into(), 30),
        ("open/d".into(), 40),
    ];
    let get = skippr_runtime_sdk::source_sync::collect_open_list_keys(
        page,
        vec![false, true, false, true],
    );
    assert_eq!(get.first().map(|(key, _)| key.as_str()), Some("open/b"));
    assert_eq!(get, vec![("open/b".into(), 20), ("open/d".into(), 40)]);
}
