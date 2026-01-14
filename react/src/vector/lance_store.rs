use serde::{Deserialize, Serialize};
use serde_json::Value;
// removed unused tracing imports

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub kind: String,         // dataset|field|doc
    pub dataset_id: String,
    pub field: Option<String>,
    pub text: String,
    pub vector: Vec<f32>,
    pub meta: Value,
    pub epoch: u64,
}

#[derive(Clone, Debug)]
pub struct ScoredChunk {
    pub item: Chunk,
    pub score: f32,
}

pub struct LanceDbStore {
    uri: String,
}

impl LanceDbStore {
    pub fn new(uri: &str) -> Self {
        Self { uri: uri.to_string() }
    }

    pub async fn upsert(&self, items: &[Chunk]) -> Result<(), String> {
            use tracing::info;
            use arrow::array::{StringArray, Float32Array, UInt64Array};
            use arrow::datatypes::{DataType, Field, Schema};
            use arrow::record_batch::RecordBatch;
            use std::sync::Arc;
            if items.is_empty() { return Ok(()); }
            // Build Arrow arrays
            let ids = StringArray::from(items.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
            let kinds = StringArray::from(items.iter().map(|c| c.kind.clone()).collect::<Vec<_>>());
            let dataset_ids = StringArray::from(items.iter().map(|c| c.dataset_id.clone()).collect::<Vec<_>>());
            let fields = StringArray::from(items.iter().map(|c| c.field.clone().unwrap_or_default()).collect::<Vec<_>>());
            let texts = StringArray::from(items.iter().map(|c| c.text.clone()).collect::<Vec<_>>());
            let epochs = UInt64Array::from(items.iter().map(|c| c.epoch).collect::<Vec<_>>());
            // For vectors, store as a fixed-size list of f32 (if dims consistent), otherwise join as JSON string
            let dims = items.first().map(|c| c.vector.len()).unwrap_or(0);
            if dims == 0 { return Err("empty vectors".to_string()); }
            let mut flat: Vec<f32> = Vec::with_capacity(items.len() * dims);
            for c in items {
                if c.vector.len() != dims { return Err("inconsistent vector dims".to_string()); }
                flat.extend_from_slice(&c.vector);
            }
            let vectors = Float32Array::from(flat);
            // Schema
            let schema = Arc::new(Schema::new(vec![
                Field::new("id", DataType::Utf8, false),
                Field::new("kind", DataType::Utf8, false),
                Field::new("dataset_id", DataType::Utf8, false),
                Field::new("field", DataType::Utf8, false),
                Field::new("text", DataType::Utf8, false),
                Field::new("epoch", DataType::UInt64, false),
                Field::new("vector", DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, false)), dims as i32), false),
            ]));
            // Convert vectors to FixedSizeList view
            use arrow::array::{FixedSizeListArray, ArrayRef};
            use arrow_buffer::NullBuffer;
            let values: ArrayRef = Arc::new(vectors);
            let list_field = Arc::new(Field::new("item", DataType::Float32, false));
            let fsl = FixedSizeListArray::try_new(list_field, dims as i32, values, None as Option<NullBuffer>).map_err(|e| e.to_string())?;
            let batch = RecordBatch::try_new(schema, vec![
                Arc::new(ids), Arc::new(kinds), Arc::new(dataset_ids), Arc::new(fields), Arc::new(texts), Arc::new(epochs), Arc::new(fsl)
            ]).map_err(|e| e.to_string())?;
            // Connect to LanceDB and upsert
            info!("LanceDB: connecting uri='{}'", self.uri);
            let db = lancedb::connect(&self.uri).execute().await.map_err(|e| format!("{:?}", e))?;
            // Build a RecordBatch iterator to satisfy IntoArrow
            use arrow::error::Result as ArrowResult;
            use arrow::record_batch::RecordBatchIterator;
            let rb_iter = RecordBatchIterator::new(vec![ArrowResult::Ok(batch.clone())].into_iter(), batch.schema());
            let tbl = match db.open_table("embeddings").execute().await {
                Ok(t) => t,
                Err(_) => db.create_table("embeddings", rb_iter).execute().await.map_err(|e| format!("{:?}", e))?,
            };
            // Add as iterator to satisfy IntoArrow
            let add_iter = RecordBatchIterator::new(vec![ArrowResult::Ok(batch.clone())].into_iter(), batch.schema());
            let _ = tbl.add(add_iter).execute().await.map_err(|e| format!("{:?}", e))?;
            info!("LanceDB: upserted rows={} dims={}", items.len(), dims);
            Ok(())
    }

    pub async fn query(&self, query_vec: &[f32], k: usize, scope: Option<&str>) -> Result<Vec<ScoredChunk>, String> {
            use lancedb::query::ExecutableQuery;
            use lancedb::query::QueryBase;
            let db = lancedb::connect(&self.uri).execute().await.map_err(|e| format!("{:?}", e))?;
            let tbl = db.open_table("embeddings").execute().await.map_err(|e| format!("{:?}", e))?;
            let q = tbl.vector_search(query_vec.to_vec()).map_err(|e| format!("{:?}", e))?.limit(k as usize);
            let mut stream = q.execute().await.map_err(|e| format!("{:?}", e))?;
            use futures::StreamExt;
            use arrow::record_batch::RecordBatch as ArrowRecordBatch;
            let mut out: Vec<ScoredChunk> = Vec::new();
            while let Some(batch_res) = stream.next().await {
                let b: ArrowRecordBatch = batch_res.map_err(|e| format!("{:?}", e))?;
                let schema = b.schema();
                let idx = |name: &str| schema.index_of(name).ok();
                let id_i = idx("id");
                let kind_i = idx("kind");
                let ds_i = idx("dataset_id");
                let field_i = idx("field");
                let text_i = idx("text");
                let epoch_i = idx("epoch");
                let dist_i = idx("_distance");
                for r in 0..b.num_rows() {
                    let s_val = |i: Option<usize>| -> String {
                        i.and_then(|j| arrow::util::display::array_value_to_string(b.column(j).as_ref(), r).ok())
                            .unwrap_or_default()
                    };
                    let id = s_val(id_i);
                    let kind = s_val(kind_i);
                    let dataset_id = s_val(ds_i);
                    let field_s = s_val(field_i);
                    let text = s_val(text_i);
                    let epoch = s_val(epoch_i).parse::<u64>().unwrap_or(0);
                    let score = s_val(dist_i).parse::<f32>().unwrap_or(0.0);
                    // Interpret scope as kind filter (dataset|field|doc)
                    if let Some(sc) = scope {
                        if (sc == "dataset" && kind != "dataset")
                            || (sc == "field" && kind != "field")
                            || (sc == "doc" && kind != "doc") {
                            continue;
                        }
                    }
                    let field = if field_s.is_empty() { None } else { Some(field_s) };
                    out.push(ScoredChunk { item: Chunk { id, kind, dataset_id, field, text, vector: Vec::new(), meta: serde_json::json!({}), epoch }, score });
                }
            }
            Ok(out)
    }

    pub async fn delete_thread_embeddings(&self, thread_id: &str) -> Result<(), String> {
            // Best-effort deletion by id pattern containing thread identifier
            let db = lancedb::connect(&self.uri).execute().await.map_err(|e| format!("{:?}", e))?;
            let tbl = db.open_table("embeddings").execute().await.map_err(|e| format!("{:?}", e))?;
            // Attempt a predicate delete on id LIKE pattern
            let pred = format!("id LIKE '%:{}:%'", thread_id);
            match tbl.delete(pred.as_str()).await {
                Ok(_) => Ok(()),
                Err(e) => Err(format!("{:?}", e)),
            }
    }

    pub async fn delete_pipeline_embeddings(&self) -> Result<(), String> {
            // Best-effort wipe of all rows in the embeddings table.
            let db = lancedb::connect(&self.uri).execute().await.map_err(|e| format!("{:?}", e))?;
            let tbl = db.open_table("embeddings").execute().await.map_err(|e| format!("{:?}", e))?;
            // Predicate that matches all rows.
            match tbl.delete("id IS NOT NULL").await {
                Ok(_) => Ok(()),
                Err(e) => Err(format!("{:?}", e)),
            }
    }
}


