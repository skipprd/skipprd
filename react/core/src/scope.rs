#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantId(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceId(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectId(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestScope {
    pub tenant: String,
    pub workspace: String,
    pub project_id: String,
}

impl TenantId {
    pub fn parse(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        ensure_safe_scope_segment("tenant", &raw)?;
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl WorkspaceId {
    pub fn parse(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        ensure_safe_scope_segment("workspace", &raw)?;
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ProjectId {
    pub fn parse(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        ensure_safe_scope_segment("project_id", &raw)?;
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl RequestScope {
    pub fn new(tenant: TenantId, workspace: WorkspaceId, project_id: ProjectId) -> Self {
        Self {
            tenant: tenant.0,
            workspace: workspace.0,
            project_id: project_id.0,
        }
    }

    pub fn parse(
        tenant: impl Into<String>,
        workspace: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Result<Self, String> {
        Ok(Self::new(
            TenantId::parse(tenant)?,
            WorkspaceId::parse(workspace)?,
            ProjectId::parse(project_id)?,
        ))
    }
}

pub fn ensure_safe_scope_segment(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("invalid {field}: empty segment"));
    }
    if value.contains("..") {
        return Err(format!("invalid {field}: path traversal '..' is not allowed"));
    }
    if value.contains('/') || value.contains('\\') {
        return Err(format!(
            "invalid {field}: path separators are not allowed in scope segments"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scope_accepts_safe_segments() {
        let scope = RequestScope::parse("tenant1", "workspace_1", "project-1")
            .expect("safe segments should parse");
        assert_eq!(scope.tenant, "tenant1");
        assert_eq!(scope.workspace, "workspace_1");
        assert_eq!(scope.project_id, "project-1");
    }

    #[test]
    fn parse_scope_rejects_path_segments() {
        let err = RequestScope::parse("tenant/../x", "w", "p")
            .expect_err("unsafe scope should fail");
        assert!(err.contains("invalid tenant"));
    }
}
