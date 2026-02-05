#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestScope {
    pub tenant: String,
    pub workspace: String,
    pub project_id: String,
}
