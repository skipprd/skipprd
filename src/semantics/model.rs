#[derive(Clone, Debug)]
pub enum SemanticFieldRole {
    Id,
    Timestamp,
    Categorical,
    Metric,
    FreeText,
}

#[derive(Clone, Debug)]
pub struct SemanticField {
    pub name: String,
    pub role: SemanticFieldRole,
}

#[derive(Clone, Debug)]
pub struct SemanticModel {
    pub namespace: String,
    pub fields: Vec<SemanticField>,
}


