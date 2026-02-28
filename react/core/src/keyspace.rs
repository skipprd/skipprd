use crate::scope::RequestScope;

pub trait Keyspace: Send + Sync {
    fn threads_prefix(&self, scope: &RequestScope) -> String;
    fn thread_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String>;
    fn thread_state_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String>;
    /// Thread-scoped, small JSON artifacts (control state, repair state, summaries, etc).
    ///
    /// `artifact_id` must be a safe single path segment (no slashes, no `..`).
    fn thread_artifact_key(
        &self,
        scope: &RequestScope,
        thread_id: &str,
        artifact_id: &str,
    ) -> Result<String, String>;

    fn logs_prefix(&self, scope: &RequestScope) -> String;
    fn thread_log_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String>;

    fn catalog_key(&self, scope: &RequestScope, dataset_id: &str) -> String;
    fn semantic_key(&self, scope: &RequestScope, dataset_id: &str) -> String;
    fn stats_key(&self, scope: &RequestScope, dataset_id: &str) -> String;
    fn manifest_key(&self, scope: &RequestScope, dataset_id: &str) -> String;

    fn lancedb_uri(&self, scope: &RequestScope) -> String;
    fn global_dbt_examples_lancedb_uri(&self) -> String;

    fn dbt_prefix(&self, scope: &RequestScope) -> String;
    fn dbt_project_key(&self, scope: &RequestScope) -> String;
    fn dbt_models_prefix(&self, scope: &RequestScope) -> String;
    fn dbt_metrics_prefix(&self, scope: &RequestScope) -> String;
    fn dbt_target_prefix(&self, scope: &RequestScope) -> String;
}

#[derive(Clone, Debug)]
pub struct DefaultKeyspace {
    pub bucket: String,
}

impl DefaultKeyspace {
    pub fn new(bucket: String) -> Self {
        Self { bucket }
    }

    fn encode_key_component(s: &str) -> String {
        // Percent-encode anything outside a conservative safe set so dataset IDs like
        // "<catalog>.<db>.<table>" can be used in S3 keys without surprises.
        let mut out = String::with_capacity(s.len());
        for b in s.as_bytes() {
            let c = *b as char;
            let safe = c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'; // allow dots
            if safe {
                out.push(c);
            } else {
                out.push_str(&format!("%{:02X}", b));
            }
        }
        out
    }

    fn ensure_safe_segment(seg: &str) -> Result<(), String> {
        if seg.is_empty() {
            return Err("empty path segment".to_string());
        }
        if seg.contains("..") {
            return Err("path traversal '..' is not allowed".to_string());
        }
        if seg.contains('/') || seg.contains('\\') {
            return Err("path separators are not allowed in path segments".to_string());
        }
        Ok(())
    }
}

impl Keyspace for DefaultKeyspace {
    fn threads_prefix(&self, scope: &RequestScope) -> String {
        format!(
            "{}/{}/{}/threads",
            scope.tenant, scope.workspace, scope.project_id
        )
    }

    fn thread_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String> {
        Self::ensure_safe_segment(&scope.tenant)?;
        Self::ensure_safe_segment(&scope.workspace)?;
        Self::ensure_safe_segment(&scope.project_id)?;
        Self::ensure_safe_segment(thread_id)?;
        Ok(format!(
            "{}/{}/{}/threads/{}.json",
            scope.tenant, scope.workspace, scope.project_id, thread_id
        ))
    }

    fn thread_state_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String> {
        Self::ensure_safe_segment(&scope.tenant)?;
        Self::ensure_safe_segment(&scope.workspace)?;
        Self::ensure_safe_segment(&scope.project_id)?;
        Self::ensure_safe_segment(thread_id)?;
        Ok(format!(
            "{}/{}/{}/state/{}/state.json",
            scope.tenant, scope.workspace, scope.project_id, thread_id
        ))
    }

    fn thread_artifact_key(
        &self,
        scope: &RequestScope,
        thread_id: &str,
        artifact_id: &str,
    ) -> Result<String, String> {
        Self::ensure_safe_segment(&scope.tenant)?;
        Self::ensure_safe_segment(&scope.workspace)?;
        Self::ensure_safe_segment(&scope.project_id)?;
        Self::ensure_safe_segment(thread_id)?;
        Self::ensure_safe_segment(artifact_id)?;
        Ok(format!(
            "{}/{}/{}/threads/{}.{}.json",
            scope.tenant, scope.workspace, scope.project_id, thread_id, artifact_id
        ))
    }

    fn logs_prefix(&self, scope: &RequestScope) -> String {
        format!(
            "{}/{}/{}/logs",
            scope.tenant, scope.workspace, scope.project_id
        )
    }

    fn thread_log_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String> {
        Self::ensure_safe_segment(&scope.tenant)?;
        Self::ensure_safe_segment(&scope.workspace)?;
        Self::ensure_safe_segment(&scope.project_id)?;
        Self::ensure_safe_segment(thread_id)?;
        Ok(format!(
            "{}/{}/{}/logs/{}.log",
            scope.tenant, scope.workspace, scope.project_id, thread_id
        ))
    }

    fn catalog_key(&self, scope: &RequestScope, dataset_id: &str) -> String {
        let id = Self::encode_key_component(dataset_id);
        format!(
            "{}/{}/{}/catalog/{}.yaml",
            scope.tenant, scope.workspace, scope.project_id, id
        )
    }

    fn semantic_key(&self, scope: &RequestScope, dataset_id: &str) -> String {
        let id = Self::encode_key_component(dataset_id);
        format!(
            "{}/{}/{}/semantic/{}.yaml",
            scope.tenant, scope.workspace, scope.project_id, id
        )
    }

    fn stats_key(&self, scope: &RequestScope, dataset_id: &str) -> String {
        let id = Self::encode_key_component(dataset_id);
        format!(
            "{}/{}/{}/stats/{}.json",
            scope.tenant, scope.workspace, scope.project_id, id
        )
    }

    fn manifest_key(&self, scope: &RequestScope, dataset_id: &str) -> String {
        let filename = format!("{}.json", Self::encode_key_component(dataset_id));
        format!(
            "{}/{}/{}/manifest/{}",
            scope.tenant, scope.workspace, scope.project_id, filename
        )
    }

    fn lancedb_uri(&self, scope: &RequestScope) -> String {
        format!(
            "s3://{}/{}/{}/{}/lancedb",
            self.bucket, scope.tenant, scope.workspace, scope.project_id
        )
    }

    fn global_dbt_examples_lancedb_uri(&self) -> String {
        format!("s3://{}/dbt-examples/lancedb", self.bucket)
    }

    fn dbt_prefix(&self, scope: &RequestScope) -> String {
        format!(
            "{}/{}/{}/dbt/",
            scope.tenant, scope.workspace, scope.project_id
        )
    }

    fn dbt_project_key(&self, scope: &RequestScope) -> String {
        format!("{}dbt_project.yml", self.dbt_prefix(scope))
    }

    fn dbt_models_prefix(&self, scope: &RequestScope) -> String {
        format!("{}models/", self.dbt_prefix(scope))
    }

    fn dbt_metrics_prefix(&self, scope: &RequestScope) -> String {
        format!("{}metrics/", self.dbt_prefix(scope))
    }

    fn dbt_target_prefix(&self, scope: &RequestScope) -> String {
        format!("{}target/", self.dbt_prefix(scope))
    }
}

/// Local filesystem keyspace.
///
/// Mirrors the default key layout, but produces local LanceDB URIs.
#[derive(Clone, Debug)]
pub struct LocalKeyspace {
    pub root_dir: String,
}

impl LocalKeyspace {
    pub fn new(root_dir: String) -> Self {
        Self { root_dir }
    }

    fn file_uri(&self, rel: &str) -> String {
        let root = self.root_dir.trim_end_matches('/');
        if rel.is_empty() {
            format!("file://{}", root)
        } else {
            format!("file://{}/{}", root, rel.trim_start_matches('/'))
        }
    }
}

impl Keyspace for LocalKeyspace {
    fn threads_prefix(&self, scope: &RequestScope) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .threads_prefix(scope)
    }

    fn thread_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String> {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .thread_key(scope, thread_id)
    }

    fn thread_state_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String> {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .thread_state_key(scope, thread_id)
    }

    fn thread_artifact_key(
        &self,
        scope: &RequestScope,
        thread_id: &str,
        artifact_id: &str,
    ) -> Result<String, String> {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .thread_artifact_key(scope, thread_id, artifact_id)
    }

    fn logs_prefix(&self, scope: &RequestScope) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .logs_prefix(scope)
    }

    fn thread_log_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String> {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .thread_log_key(scope, thread_id)
    }

    fn catalog_key(&self, scope: &RequestScope, dataset_id: &str) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .catalog_key(scope, dataset_id)
    }

    fn semantic_key(&self, scope: &RequestScope, dataset_id: &str) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .semantic_key(scope, dataset_id)
    }

    fn stats_key(&self, scope: &RequestScope, dataset_id: &str) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .stats_key(scope, dataset_id)
    }

    fn manifest_key(&self, scope: &RequestScope, dataset_id: &str) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .manifest_key(scope, dataset_id)
    }

    fn lancedb_uri(&self, scope: &RequestScope) -> String {
        // Path layout (relative): <tenant>/<workspace>/<project_id>/lancedb
        self.file_uri(&format!(
            "{}/{}/{}/lancedb",
            scope.tenant, scope.workspace, scope.project_id
        ))
    }

    fn global_dbt_examples_lancedb_uri(&self) -> String {
        self.file_uri("dbt-examples/lancedb")
    }

    fn dbt_prefix(&self, scope: &RequestScope) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .dbt_prefix(scope)
    }

    fn dbt_project_key(&self, scope: &RequestScope) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .dbt_project_key(scope)
    }

    fn dbt_models_prefix(&self, scope: &RequestScope) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .dbt_models_prefix(scope)
    }

    fn dbt_metrics_prefix(&self, scope: &RequestScope) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .dbt_metrics_prefix(scope)
    }

    fn dbt_target_prefix(&self, scope: &RequestScope) -> String {
        DefaultKeyspace {
            bucket: "".to_string(),
        }
        .dbt_target_prefix(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyspace_rejects_bad_segments() {
        let ks = DefaultKeyspace::new("b".to_string());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        assert!(ks.thread_key(&scope, "../x").is_err());
        assert!(ks.thread_key(&scope, "a/b").is_err());
        assert!(ks.thread_key(&scope, "").is_err());
        assert!(ks.thread_artifact_key(&scope, "123", "../x").is_err());
        assert!(ks.thread_artifact_key(&scope, "123", "a/b").is_err());
        assert!(ks.thread_artifact_key(&scope, "123", "").is_err());
    }

    #[test]
    fn keyspace_builds_thread_key() {
        let ks = DefaultKeyspace::new("b".to_string());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let k = ks.thread_key(&scope, "123").unwrap();
        assert_eq!(k, "t/w/p/threads/123.json");
    }

    #[test]
    fn keyspace_builds_thread_state_key() {
        let ks = DefaultKeyspace::new("b".to_string());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let k = ks.thread_state_key(&scope, "123").unwrap();
        assert_eq!(k, "t/w/p/state/123/state.json");
    }

    #[test]
    fn keyspace_builds_thread_log_key() {
        let ks = DefaultKeyspace::new("b".to_string());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let k = ks.thread_log_key(&scope, "123").unwrap();
        assert_eq!(k, "t/w/p/logs/123.log");
    }

    #[test]
    fn keyspace_builds_thread_artifact_key() {
        let ks = DefaultKeyspace::new("b".to_string());
        let scope = RequestScope {
            tenant: "t".into(),
            workspace: "w".into(),
            project_id: "p".into(),
        };
        let k = ks.thread_artifact_key(&scope, "123", "control").unwrap();
        assert_eq!(k, "t/w/p/threads/123.control.json");
    }
}
