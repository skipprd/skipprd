use crate::buffer::segment_file::{SegmentFile, WalPayload, PART_CODEC_ARROW, PART_CODEC_COPY};
use crate::discover::schema_alter::SchemaAlterOp;
use arrow::array::RecordBatch;
use arrow::datatypes::{Field, Schema};
use arrow::ipc::reader::StreamReader;
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

const FIELD_ID_KEYS: [&str; 2] = ["PARQUET:field_id", "iceberg.field-id"];

pub fn rewrite_record_batch(
    batch: RecordBatch,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<RecordBatch, String> {
    let schema = rewrite_schema(batch.schema_ref().as_ref(), op, field_id)?;
    RecordBatch::try_new(Arc::new(schema), batch.columns().to_vec()).map_err(|err| err.to_string())
}

pub fn rewrite_schema(
    schema: &Schema,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<Schema, String> {
    let id_token = field_id.to_string();
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|field| rewrite_field(field.as_ref(), op, &id_token))
        .collect();
    Ok(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

fn rewrite_field(field: &Field, op: &SchemaAlterOp, id_token: &str) -> Field {
    let matches = field_has_id(field, id_token) || field.name() == lookup_name(op);
    if !matches {
        return field.clone();
    }
    match op {
        SchemaAlterOp::Rename { to, .. } => field.clone().with_name(iceberg_flatten(to)),
        SchemaAlterOp::Promote { .. } => field.clone(),
        SchemaAlterOp::Drop { .. } | SchemaAlterOp::Merge { .. } => field.clone(),
    }
}

fn lookup_name(op: &SchemaAlterOp) -> &str {
    match op {
        SchemaAlterOp::Rename { from, .. } => from.as_str(),
        SchemaAlterOp::Merge { src, .. } => src.as_str(),
        SchemaAlterOp::Drop { column } | SchemaAlterOp::Promote { column, .. } => column.as_str(),
    }
}

fn iceberg_flatten(name: &str) -> String {
    name.replace('.', "_").replace('-', "_")
}

fn field_has_id(field: &Field, id_token: &str) -> bool {
    FIELD_ID_KEYS
        .iter()
        .any(|key| field.metadata().get(*key).map(String::as_str) == Some(id_token))
}

/// Rewrite live Arrow WAL parts onto schema N+1. Copy/non-Arrow parts fail closed.
pub fn rewrite_live_wal(
    config: &crate::helpers::configuration::Config,
    namespace: &str,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<(), String> {
    crate::buffer::ingest_buffer::Buffers::map_live_arrow(namespace, |batch| {
        rewrite_record_batch(batch, op, field_id)
    })?;
    let paths =
        skippr_lease::PipelinePaths::legacy_disk(std::path::Path::new(&config.get_data_dir()));
    if !paths.segs.exists() {
        return Ok(());
    }
    let entries = std::fs::read_dir(&paths.segs).map_err(|err| err.to_string())?;
    for entry in entries {
        let path = entry.map_err(|err| err.to_string())?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("seg") {
            continue;
        }
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| stem.ends_with("-migrating"))
        {
            continue;
        }
        rewrite_segment_file(config, &path, namespace, op, field_id)?;
    }
    Ok(())
}

fn rewrite_segment_file(
    config: &crate::helpers::configuration::Config,
    path: &Path,
    namespace: &str,
    op: &SchemaAlterOp,
    field_id: i32,
) -> Result<(), String> {
    let meta = match SegmentFile::admit_owned_pair_path(path) {
        Ok(meta) => meta,
        Err(_) => return Ok(()),
    };
    let mut file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let mut batches: HashMap<_, WalPayload> = HashMap::new();
    let mut partitions_meta = HashMap::new();
    let mut part_meta_blobs = HashMap::new();
    let mut mutated = false;
    for idx in &meta.index {
        if idx.codec == PART_CODEC_COPY && idx.key.namespace == namespace {
            return Err(format!(
                "copy/non-Arrow WAL parts cannot be rewritten ({})",
                path.display()
            ));
        }
        file.seek(std::io::SeekFrom::Start(idx.start))
            .map_err(|err| err.to_string())?;
        let mut take = (&mut file).take(idx.len);
        let payload = if idx.codec == PART_CODEC_ARROW {
            let reader = StreamReader::try_new(&mut take, None).map_err(|err| err.to_string())?;
            let mut arrow_batches = Vec::new();
            for batch in reader {
                let batch = batch.map_err(|err| err.to_string())?;
                if idx.key.namespace == namespace {
                    arrow_batches.push(rewrite_record_batch(batch, op, field_id)?);
                    mutated = true;
                } else {
                    arrow_batches.push(batch);
                }
            }
            WalPayload::Arrow(arrow_batches)
        } else {
            let mut bytes = Vec::new();
            take.read_to_end(&mut bytes)
                .map_err(|err| err.to_string())?;
            WalPayload::Raw(
                crate::buffer::segment_file::decode_copy_part(&bytes)
                    .map_err(|err| err.to_string())?,
            )
        };
        if idx.part_meta_len > 0 {
            let mut blob = vec![0u8; idx.part_meta_len as usize];
            file.seek(std::io::SeekFrom::Start(idx.part_meta_start))
                .map_err(|err| err.to_string())?;
            file.read_exact(&mut blob).map_err(|err| err.to_string())?;
            part_meta_blobs.insert(idx.key.clone(), blob);
        }
        partitions_meta.insert(
            idx.key.clone(),
            (payload.estimated_bytes(), SystemTime::now()),
        );
        batches.insert(idx.key.clone(), payload);
    }
    if !mutated {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| "WAL segment has no parent".to_string())?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "WAL segment has no stem".to_string())?;
    let tmp = SegmentFile::new(dir, &format!("{stem}-migrating")).map_err(|err| err.to_string())?;
    let (new_meta, _, _) = tmp
        .write_snapshot(&meta.offsets, &batches, &partitions_meta, &part_meta_blobs)
        .map_err(|err| err.to_string())?;
    tmp.write_commit_marker(&new_meta)
        .map_err(|err| err.to_string())?;
    std::fs::rename(&tmp.path, path).map_err(|err| err.to_string())?;
    std::fs::rename(
        SegmentFile::commit_path_for_seg(&tmp.path),
        SegmentFile::commit_path_for_seg(path),
    )
    .map_err(|err| err.to_string())?;
    let admitted = SegmentFile::admit_owned_pair_path(path).map_err(|err| err.to_string())?;
    crate::buffer::ingest_buffer::Buffers::reregister_disk_segment(
        config,
        path.to_path_buf(),
        admitted,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::segment_file::PartitionKey;
    use arrow::array::Int64Array;
    use arrow::datatypes::DataType;
    use std::collections::HashMap;

    #[test]
    fn rename_rewrites_arrow_name_and_keeps_field_id() {
        let mut meta = HashMap::new();
        meta.insert("PARQUET:field_id".into(), "7".into());
        let field = Field::new("price", DataType::Int64, true).with_metadata(meta);
        let schema = Schema::new(vec![field]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(Int64Array::from(vec![1, 2]))],
        )
        .unwrap();
        let rewritten = rewrite_record_batch(
            batch,
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            },
            7,
        )
        .unwrap();
        assert_eq!(rewritten.schema().field(0).name(), "amount");
        assert_eq!(
            rewritten
                .schema()
                .field(0)
                .metadata()
                .get("PARQUET:field_id"),
            Some(&"7".to_string())
        );
    }

    #[test]
    fn rewrite_segment_persists_renamed_field() {
        let dir = std::env::temp_dir().join(format!("skippr_mig_wal_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let seg = SegmentFile::new(&dir, "t1").unwrap();
        let mut field_meta = HashMap::new();
        field_meta.insert("PARQUET:field_id".into(), "7".into());
        let field = Field::new("price", DataType::Int64, true).with_metadata(field_meta);
        let schema = Schema::new(vec![field]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(Int64Array::from(vec![1, 2]))],
        )
        .unwrap();
        let key = PartitionKey {
            sink_ref: "data_outputs.test".into(),
            namespace: "trips".into(),
            partition: String::new(),
            time: Some(0),
            schema_fingerprint: "schema".into(),
        };
        let mut batches = HashMap::new();
        batches.insert(key.clone(), WalPayload::Arrow(vec![batch]));
        let mut parts_meta = HashMap::new();
        parts_meta.insert(key, (0, SystemTime::now()));
        let offsets = HashMap::new();
        let empty_blobs = HashMap::new();
        let (meta, _, _) = seg
            .write_snapshot(&offsets, &batches, &parts_meta, &empty_blobs)
            .unwrap();
        seg.write_commit_marker(&meta).unwrap();

        let config = crate::helpers::configuration::Config::new();
        rewrite_segment_file(
            &config,
            &seg.path,
            "trips",
            &SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            },
            7,
        )
        .unwrap();

        let admitted = SegmentFile::admit_owned_pair_path(&seg.path).unwrap();
        let mut file = std::fs::File::open(&seg.path).unwrap();
        file.seek(std::io::SeekFrom::Start(admitted.index[0].start))
            .unwrap();
        let mut take = (&mut file).take(admitted.index[0].len);
        let reader = StreamReader::try_new(&mut take, None).unwrap();
        let rewritten = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(rewritten.schema().field(0).name(), "amount");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
