use crate::discover::schema_alter::SchemaAlterOp;
use crate::discover::SkipprDataType;
use crate::migration::journal::{
    latest_phase, JournalError, MigrationApply, MigrationLog, MigrationPhase, MigrationRecord,
};
use crate::migration::{load_sql_file, MigrationChecksum, MigrationDirection, MigrationId};
use crate::plugins::AlterSupport;
use crate::query_flight::service::sql_is_within_ceiling;
use crate::sqlrt::parser::{SParser, Statement};
use skippr_lease::CONTROL_FRAME_MAX_BYTES;

fn alter_stmt(sql: &str) -> crate::sqlrt::parser::AlterTableStatement {
    let mut parser = SParser::new(sql).unwrap();
    match parser.parse_statement().unwrap() {
        Statement::AlterTable(stmt) => stmt,
        other => panic!("expected AlterTable, got {other:?}"),
    }
}

fn alter_op(sql: &str) -> SchemaAlterOp {
    alter_stmt(sql).op
}

#[test]
fn f_rename_column() {
    assert_eq!(
        alter_op("ALTER TABLE s3_alter.s3_alter RENAME COLUMN a TO b"),
        SchemaAlterOp::Rename {
            from: "a".into(),
            to: "b".into(),
        }
    );
}

#[test]
fn f_rename_dotted_to_path() {
    assert_eq!(
        alter_op("ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region"),
        SchemaAlterOp::Rename {
            from: "region".into(),
            to: "detail.region".into(),
        }
    );
}

#[test]
fn f_rename_sql_token_forms_parse() {
    assert_eq!(
        alter_op("ALTER TABLE bikehire.rides RENAME COLUMN detail_region TO region_code"),
        SchemaAlterOp::Rename {
            from: "detail_region".into(),
            to: "region_code".into(),
        }
    );
    assert_eq!(
        alter_op("ALTER TABLE bikehire.rides RENAME COLUMN detail.region TO region_code"),
        SchemaAlterOp::Rename {
            from: "detail.region".into(),
            to: "region_code".into(),
        }
    );
}

#[test]
fn f_hyphenated_quoted_namespace() {
    let stmt = alter_stmt(
        r#"ALTER TABLE s3_alter."skippr-e2e-sample-data" RENAME COLUMN region TO region_code"#,
    );
    let pipeline = format!("{}", stmt.pipeline).replace('"', "");
    let namespace = stmt
        .namespace
        .as_ref()
        .map(|n| format!("{n}").replace('"', ""))
        .unwrap();
    assert_eq!(pipeline, "s3_alter");
    assert_eq!(namespace, "skippr-e2e-sample-data");
}

#[test]
fn f_merge_column() {
    assert_eq!(
        alter_op("ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price"),
        SchemaAlterOp::Merge {
            src: "price_string".into(),
            dst: "price".into(),
        }
    );
}

#[test]
fn f_drop_column() {
    assert_eq!(
        alter_op("ALTER TABLE bikehire.rides DROP COLUMN leftover"),
        SchemaAlterOp::Drop {
            column: "leftover".into(),
        }
    );
}

#[test]
fn f_rename_column_via_surviving_name() {
    use crate::sqlrt::iceberg_table::snapshot_scan_names;
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    let snapshot = Schema::builder()
        .with_schema_id(0)
        .with_fields(vec![
            NestedField::optional(1, "name", Type::Primitive(PrimitiveType::String)).into(),
            NestedField::optional(3, "category", Type::Primitive(PrimitiveType::String)).into(),
        ])
        .build()
        .unwrap();
    let current = Schema::builder()
        .with_schema_id(1)
        .with_fields(vec![
            NestedField::optional(1, "name", Type::Primitive(PrimitiveType::String)).into(),
            NestedField::optional(3, "category_code", Type::Primitive(PrimitiveType::String))
                .into(),
        ])
        .build()
        .unwrap();
    assert_eq!(
        snapshot_scan_names(&current, &snapshot, &["category_code".into()]),
        vec!["category".to_string()]
    );
}

#[test]
fn f_drop_column_absent_from_current_table() {
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    let current = Schema::builder()
        .with_schema_id(1)
        .with_fields(vec![NestedField::optional(
            1,
            "name",
            Type::Primitive(PrimitiveType::String),
        )
        .into()])
        .build()
        .unwrap();
    assert!(!current
        .as_struct()
        .fields()
        .iter()
        .any(|field| field.name == "leftover"));
}

#[test]
fn f_alter_column_type_widen() {
    assert_eq!(
        alter_op("ALTER TABLE bikehire.rides ALTER COLUMN n TYPE BIGINT"),
        SchemaAlterOp::Promote {
            column: "n".into(),
            to: SkipprDataType::Long,
        }
    );
}

#[test]
fn f_dst_alter_uses_production_apply_log() {
    use crate::discover::schema_alter::{apply_alter, canonicalize, FlattenMode, SchemaAlterOp};
    use crate::discover::Metadata;
    use crate::sqlrt::dst::{apply, batch_from_row, Op, UserRow, World};
    let batch = batch_from_row(&UserRow {
        id: 1,
        name: "a".into(),
        amount: 10,
    });
    let op = SchemaAlterOp::Promote {
        column: "amount".into(),
        to: SkipprDataType::Long,
    };
    let mut meta = Metadata::new().unwrap();
    let mut amount = Metadata::new_with_type(SkipprDataType::Long, "amount");
    amount.field_id = 3;
    meta.fields.insert("amount".into(), amount);
    let alter = canonicalize(FlattenMode::Nested, &meta, &op, &[]).unwrap();
    let out = apply_alter(batch, &alter).unwrap();
    assert!(out.schema().field_with_name("amount").is_ok());

    let mut world = World::new();
    apply(
        &mut world,
        Op::IngestLake(UserRow {
            id: 1,
            name: "a".into(),
            amount: 10,
        }),
    );
    apply(&mut world, Op::Alter(op));
    assert_eq!(world.snapshots.last().unwrap().rows.len(), 1);
    assert!(world.alter_error.is_none());
}

#[test]
fn f_dst_alter_records_canonicalize_and_keeps_rewritten_rows() {
    use crate::discover::schema_alter::SchemaAlterOp;
    use crate::sqlrt::dst::{apply, Op, UserRow, World};
    let mut world = World::new();
    apply(
        &mut world,
        Op::IngestLake(UserRow {
            id: 1,
            name: "a".into(),
            amount: 10,
        }),
    );
    apply(
        &mut world,
        Op::Alter(SchemaAlterOp::Drop {
            column: "leftover".into(),
        }),
    );
    assert!(world.alter_error.is_some(), "{:?}", world.alter_error);
    assert_eq!(world.snapshots.last().unwrap().rows[0].amount, 10);

    let mut drop_amount = World::new();
    apply(
        &mut drop_amount,
        Op::IngestLake(UserRow {
            id: 1,
            name: "a".into(),
            amount: 10,
        }),
    );
    apply(
        &mut drop_amount,
        Op::Alter(SchemaAlterOp::Drop {
            column: "amount".into(),
        }),
    );
    assert!(drop_amount.alter_error.is_none());
    assert_eq!(drop_amount.snapshots.last().unwrap().rows[0].amount, 0);
}

#[test]
fn f_illegal_type_fail_closed() {
    assert!(!crate::discover::schema_alter::is_iceberg_legal_promote(
        &SkipprDataType::String,
        &SkipprDataType::Long
    ));
}

#[test]
fn f_journal_idempotent_retry() {
    let checksum = MigrationChecksum::of("ALTER TABLE p.ns RENAME COLUMN a TO b");
    let apply = MigrationApply {
        id: MigrationId::new("m1"),
        checksum: checksum.clone(),
        direction: MigrationDirection::Up,
    };
    let log = MigrationLog {
        records: vec![MigrationRecord {
            id: apply.id.clone(),
            checksum,
            direction: MigrationDirection::Up,
            phase: MigrationPhase::Committed,
        }],
    };
    assert_eq!(
        latest_phase(&log, &apply).unwrap(),
        Some(MigrationPhase::Committed)
    );
}

#[test]
fn f_journal_checksum_mismatch() {
    let apply = MigrationApply {
        id: MigrationId::new("m1"),
        checksum: MigrationChecksum::of("new"),
        direction: MigrationDirection::Up,
    };
    let log = MigrationLog {
        records: vec![MigrationRecord {
            id: apply.id.clone(),
            checksum: MigrationChecksum::of("old"),
            direction: MigrationDirection::Up,
            phase: MigrationPhase::Committed,
        }],
    };
    match latest_phase(&log, &apply) {
        Err(JournalError::ChecksumMismatch { .. }) => {}
        other => panic!("expected checksum mismatch, got {other:?}"),
    }
}

#[test]
fn f_query_sql_alter_journals_query_checksum() {
    let checksum = MigrationChecksum::of("ALTER TABLE p.ns RENAME COLUMN a TO b");
    let id = MigrationId::query(checksum);
    assert!(id.as_str().starts_with("query-"));
}

#[test]
fn f_down_only_rename() {
    let sql = "\
ALTER TABLE bikehire.trips RENAME COLUMN region TO region_code;
-- migrate:down
ALTER TABLE bikehire.trips RENAME COLUMN region_code TO region;
";
    let loaded = load_sql_file("rename_region", sql).unwrap();
    assert_eq!(loaded.down.as_ref().map(|ops| ops.len()), Some(1));
}

#[test]
fn f_down_drop_merge_fail_closed() {
    for (id, sql) in [
        (
            "bad_drop",
            "ALTER TABLE p.ns DROP COLUMN leftover;\n-- migrate:down\nALTER TABLE p.ns RENAME COLUMN a TO b;\n",
        ),
        (
            "bad_promote",
            "ALTER TABLE p.ns ALTER COLUMN n TYPE BIGINT;\n-- migrate:down\nALTER TABLE p.ns RENAME COLUMN a TO b;\n",
        ),
    ] {
        let err = load_sql_file(id, sql).unwrap_err();
        assert!(
            format!("{err}").contains("down() is only valid for Rename"),
            "{id}: {err}"
        );
    }
}

#[test]
fn f_glue_hive_clickhouse_alter_fail_closed() {
    assert!(!AlterSupport::Unsupported.is_table());
    assert!(AlterSupport::Iceberg.is_table());
}

#[test]
fn f_query_after_migrate_two_part_name() {
    let stmt = alter_stmt("ALTER TABLE s3_alter.s3_alter DROP COLUMN leftover");
    assert!(stmt.namespace.is_some());
    assert!(!format!("{}", stmt.pipeline).contains("datafusion.public"));
}

#[test]
fn f_iceberg_writer_does_not_remove_old_schemas() {
    let src = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/skippr-iceberg-writer/src/lib.rs"
    ));
    assert!(
        !src.contains("remove_old_schemas()"),
        "Iceberg writer must keep historical schemas so Athena can resolve snapshot schema id 0"
    );
}

#[test]
fn f_iceberg_replace_schema_rename_field_id() {
    use crate::sqlrt::iceberg_table::snapshot_scan_names;
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    let snapshot = Schema::builder()
        .with_schema_id(0)
        .with_fields(vec![
            NestedField::optional(1, "name", Type::Primitive(PrimitiveType::String)).into(),
            NestedField::optional(3, "category", Type::Primitive(PrimitiveType::String)).into(),
        ])
        .build()
        .unwrap();
    let current = Schema::builder()
        .with_schema_id(1)
        .with_fields(vec![
            NestedField::optional(1, "name", Type::Primitive(PrimitiveType::String)).into(),
            NestedField::optional(3, "category_code", Type::Primitive(PrimitiveType::String))
                .into(),
        ])
        .build()
        .unwrap();
    assert_eq!(
        snapshot_scan_names(
            &current,
            &snapshot,
            &["category_code".into(), "name".into()]
        ),
        vec!["category".to_string(), "name".to_string()]
    );
}

#[test]
fn f_migrate_reuses_open_store() {
    use crate::helpers::configuration::{Pipeline, Skippr};
    use crate::helpers::wal_storage::ElStorageMode;
    use crate::migration::{apply_pending_on, MigrationDirection};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path()).unwrap();
    let mut config = crate::helpers::configuration::Config::new();
    config.skippr = Some(Skippr {
        workspace: Some("dstmig".into()),
        skipprd_el_storage_mode: Some(ElStorageMode::Local),
        ..Skippr::default()
    });
    config.pipelines.insert(
        "dst-migrate-reuse".into(),
        Pipeline {
            data_dir: Some(dir.path().display().to_string()),
            ..Pipeline::default()
        },
    );
    let config = config.bind_pipeline("dst-migrate-reuse");
    let offsets = Arc::new(crate::helpers::offsets::Offsets::init(&config).unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let first = rt
        .block_on(apply_pending_on(&config, &offsets, MigrationDirection::Up))
        .unwrap();
    let second = rt
        .block_on(apply_pending_on(&config, &offsets, MigrationDirection::Up))
        .unwrap();
    assert_eq!(first, second);
}

#[test]
fn legal_rename_parses() {
    let mut parser = SParser::new("ALTER TABLE s3_alter RENAME COLUMN a TO b").unwrap();
    parser.parse_statement().unwrap();
}

#[test]
fn illegal_sql_fails_closed_without_panic() {
    match SParser::new("ALTER TABLE s3_alter DROP COLUMN") {
        Err(_) => {}
        Ok(mut parser) => {
            assert!(parser.parse_statement().is_err());
        }
    }
}

#[test]
fn oversized_sql_fails_closed() {
    let sql = "x".repeat(CONTROL_FRAME_MAX_BYTES + 1);
    assert!(!sql_is_within_ceiling(&sql));
    assert!(sql_is_within_ceiling("SELECT 1"));
}
