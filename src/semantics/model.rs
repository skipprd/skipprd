use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SemanticFieldRole {
    Id,
    Timestamp,
    Categorical,
    Metric,
    FreeText,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticField {
    pub name: String,
    pub role: SemanticFieldRole,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SemanticModel {
    pub namespace: String,
    pub fields: Vec<SemanticField>,
    #[serde(default)]
    pub dimensions: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<String>,
}

// Data catalog artifacts (separate from semantic model, but linked by namespace and field names)
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogField {
    pub entity: String,
    pub name: String,
    pub description: Option<String>,
    pub synonyms: Option<Vec<String>>,
    pub pii_sensitivity: Option<String>,
    pub units_or_format: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataCatalog {
    pub namespace: String,
    pub description: Option<String>,
    pub fields: Vec<CatalogField>,
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_round_trip_semantic_and_catalog() {
        let sem = SemanticModel {
            namespace: "ns".into(),
            fields: vec![
                SemanticField { name: "id".into(), role: SemanticFieldRole::Id },
                SemanticField { name: "ts".into(), role: SemanticFieldRole::Timestamp },
            ],
            dimensions: vec!["id".into()],
            metrics: vec![],
        };
        let y = serde_yaml::to_string(&sem).unwrap();
        let back: SemanticModel = serde_yaml::from_str(&y).unwrap();
        assert_eq!(sem, back);

        let cat = DataCatalog {
            namespace: "ns".into(),
            fields: vec![
                CatalogField { entity: "".into(), name: "id".into(), description: Some("identifier".into()), synonyms: Some(vec!["key".into()]), pii_sensitivity: None, units_or_format: None }
            ],
        };
        let y2 = serde_yaml::to_string(&cat).unwrap();
        let back2: DataCatalog = serde_yaml::from_str(&y2).unwrap();
        assert_eq!(cat, back2);
    }

    #[test]
    fn link_integrity_catalog_fields_exist_in_semantic() {
        let sem = SemanticModel { namespace: "ns".into(), fields: vec![SemanticField { name: "a".into(), role: SemanticFieldRole::Id }], dimensions: vec!["a".into()], metrics: vec![] };
        let cat = DataCatalog { namespace: "ns".into(), fields: vec![CatalogField { entity: "".into(), name: "a".into(), description: None, synonyms: None, pii_sensitivity: None, units_or_format: None }] };
        let sem_names: std::collections::HashSet<String> = sem.fields.iter().map(|f| f.name.clone()).collect();
        for f in cat.fields.iter() {
            assert!(sem_names.contains(&f.name));
        }
    }
}


