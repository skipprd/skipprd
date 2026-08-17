use std::any::Any;
use std::sync::Arc;

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;
use skippr_lease::PipelineKey;
use skippr_query_ballista::FlightSqlExec;

/// Remote Flight SQL leaf. Does not collect batches in `scan`.
#[derive(Debug)]
pub struct FlightSqlTableProvider {
    pub endpoint: String,
    pub sql: String,
    schema: SchemaRef,
    pub(crate) authorization: String,
}

impl FlightSqlTableProvider {
    pub fn new(endpoint: String, sql: String, schema: SchemaRef) -> Self {
        Self::with_session(endpoint, sql, schema, String::new())
    }

    pub fn with_session(
        endpoint: String,
        sql: String,
        schema: SchemaRef,
        authorization: String,
    ) -> Self {
        Self {
            endpoint,
            sql,
            schema,
            authorization,
        }
    }

    pub fn live_wal(
        endpoint: String,
        pipeline: &PipelineKey,
        namespace: &str,
        exclude_segment_ids: &[String],
        schema: SchemaRef,
        authorization: String,
    ) -> Self {
        Self {
            endpoint,
            sql: crate::query_flight::sql::live_wal_scan_sql(
                pipeline,
                namespace,
                exclude_segment_ids,
            ),
            schema,
            authorization,
        }
    }

    pub fn iceberg_scan(
        endpoint: String,
        namespace: &str,
        schema: SchemaRef,
        authorization: String,
    ) -> Self {
        Self::with_session(
            endpoint,
            crate::query_flight::sql::iceberg_scan_sql(namespace),
            schema,
            authorization,
        )
    }
}

#[async_trait::async_trait]
impl TableProvider for FlightSqlTableProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::View
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if self.endpoint.is_empty() {
            return Err(DataFusionError::Plan(
                "pipeline has no ready Flight SQL endpoint".into(),
            ));
        }
        let schema = crate::sqlrt::wal_table::projected_schema(&self.schema, projection)?;
        Ok(Arc::new(FlightSqlExec::with_authorization(
            self.endpoint.clone(),
            self.sql.as_bytes().to_vec(),
            schema,
            self.authorization.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use skippr_lease::PipelineKey;

    #[test]
    fn empty_endpoint_is_typed_unavailable() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let provider = FlightSqlTableProvider::live_wal(
            String::new(),
            &PipelineKey::new("t", "w", "p").unwrap(),
            "ns",
            &[],
            schema,
            String::new(),
        );
        assert!(provider.endpoint.is_empty());
    }

    #[test]
    fn scan_returns_flight_sql_exec_leaf() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let provider = FlightSqlTableProvider::new("127.0.0.1:9".into(), "SELECT 1".into(), schema);
        assert!(!provider.sql.is_empty());
    }
}
