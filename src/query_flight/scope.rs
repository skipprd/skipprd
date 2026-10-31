//! Logical tenant for Flight SQL sessions.

use skippr_lease::PathError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantScope {
    pub tenant: String,
    pub workspace: String,
}

impl TenantScope {
    pub fn new(tenant: impl Into<String>, workspace: impl Into<String>) -> Result<Self, PathError> {
        let tenant = tenant.into();
        let workspace = workspace.into();
        skippr_lease::validate_path_component(&tenant)?;
        skippr_lease::validate_path_component(&workspace)?;
        Ok(Self { tenant, workspace })
    }

    pub fn matches_pipeline(&self, key: &skippr_lease::PipelineKey) -> bool {
        key.tenant() == self.tenant && key.workspace() == self.workspace
    }

    pub fn basic_authorization(&self) -> String {
        use base64::Engine;
        let user = format!("{}/{}", self.tenant, self.workspace);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(user.as_bytes())
        )
    }
}
