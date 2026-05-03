use serde::{Deserialize, Serialize};

use crate::providers::DatasetId;

macro_rules! non_empty_string_newtype {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Option<Self> {
                let value = value.into().trim().to_string();
                if value.is_empty() {
                    return None;
                }
                Some(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

non_empty_string_newtype!(FieldName);
non_empty_string_newtype!(DbtModelName);
non_empty_string_newtype!(ModelRelPath);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StagingModelName(DbtModelName);

impl StagingModelName {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into().trim().to_ascii_lowercase();
        if !value.starts_with("stg_") {
            return None;
        }
        DbtModelName::new(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GoldModelName(DbtModelName);

impl GoldModelName {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into().trim().to_ascii_lowercase();
        if value.is_empty() || value.starts_with("stg_") {
            return None;
        }
        DbtModelName::new(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum ModelInput {
    Staging(StagingModelName),
    IntraPlan(GoldModelName),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "dataset", rename_all = "snake_case")]
pub enum SemanticProfileKey {
    Dataset(DatasetRef),
    Global,
}

/// Canonical dataset reference in `<catalog>.<schema>.<table>` form.
///
/// This is the internal reference type used throughout the data_engineer suite.
/// [`DatasetId`] is the corresponding type on the provider trait boundary.
/// The middle segment is called `schema` here and `database` in `DatasetId`;
/// both refer to the same catalog-level namespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DatasetRef {
    pub catalog: String,
    pub schema: String,
    pub table: String,
}

impl DatasetRef {
    pub fn parse(raw: &str) -> Option<Self> {
        let parts: Vec<&str> = raw.trim().split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let catalog = parts[0].trim();
        let schema = parts[1].trim();
        let table = parts[2].trim();
        if catalog.is_empty() || schema.is_empty() || table.is_empty() {
            return None;
        }
        Some(Self {
            catalog: catalog.to_string(),
            schema: schema.to_string(),
            table: table.to_string(),
        })
    }

    pub fn fqn(&self) -> String {
        format!("{}.{}.{}", self.catalog, self.schema, self.table)
    }
}

impl From<DatasetId> for DatasetRef {
    fn from(id: DatasetId) -> Self {
        Self {
            catalog: id.catalog,
            schema: id.database,
            table: id.table,
        }
    }
}

impl From<DatasetRef> for DatasetId {
    fn from(r: DatasetRef) -> Self {
        Self {
            catalog: r.catalog,
            database: r.schema,
            table: r.table,
        }
    }
}

/// Canonical column reference bound to a dataset reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ColumnRef {
    pub dataset: DatasetRef,
    pub column: String,
}

impl ColumnRef {
    pub fn new(dataset: DatasetRef, column: impl Into<String>) -> Option<Self> {
        let column = column.into().trim().to_string();
        if column.is_empty() {
            return None;
        }
        Some(Self { dataset, column })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_ref_parses_canonical_fqn() {
        let ds = DatasetRef::parse("AwsDataCatalog.test_raw.raw_customers").expect("parse");
        assert_eq!(ds.catalog, "AwsDataCatalog");
        assert_eq!(ds.schema, "test_raw");
        assert_eq!(ds.table, "raw_customers");
        assert_eq!(ds.fqn(), "AwsDataCatalog.test_raw.raw_customers");
    }

    #[test]
    fn column_ref_rejects_empty_column() {
        let ds = DatasetRef::parse("AwsDataCatalog.test_raw.raw_customers").expect("parse");
        assert!(ColumnRef::new(ds, "   ").is_none());
    }

    #[test]
    fn model_name_newtypes_enforce_layer_prefixes() {
        assert!(StagingModelName::new("stg_orders").is_some());
        assert!(StagingModelName::new("orders").is_none());
        assert!(GoldModelName::new("fct_orders").is_some());
        assert!(GoldModelName::new("stg_orders").is_none());
    }
}
