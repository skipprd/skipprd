//! One DataFusion register path for `pipeline.compute_name`.

use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use datafusion::datasource::memory::MemTable;
use datafusion::datasource::TableProvider;
use datafusion::error::DataFusionError;
use skippr_wal_catalog::{SkipprStoreRead, StoreFamily};

use super::{ComputeKind, Fqn, Relation, StatelessSelect};

pub struct ComputeTableProvider;

impl ComputeTableProvider {
    pub fn from_store(
        store: &dyn SkipprStoreRead,
        pipeline: &str,
        kind: &ComputeKind,
    ) -> Result<Arc<dyn TableProvider>, DataFusionError> {
        let fqn = kind.name();
        match kind {
            ComputeKind::KeyedTable { keys, measures, .. } => keyed_provider(
                store,
                pipeline,
                fqn,
                keys.as_slice().len(),
                measures.as_slice(),
            ),
            ComputeKind::WindowedTable { keys, measures, .. } => keyed_provider(
                store,
                pipeline,
                fqn,
                keys.as_slice().len() + 1,
                measures.as_slice(),
            ),
            ComputeKind::Latest { keys, .. } => {
                latest_provider(store, pipeline, fqn, keys.as_slice().len())
            }
            ComputeKind::View { .. } | ComputeKind::StreamTableJoin { .. } => {
                Err(DataFusionError::Plan(
                    "view / join is a DataFusion view over the input relation; register via view_sql"
                        .into(),
                ))
            }
            ComputeKind::Stateless { .. } => Err(DataFusionError::Plan(
                "Stateless is not table-backed; ALTER COMPUTE only".into(),
            )),
        }
    }
}

fn keyed_provider(
    store: &dyn SkipprStoreRead,
    pipeline: &str,
    fqn: &Fqn,
    key_count: usize,
    measures: &[super::Measure],
) -> Result<Arc<dyn TableProvider>, DataFusionError> {
    let mut fields = Vec::new();
    for i in 0..key_count {
        fields.push(Field::new(format!("key_{i}"), DataType::Utf8, false));
    }
    for measure in measures {
        fields.push(Field::new(measure.name.as_str(), DataType::Float64, false));
    }
    let schema = Arc::new(Schema::new(fields));
    let prefix =
        StoreFamily::StreamState.key(format!("agg/{}/{}/", pipeline, fqn.name.as_str()).as_bytes());
    let page = store
        .query_prefix(&prefix, None, usize::MAX)
        .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    let mut key_cols: Vec<Vec<String>> = vec![Vec::new(); key_count];
    let mut measure_cols: Vec<Vec<f64>> = vec![Vec::new(); measures.len()];
    for (key, value) in page {
        let rest = key.strip_prefix(prefix.as_slice()).unwrap_or(&key);
        let encoded = String::from_utf8_lossy(rest);
        let parts: Vec<&str> = encoded.split('\u{1f}').collect();
        for i in 0..key_count {
            key_cols[i].push(parts.get(i).copied().unwrap_or("").to_string());
        }
        let map: std::collections::BTreeMap<String, f64> =
            serde_json::from_slice(&value).map_err(|err| DataFusionError::Plan(err.to_string()))?;
        for (i, measure) in measures.iter().enumerate() {
            measure_cols[i].push(*map.get(measure.name.as_str()).unwrap_or(&0.0));
        }
    }
    let mut arrays: Vec<ArrayRef> = key_cols
        .into_iter()
        .map(|col| Arc::new(StringArray::from(col)) as ArrayRef)
        .collect();
    for col in measure_cols {
        arrays.push(Arc::new(Float64Array::from(col)));
    }
    let batch = if arrays.iter().all(|a| a.len() == 0) {
        RecordBatch::new_empty(schema.clone())
    } else {
        RecordBatch::try_new(schema.clone(), arrays)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?
    };
    Ok(Arc::new(MemTable::try_new(schema, vec![vec![batch]])?))
}

fn latest_provider(
    store: &dyn SkipprStoreRead,
    pipeline: &str,
    fqn: &Fqn,
    _key_count: usize,
) -> Result<Arc<dyn TableProvider>, DataFusionError> {
    let prefix =
        StoreFamily::StreamState.key(format!("row/{}/{}/", pipeline, fqn.name.as_str()).as_bytes());
    let page = store
        .query_prefix(&prefix, None, usize::MAX)
        .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    let mut json_rows: Vec<serde_json::Map<String, serde_json::Value>> = Vec::new();
    for (_, value) in page {
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&value).map_err(|err| DataFusionError::Plan(err.to_string()))?;
        if !map.is_empty() {
            json_rows.push(map);
        }
    }
    if json_rows.is_empty() {
        let schema = Arc::new(Schema::new(vec![Field::new("key_0", DataType::Utf8, true)]));
        return Ok(Arc::new(MemTable::try_new(
            schema.clone(),
            vec![vec![RecordBatch::new_empty(schema)]],
        )?));
    }
    let names: Vec<String> = json_rows[0].keys().cloned().collect();
    let fields: Vec<Field> = names
        .iter()
        .map(|n| Field::new(n.as_str(), DataType::Utf8, true))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let arrays: Vec<ArrayRef> = names
        .iter()
        .map(|name| {
            let col: Vec<Option<String>> = json_rows
                .iter()
                .map(|row| row.get(name).map(|v| json_cell(v)))
                .collect();
            Arc::new(StringArray::from(col)) as ArrayRef
        })
        .collect();
    let batch = RecordBatch::try_new(schema.clone(), arrays)
        .map_err(|err| DataFusionError::Plan(err.to_string()))?;
    Ok(Arc::new(MemTable::try_new(schema, vec![vec![batch]])?))
}

/// SQL for a declared View or StreamTableJoin over already-registered two-part names.
pub fn view_sql(kind: &ComputeKind) -> Result<String, DataFusionError> {
    match kind {
        ComputeKind::View { input, select, .. } => Ok(stateless_over(relation_sql(input), select)),
        ComputeKind::StreamTableJoin { .. } => Err(DataFusionError::Plan(
            "StreamTableJoin SQL needs the stream schema; use stream_join_sql".into(),
        )),
        _ => Err(DataFusionError::Plan(
            "view_sql is only for View and StreamTableJoin".into(),
        )),
    }
}

pub fn stream_join_sql(
    kind: &ComputeKind,
    stream_schema: &arrow_schema::Schema,
) -> Result<String, DataFusionError> {
    let ComputeKind::StreamTableJoin {
        stream, table, on, ..
    } = kind
    else {
        return Err(DataFusionError::Plan(
            "stream_join_sql is only for StreamTableJoin".into(),
        ));
    };
    let left = relation_sql(stream);
    let right = format!(
        "{}.{}",
        quote_ident(table.pipeline.as_str()),
        quote_ident(table.name.as_str())
    );
    let mut pred = Vec::new();
    for (i, id) in on.as_slice().iter().enumerate() {
        let name = super::name_for_field_id(stream_schema, *id).ok_or_else(|| {
            DataFusionError::Plan(format!("join FieldId {} missing from stream", id.get()))
        })?;
        pred.push(format!("l.{} = r.key_{i}", quote_ident(name)));
    }
    if pred.is_empty() {
        return Err(DataFusionError::Plan(
            "StreamTableJoin requires join keys".into(),
        ));
    }
    Ok(format!(
        "SELECT * FROM {left} AS l JOIN {right} AS r ON {}",
        pred.join(" AND ")
    ))
}

pub fn relation_sql(rel: &Relation) -> String {
    match rel {
        Relation::PipelineTable {
            pipeline,
            namespace,
        } => format!(
            "{}.{}",
            quote_ident(pipeline.as_str()),
            quote_ident(namespace.as_str())
        ),
        Relation::View { name }
        | Relation::Latest { name }
        | Relation::KeyedTable { name }
        | Relation::WindowedTable { name } => format!(
            "{}.{}",
            quote_ident(name.pipeline.as_str()),
            quote_ident(name.name.as_str())
        ),
    }
}

fn stateless_over(from: String, select: &StatelessSelect) -> String {
    let mut sql = format!("SELECT * FROM {from}");
    if let Some(filter) = select.filter.as_deref() {
        sql = format!("{sql} WHERE {filter}");
    }
    for (name, expr) in &select.derive {
        sql = format!("SELECT *, ({expr}) AS {} FROM ({sql})", quote_ident(name));
    }
    sql
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn json_cell(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
