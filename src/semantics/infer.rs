use super::model::{SemanticField, SemanticFieldRole, SemanticModel};

/// Stub inference from schema+stats. To be implemented.
pub fn infer_semantic_model(namespace: &str) -> SemanticModel {
    SemanticModel {
        namespace: namespace.to_string(),
        fields: Vec::new(),
    }
}


