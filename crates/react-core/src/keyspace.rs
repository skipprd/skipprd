use crate::scope::RequestScope;

pub trait Keyspace: Send + Sync {
    fn threads_prefix(&self, scope: &RequestScope) -> String;
    fn thread_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String>;
    fn thread_state_key(&self, scope: &RequestScope, thread_id: &str) -> Result<String, String>;

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
            "{}/{}/{}/threads/{}.state.json",
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
        assert_eq!(k, "t/w/p/threads/123.state.json");
    }
}
