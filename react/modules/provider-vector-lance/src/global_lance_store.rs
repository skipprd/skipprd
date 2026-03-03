use crate::lance_store::{Chunk, LanceDbStore, ScoredChunk};
use serde_json::Value;

pub struct GlobalLanceDbStore {
    uri: String,
}

impl GlobalLanceDbStore {
    pub fn new(uri: String) -> Self {
        Self { uri }
    }

    pub async fn upsert(&self, items: &[Chunk]) -> Result<(), String> {
        LanceDbStore::new(&self.uri).upsert(items).await
    }

    pub async fn query(&self, query_vec: &[f32], k: usize) -> Result<Vec<ScoredChunk>, String> {
        use lancedb::query::{ExecutableQuery, QueryBase};
        let db = lancedb::connect(&self.uri)
            .execute()
            .await
            .map_err(|e| format!("{:?}", e))?;
        let tbl = db
            .open_table("embeddings")
            .execute()
            .await
            .map_err(|e| format!("{:?}", e))?;
        let q = tbl
            .vector_search(query_vec.to_vec())
            .map_err(|e| format!("{:?}", e))?
            .limit(k);
        let mut stream = q.execute().await.map_err(|e| format!("{:?}", e))?;
        use arrow::record_batch::RecordBatch as ArrowRecordBatch;
        use futures::StreamExt;
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
                    i.and_then(|j| {
                        arrow::util::display::array_value_to_string(b.column(j).as_ref(), r).ok()
                    })
                    .unwrap_or_default()
                };
                let id = s_val(id_i);
                let kind = s_val(kind_i);
                let dataset_id = s_val(ds_i);
                let field_s = s_val(field_i);
                let text = s_val(text_i);
                let epoch = s_val(epoch_i).parse::<u64>().unwrap_or(0);
                let score = s_val(dist_i).parse::<f32>().unwrap_or(0.0);
                let field = if field_s.is_empty() {
                    None
                } else {
                    Some(field_s)
                };
                out.push(ScoredChunk {
                    item: Chunk {
                        id,
                        kind,
                        dataset_id,
                        field,
                        text,
                        vector: Vec::new(),
                        meta: Value::Null,
                        epoch,
                    },
                    score,
                });
            }
        }
        Ok(out)
    }
}
