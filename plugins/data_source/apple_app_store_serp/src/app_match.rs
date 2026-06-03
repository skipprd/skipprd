use serde::Deserialize;
use serde_derive::Serialize;

use crate::config::TargetEntry;
use crate::itunes::AppResultRow;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetMatchRow {
    pub target_app_id: String,
    pub matched_app_id: Option<String>,
    pub matched_bundle_id: Option<String>,
    pub matched_track_name: Option<String>,
    pub position: Option<u32>,
    pub found: bool,
}

fn normalize_app_id(id: &str) -> String {
    id.trim().to_string()
}

fn app_id_matches(candidate: &str, target_id: &str) -> bool {
    normalize_app_id(candidate) == normalize_app_id(target_id)
}

fn bundle_matches(candidate: Option<&str>, target_bundle: Option<&str>) -> bool {
    match (candidate, target_bundle) {
        (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
        _ => false,
    }
}

fn find_match_for_target(target: &TargetEntry, results: &[AppResultRow]) -> TargetMatchRow {
    let mut ids_to_match: Vec<&str> = vec![target.app_id.as_str()];
    for alias in &target.aliases {
        if !alias.trim().is_empty() {
            ids_to_match.push(alias.as_str());
        }
    }

    for row in results {
        for id in &ids_to_match {
            if app_id_matches(&row.app_id, id) {
                return TargetMatchRow {
                    target_app_id: target.app_id.clone(),
                    matched_app_id: Some(row.app_id.clone()),
                    matched_bundle_id: row.bundle_id.clone(),
                    matched_track_name: row.track_name.clone(),
                    position: Some(row.position),
                    found: true,
                };
            }
        }
        if let Some(bundle) = target.bundle_id.as_deref() {
            if bundle_matches(row.bundle_id.as_deref(), Some(bundle)) {
                return TargetMatchRow {
                    target_app_id: target.app_id.clone(),
                    matched_app_id: Some(row.app_id.clone()),
                    matched_bundle_id: row.bundle_id.clone(),
                    matched_track_name: row.track_name.clone(),
                    position: Some(row.position),
                    found: true,
                };
            }
        }
    }

    TargetMatchRow {
        target_app_id: target.app_id.clone(),
        matched_app_id: None,
        matched_bundle_id: None,
        matched_track_name: None,
        position: None,
        found: false,
    }
}

pub fn match_targets(
    targets: &[TargetEntry],
    results: &[AppResultRow],
    stop_after_first: bool,
) -> Vec<TargetMatchRow> {
    let mut out = Vec::new();
    for target in targets {
        let row = find_match_for_target(target, results);
        let found = row.found;
        out.push(row);
        if stop_after_first && found {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TargetEntry;

    fn sample_results() -> Vec<AppResultRow> {
        vec![
            AppResultRow {
                position: 1,
                app_id: "111".into(),
                bundle_id: Some("com.other".into()),
                track_name: Some("Other".into()),
                artist_name: Some("Dev".into()),
            },
            AppResultRow {
                position: 3,
                app_id: "123456789".into(),
                bundle_id: Some("com.example.app".into()),
                track_name: Some("Example App".into()),
                artist_name: Some("Example Inc".into()),
            },
        ]
    }

    #[test]
    fn matches_track_id() {
        let targets = vec![TargetEntry {
            app_id: "123456789".into(),
            bundle_id: None,
            aliases: vec![],
        }];
        let matches = match_targets(&targets, &sample_results(), false);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].found);
        assert_eq!(matches[0].position, Some(3));
    }

    #[test]
    fn matches_bundle_id_fallback() {
        let targets = vec![TargetEntry {
            app_id: "999".into(),
            bundle_id: Some("com.example.app".into()),
            aliases: vec![],
        }];
        let matches = match_targets(&targets, &sample_results(), false);
        assert!(matches[0].found);
        assert_eq!(matches[0].position, Some(3));
    }

    #[test]
    fn not_found_emits_absent_row() {
        let targets = vec![TargetEntry {
            app_id: "000".into(),
            bundle_id: None,
            aliases: vec![],
        }];
        let matches = match_targets(&targets, &sample_results(), false);
        assert!(!matches[0].found);
        assert!(matches[0].position.is_none());
    }
}
