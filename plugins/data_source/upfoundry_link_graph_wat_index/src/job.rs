use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatIndexJobMessage {
    #[serde(rename = "crawlId")]
    pub crawl_id: String,
    #[serde(rename = "manifestUri", default)]
    pub manifest_uri: Option<String>,
    #[serde(rename = "resetCheckpoint", default)]
    pub reset_checkpoint: bool,
    #[serde(rename = "watPathStart", default)]
    pub wat_path_start: Option<usize>,
    #[serde(rename = "watPathEnd", default)]
    pub wat_path_end: Option<usize>,
    #[serde(rename = "maxWatObjectsPerSync", default)]
    pub max_wat_objects_per_sync: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatManifestCheckpoint {
    pub crawl_id: String,
    pub manifest_uri: String,
    pub next_path_index: usize,
    pub total_paths: Option<usize>,
    #[serde(default)]
    pub cleared: bool,
}

pub fn effective_checkpoint(
    checkpoint: Option<WatManifestCheckpoint>,
    reset_checkpoint: bool,
) -> Option<WatManifestCheckpoint> {
    if reset_checkpoint {
        return None;
    }
    checkpoint.filter(|cp| !cp.cleared)
}

pub fn cleared_checkpoint(crawl_id: &str, manifest_uri: &str) -> WatManifestCheckpoint {
    WatManifestCheckpoint {
        crawl_id: crawl_id.to_string(),
        manifest_uri: manifest_uri.to_string(),
        next_path_index: 0,
        total_paths: None,
        cleared: true,
    }
}

pub fn checkpoint_key(crawl_id: &str) -> String {
    format!("wat_manifest:{crawl_id}")
}

pub fn default_manifest_uri(crawl_id: &str) -> String {
    format!("https://data.commoncrawl.org/crawl-data/{crawl_id}/wat.paths.gz")
}

pub fn fifo_deduplication_id(crawl_id: &str) -> String {
    crawl_id
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
        .chars()
        .take(128)
        .collect()
}

pub fn apply_path_cap(paths: &mut Vec<String>, max_wat_objects_per_sync: Option<usize>) {
    if let Some(limit) = max_wat_objects_per_sync {
        paths.truncate(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_job_message() {
        let job: WatIndexJobMessage = serde_json::from_str(
            r#"{"crawlId":"CC-MAIN-2025-08","manifestUri":"https://example/wat.paths.gz"}"#,
        )
        .unwrap();
        assert_eq!(job.crawl_id, "CC-MAIN-2025-08");
        assert_eq!(
            job.manifest_uri.as_deref(),
            Some("https://example/wat.paths.gz")
        );
    }

    #[test]
    fn path_cap_none_keeps_all() {
        let mut paths = vec!["a".into(), "b".into(), "c".into()];
        apply_path_cap(&mut paths, None);
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn path_cap_some_truncates() {
        let mut paths = vec!["a".into(), "b".into(), "c".into()];
        apply_path_cap(&mut paths, Some(2));
        assert_eq!(paths, vec!["a", "b"]);
    }

    #[test]
    fn dedupe_id_normalizes_crawl_id() {
        assert_eq!(
            fifo_deduplication_id("CC-MAIN-2025-08"),
            "cc_main_2025_08"
        );
    }

    #[test]
    fn parses_reset_checkpoint_flag() {
        let job: WatIndexJobMessage = serde_json::from_str(
            r#"{"crawlId":"CC-MAIN-2025-08","resetCheckpoint":true,"watPathStart":0,"watPathEnd":50,"maxWatObjectsPerSync":25}"#,
        )
        .unwrap();
        assert!(job.reset_checkpoint);
        assert_eq!(job.wat_path_start, Some(0));
        assert_eq!(job.wat_path_end, Some(50));
        assert_eq!(job.max_wat_objects_per_sync, Some(25));
    }

    #[test]
    fn effective_checkpoint_honors_reset_and_cleared() {
        let checkpoint = WatManifestCheckpoint {
            crawl_id: "CC-MAIN-2025-08".into(),
            manifest_uri: "https://example/wat.paths.gz".into(),
            next_path_index: 99,
            total_paths: Some(100),
            cleared: false,
        };
        assert!(effective_checkpoint(Some(checkpoint.clone()), true).is_none());
        assert_eq!(
            effective_checkpoint(Some(checkpoint.clone()), false),
            Some(checkpoint.clone())
        );
        assert!(
            effective_checkpoint(
                Some(WatManifestCheckpoint {
                    cleared: true,
                    ..checkpoint
                }),
                false
            )
            .is_none()
        );
    }
}
