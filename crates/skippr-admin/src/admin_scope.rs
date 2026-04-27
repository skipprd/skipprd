use react_core::scope::{ProjectId, RequestScope, WorkspaceId};

pub const ADMIN_WORKSPACE: &str = "skippr-admin";

pub fn derive_admin_scope(target: &RequestScope) -> Result<RequestScope, String> {
    let workspace = WorkspaceId::parse(ADMIN_WORKSPACE).map_err(|e| e.to_string())?;
    let project_id = ProjectId::parse(format!(
        "{}--{}",
        target.workspace.as_str(),
        target.project_id.as_str()
    ))
    .map_err(|e| e.to_string())?;
    Ok(RequestScope::new(
        target.tenant.clone(),
        workspace,
        project_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_admin_scope_from_target_scope() {
        let target = RequestScope::parse("tenant-a", "workspace-a", "project-a").expect("scope");
        let admin = derive_admin_scope(&target).expect("admin scope");
        assert_eq!(admin.tenant.as_str(), "tenant-a");
        assert_eq!(admin.workspace.as_str(), "skippr-admin");
        assert_eq!(admin.project_id.as_str(), "workspace-a--project-a");
    }
}
