use assert_cmd::prelude::*;
use std::fs;
use std::process::Command;

#[test]
fn semantic_and_catalog_tables_register_and_query() {
    if std::env::var("SKIPPR_SEMANTIC_CATALOG_TEST")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    use std::process::Command;
    std::env::set_var("SKIPPR_OFFLINE", "true");
    std::env::set_var("PIPELINE_NAME", "bike_hire5");

    // Write minimal semantic and catalog for a namespace into default data dir
    let cache = format!("{}/catalog_cache", "./data");
    let _ = std::fs::create_dir_all(&cache);
    let sem_y = r#"namespace: bike_hire5
fields:
  - { name: ride_id, role: Id }
  - { name: event_date, role: Timestamp }
"#;
    let cat_y = r#"namespace: bike_hire5
fields:
  - { entity: "", name: ride_id, description: "", synonyms: ["r_id"] }
  - { entity: "", name: event_date, description: "", synonyms: ["ts"] }
"#;
    std::fs::write(format!("{}/{}_semantic.yaml", cache, "bike_hire5"), sem_y).unwrap();
    std::fs::write(format!("{}/{}_catalog.yaml", cache, "bike_hire5"), cat_y).unwrap();

    let out = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "query",
            "--plain",
            "--sql",
            "SHOW SEMANTIC FOR bike_hire5.bike_hire5",
        ])
        .output()
        .expect("failed to run query");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ride_id") && stdout.contains("event_date"));

    let out2 = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "query",
            "--plain",
            "--sql",
            "SHOW CATALOG FOR bike_hire5.bike_hire5",
        ])
        .output()
        .expect("failed to run query");
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("ride_id") && stdout2.contains("event_date"));
}
