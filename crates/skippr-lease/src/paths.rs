use crate::error::PathError;
use crate::identity::{PipelineKey, SegmentId};
use std::path::{Path, PathBuf};

pub fn validate_path_component(value: &str) -> Result<(), PathError> {
    if value.is_empty() {
        return Err(PathError::InvalidComponent(
            "path component must not be empty".into(),
        ));
    }
    if value == "." || value == ".." {
        return Err(PathError::InvalidComponent(format!(
            "path component '{value}' is not allowed"
        )));
    }
    if value.contains('/') || value.contains('\\') || value.contains('\0') {
        return Err(PathError::InvalidComponent(format!(
            "path component '{value}' contains a reserved character"
        )));
    }
    Ok(())
}

pub fn encode_path_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct PipelinePaths {
    pub root: PathBuf,
    pub segs: PathBuf,
    pub completions: PathBuf,
    pub compactions: PathBuf,
    pub durable: PathBuf,
    pub snapshots: PathBuf,
}

impl PipelinePaths {
    pub fn new(data_root: &Path, key: &PipelineKey) -> Result<Self, PathError> {
        validate_path_component(key.tenant())?;
        validate_path_component(key.workspace())?;
        validate_path_component(key.pipeline())?;
        let root = data_root
            .join("clustered")
            .join(encode_path_component(key.tenant()))
            .join(encode_path_component(key.workspace()))
            .join(encode_path_component(key.pipeline()));
        Ok(Self::from_pipeline_root(root))
    }

    /// Layout under an already-resolved pipeline root: `{root}/segment_buffer/...`.
    pub fn from_pipeline_root(root: PathBuf) -> Self {
        Self {
            segs: root.join("segment_buffer/segs"),
            completions: root.join("segment_buffer/done"),
            compactions: root.join("segment_buffer/compactions"),
            durable: root.join("segment_buffer/durable"),
            snapshots: root.join("segment_buffer/durable/snapshots"),
            root,
        }
    }

    /// Legacy `WAL_STORAGE=disk` layout: `{DATA_DIR}/segment_buffer/...`.
    /// Clustered mode must keep using [`PipelinePaths::new`].
    pub fn legacy_disk(data_root: &Path) -> Self {
        Self::from_pipeline_root(data_root.to_path_buf())
    }

    pub fn segment(&self, id: &SegmentId) -> PathBuf {
        self.segs.join(format!("{}.seg", id.as_str()))
    }

    pub fn segment_commit(&self, id: &SegmentId) -> PathBuf {
        self.segs.join(format!("{}.seg.commit", id.as_str()))
    }

    pub fn mutation_log(&self) -> PathBuf {
        self.durable.join("mutation.log")
    }

    pub fn state_file(&self) -> PathBuf {
        self.durable.join("STATE")
    }

    pub fn offset_published_file(&self) -> PathBuf {
        self.durable.join("OFFSET_PUBLISHED")
    }

    pub fn format_marker(&self) -> PathBuf {
        self.durable.join("CLUSTER_FORMAT_V1")
    }

    pub fn snapshot_current(&self) -> PathBuf {
        self.snapshots.join("current")
    }

    pub fn wal_uri(&self, key: &PipelineKey, segment_id: &SegmentId) -> String {
        format!(
            "wal://{}/{}/{}/{}",
            key.tenant(),
            key.workspace(),
            key.pipeline(),
            segment_id.as_str()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::PipelineKey;
    use std::path::Path;

    #[test]
    fn clustered_paths_are_tenant_scoped() {
        let key = PipelineKey::new("acme", "prod", "events").unwrap();
        let paths = PipelinePaths::new(Path::new("/data"), &key).unwrap();
        assert_eq!(paths.root, Path::new("/data/clustered/acme/prod/events"));
        assert!(paths.segs.ends_with("segment_buffer/segs"));
    }

    #[test]
    fn encode_preserves_safe_names_and_escapes_the_rest() {
        assert_eq!(encode_path_component("events"), "events");
        assert_eq!(encode_path_component("a b"), "a%20b");
    }

    #[test]
    fn rejects_path_separators() {
        assert!(PipelineKey::new("acme", "prod", "a/b").is_err());
        assert!(validate_path_component("..").is_err());
    }

    #[test]
    fn legacy_disk_keeps_workspace_pipeline_root() {
        let paths = PipelinePaths::legacy_disk(Path::new("/data/acme/prod/events"));
        assert_eq!(paths.root, Path::new("/data/acme/prod/events"));
        assert_eq!(
            paths.segs,
            Path::new("/data/acme/prod/events/segment_buffer/segs")
        );
        assert!(!paths.root.starts_with("/data/clustered"));
    }
}
