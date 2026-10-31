use crate::failpoint::{failpoint_file, hit, FailpointName, BEFORE_CATALOG_COMMIT};

#[test]
fn known_failpoint_names_parse() {
    for name in FailpointName::ALL {
        assert_eq!(FailpointName::parse(name.as_str()).unwrap(), name);
    }
}

#[test]
fn unknown_failpoint_fails_closed() {
    let err = FailpointName::parse("not_a_failpoint").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown failpoint"), "{msg}");
}

#[test]
fn c_failpoint_file_is_single_shot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(failpoint_file(dir.path()), BEFORE_CATALOG_COMMIT).unwrap();
    assert!(hit(FailpointName::BeforeCatalogCommit, dir.path()).is_err());
    assert!(!failpoint_file(dir.path()).exists());
    assert!(hit(FailpointName::BeforeCatalogCommit, dir.path()).is_ok());
}
