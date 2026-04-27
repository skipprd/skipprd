use crate::providers::DatasetId;
use crate::references::DatasetRef;
use react_core::agent::AgentCtx;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeTarget {
    pub dataset: DatasetRef,
    pub field: Option<String>,
}

impl ProbeTarget {
    pub fn canonical_table(ctx: &AgentCtx, raw: &str) -> Result<DatasetRef, String> {
        let table = raw.trim();
        if table.is_empty() {
            return Err("missing_table".to_string());
        }
        let wh = crate::ctx_ext::actx_warehouse(ctx)
            .ok_or_else(|| "warehouse_provider_missing".to_string())?;
        let parsed: DatasetId = wh
            .parse_dataset_fqn(table)
            .map_err(|_| "invalid_table_format".to_string())?;
        DatasetRef::parse(&parsed.fqn()).ok_or_else(|| "invalid_table_format".to_string())
    }

    pub fn from_table_and_field(
        ctx: &AgentCtx,
        raw_table: &str,
        raw_field: Option<&str>,
    ) -> Result<Self, String> {
        let dataset = Self::canonical_table(ctx, raw_table)?;
        let field = raw_field
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(Self { dataset, field })
    }

    pub fn table_fqn(&self) -> String {
        self.dataset.fqn()
    }
}
