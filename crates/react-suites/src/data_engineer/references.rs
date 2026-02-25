use serde::{Deserialize, Serialize};

/// Canonical dataset reference in `<catalog>.<schema>.<table>` form.
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
}
