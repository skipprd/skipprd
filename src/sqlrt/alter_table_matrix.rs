//! Functional matrix: canonicalize → field_id → apply → Iceberg schema.
//!
//! Every `SchemaAlterOp` and every Iceberg-legal promote pair is a named row.

use std::sync::Arc;

use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};

use crate::discover::schema_alter::{
    apply, canonicalize_op, field_id_for_op, is_iceberg_legal_promote, SchemaAlterOp,
};
use crate::discover::{Metadata, SkipprDataType};
use crate::sqlrt::iceberg_alter::iceberg_schema_after_alter;
use crate::sqlrt::parser::{SParser, Statement};

#[derive(Clone, Copy)]
enum IdMode {
    Matching,
    Sequential,
}

struct Case {
    name: &'static str,
    op: SchemaAlterOp,
    id_mode: IdMode,
    expect_ok: bool,
    err_contains: Option<&'static str>,
    iceberg_has: Option<&'static str>,
    iceberg_lacks: Option<&'static str>,
}

const LEGAL_PROMOTES: &[(SkipprDataType, SkipprDataType)] = &[
    (SkipprDataType::Byte, SkipprDataType::Integer),
    (SkipprDataType::Byte, SkipprDataType::Long),
    (SkipprDataType::Short, SkipprDataType::Integer),
    (SkipprDataType::Short, SkipprDataType::Long),
    (SkipprDataType::Integer, SkipprDataType::Long),
    (SkipprDataType::Float, SkipprDataType::Double),
    (SkipprDataType::TimestampMilli, SkipprDataType::Timestamp),
];

const ILLEGAL_PROMOTES: &[(SkipprDataType, SkipprDataType)] = &[
    (SkipprDataType::Integer, SkipprDataType::String),
    (SkipprDataType::Long, SkipprDataType::Integer),
    (SkipprDataType::Double, SkipprDataType::Float),
    (SkipprDataType::String, SkipprDataType::Long),
    (SkipprDataType::Timestamp, SkipprDataType::TimestampMilli),
    (SkipprDataType::Boolean, SkipprDataType::Integer),
];

fn field(name: &str, ty: SkipprDataType, field_id: i32) -> Metadata {
    let mut m = Metadata::new_with_type(ty, name);
    m.field_id = field_id;
    m
}

fn fixture_meta() -> Metadata {
    let mut pour = field("pour_location", SkipprDataType::Record, 1_859_207_811);
    pour.fields.insert(
        "latitude".into(),
        field("latitude", SkipprDataType::Decimal, 11),
    );
    let mut detail = field("detail", SkipprDataType::Record, 1);
    detail.fields.insert(
        "truck_reg".into(),
        field("truck_reg", SkipprDataType::String, 4),
    );
    detail.fields.insert(
        "geofence_id".into(),
        field("geofence_id", SkipprDataType::String, 23),
    );
    detail.fields.insert("pour_location".into(), pour);
    let mut detail_record = field("detail_record", SkipprDataType::Record, 2);
    detail_record.fields.insert(
        "geofence_id".into(),
        field("geofence_id", SkipprDataType::String, 12),
    );
    let mut root = Metadata::new().unwrap();
    for f in [
        field("region", SkipprDataType::String, 575_921_751),
        field("version", SkipprDataType::Integer, 298_041_464),
        field("note", SkipprDataType::String, 8),
        field("price", SkipprDataType::Long, 7),
        field("price_string", SkipprDataType::String, 9),
        field("n_byte", SkipprDataType::Byte, 31),
        field("n_short", SkipprDataType::Short, 32),
        field("n_float", SkipprDataType::Float, 33),
        field("n_double", SkipprDataType::Double, 36),
        field("ts_milli", SkipprDataType::TimestampMilli, 34),
        field("ts_micro", SkipprDataType::Timestamp, 37),
        field("already_long", SkipprDataType::Long, 35),
        field("flag", SkipprDataType::Boolean, 38),
        detail,
        detail_record,
    ] {
        root.set_field(&f.out_field_name.clone(), f);
    }
    root
}

fn iceberg_type(ty: &SkipprDataType) -> Type {
    Type::Primitive(match ty {
        SkipprDataType::Long => PrimitiveType::Long,
        SkipprDataType::Float => PrimitiveType::Float,
        SkipprDataType::Double => PrimitiveType::Double,
        SkipprDataType::Decimal => PrimitiveType::Decimal {
            precision: 38,
            scale: 9,
        },
        SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => PrimitiveType::Timestamp,
        SkipprDataType::Byte | SkipprDataType::Short | SkipprDataType::Integer => {
            PrimitiveType::Int
        }
        SkipprDataType::Boolean => PrimitiveType::Boolean,
        _ => PrimitiveType::String,
    })
}

fn fixture_iceberg(mode: IdMode) -> Schema {
    let cols: [(&str, i32, SkipprDataType); 17] = [
        ("region", 575_921_751, SkipprDataType::String),
        ("version", 298_041_464, SkipprDataType::Integer),
        ("note", 8, SkipprDataType::String),
        ("price", 7, SkipprDataType::Long),
        ("price_string", 9, SkipprDataType::String),
        ("n_byte", 31, SkipprDataType::Byte),
        ("n_short", 32, SkipprDataType::Short),
        ("n_float", 33, SkipprDataType::Float),
        ("n_double", 36, SkipprDataType::Double),
        ("ts_milli", 34, SkipprDataType::TimestampMilli),
        ("ts_micro", 37, SkipprDataType::Timestamp),
        ("already_long", 35, SkipprDataType::Long),
        ("flag", 38, SkipprDataType::Boolean),
        ("detail_truck_reg", 4, SkipprDataType::String),
        ("detail_geofence_id", 23, SkipprDataType::String),
        ("detail_record_geofence_id", 12, SkipprDataType::String),
        ("detail_pour_location_latitude", 11, SkipprDataType::Decimal),
    ];
    let fields: Vec<Arc<NestedField>> = cols
        .iter()
        .enumerate()
        .map(|(idx, (name, skippr_id, ty))| {
            let id = match mode {
                IdMode::Matching => *skippr_id,
                IdMode::Sequential => (idx + 1) as i32,
            };
            Arc::new(NestedField::new(id, *name, iceberg_type(ty), false))
        })
        .collect();
    Schema::builder()
        .with_schema_id(0)
        .with_fields(fields)
        .build()
        .unwrap()
}

fn promote_column(from: &SkipprDataType) -> &'static str {
    match from {
        SkipprDataType::Byte => "n_byte",
        SkipprDataType::Short => "n_short",
        SkipprDataType::Integer => "version",
        SkipprDataType::Float => "n_float",
        SkipprDataType::TimestampMilli => "ts_milli",
        SkipprDataType::Long => "already_long",
        SkipprDataType::String => "region",
        SkipprDataType::Double => "n_double",
        SkipprDataType::Timestamp => "ts_micro",
        SkipprDataType::Boolean => "flag",
        _ => "version",
    }
}

fn matrix() -> Vec<Case> {
    let mut cases = vec![
        Case {
            name: "rename_top_level_matching_ids",
            op: SchemaAlterOp::Rename {
                from: "region".into(),
                to: "region_code".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("region_code"),
            iceberg_lacks: Some("region"),
        },
        Case {
            name: "rename_top_level_sequential_ids",
            op: SchemaAlterOp::Rename {
                from: "region".into(),
                to: "region_code".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("region_code"),
            iceberg_lacks: Some("region"),
        },
        Case {
            name: "rename_dotted_nested_matching_ids",
            op: SchemaAlterOp::Rename {
                from: "detail.truck_reg".into(),
                to: "truck_reg_x".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("truck_reg_x"),
            iceberg_lacks: Some("detail_truck_reg"),
        },
        Case {
            name: "rename_flatten_name_sequential_ids",
            op: SchemaAlterOp::Rename {
                from: "detail_truck_reg".into(),
                to: "truck_reg_x".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("truck_reg_x"),
            iceberg_lacks: Some("detail_truck_reg"),
        },
        Case {
            name: "rename_idempotent_same_name",
            op: SchemaAlterOp::Rename {
                from: "region".into(),
                to: "region".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("region"),
            iceberg_lacks: None,
        },
        Case {
            name: "rename_into_nested_path_matching_ids",
            op: SchemaAlterOp::Rename {
                from: "region".into(),
                to: "detail.region".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("detail_region"),
            iceberg_lacks: Some("region"),
        },
        Case {
            name: "rename_into_nested_path_sequential_ids",
            op: SchemaAlterOp::Rename {
                from: "region".into(),
                to: "detail.region".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("detail_region"),
            iceberg_lacks: Some("region"),
        },
        Case {
            name: "rename_nested_leaf_same_parent",
            op: SchemaAlterOp::Rename {
                from: "detail.truck_reg".into(),
                to: "detail.vehicle_reg".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("detail_vehicle_reg"),
            iceberg_lacks: Some("detail_truck_reg"),
        },
        Case {
            name: "rename_dotted_target_missing_parent",
            op: SchemaAlterOp::Rename {
                from: "region".into(),
                to: "ghost.region".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("not found"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "rename_missing_column",
            op: SchemaAlterOp::Rename {
                from: "no_such_column".into(),
                to: "x".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("not found"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "rename_target_exists",
            op: SchemaAlterOp::Rename {
                from: "region".into(),
                to: "version".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("already exists"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "merge_type_sibling_matching_ids",
            op: SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("price"),
            iceberg_lacks: Some("price_string"),
        },
        Case {
            name: "merge_type_sibling_sequential_ids",
            op: SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("price"),
            iceberg_lacks: Some("price_string"),
        },
        Case {
            name: "merge_flatten_dotted_sequential_ids",
            op: SchemaAlterOp::Merge {
                src: "detail_record.geofence_id".into(),
                dst: "detail.geofence_id".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("detail_geofence_id"),
            iceberg_lacks: Some("detail_record_geofence_id"),
        },
        Case {
            name: "merge_flatten_iceberg_names",
            op: SchemaAlterOp::Merge {
                src: "detail_record_geofence_id".into(),
                dst: "detail_geofence_id".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("detail_geofence_id"),
            iceberg_lacks: Some("detail_record_geofence_id"),
        },
        Case {
            name: "merge_src_equals_dst",
            op: SchemaAlterOp::Merge {
                src: "price".into(),
                dst: "price".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("must differ"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "merge_missing_src",
            op: SchemaAlterOp::Merge {
                src: "missing_src".into(),
                dst: "price".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("not found"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "merge_missing_dst",
            op: SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "missing_dst".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("not found"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "merge_nested_record_without_top_level_iceberg_id",
            op: SchemaAlterOp::Merge {
                src: "detail.pour_location".into(),
                dst: "detail.geofence_id".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: false,
            err_contains: Some("nested ALTER is not supported"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "drop_top_level_matching_ids",
            op: SchemaAlterOp::Drop {
                column: "note".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("region"),
            iceberg_lacks: Some("note"),
        },
        Case {
            name: "drop_top_level_sequential_ids",
            op: SchemaAlterOp::Drop {
                column: "note".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("region"),
            iceberg_lacks: Some("note"),
        },
        Case {
            name: "drop_flatten_name",
            op: SchemaAlterOp::Drop {
                column: "detail_truck_reg".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("region"),
            iceberg_lacks: Some("detail_truck_reg"),
        },
        Case {
            name: "drop_dotted_nested",
            op: SchemaAlterOp::Drop {
                column: "detail.truck_reg".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("region"),
            iceberg_lacks: Some("detail_truck_reg"),
        },
        Case {
            name: "drop_already_absent_is_idempotent",
            op: SchemaAlterOp::Drop {
                column: "ghost".into(),
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("not found"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "drop_nested_record_without_top_level_iceberg_id",
            op: SchemaAlterOp::Drop {
                column: "detail.pour_location".into(),
            },
            id_mode: IdMode::Sequential,
            expect_ok: false,
            err_contains: Some("nested ALTER is not supported"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
        Case {
            name: "promote_identity_integer",
            op: SchemaAlterOp::Promote {
                column: "version".into(),
                to: SkipprDataType::Integer,
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("version"),
            iceberg_lacks: None,
        },
        Case {
            name: "promote_integer_to_long_sequential_ids",
            op: SchemaAlterOp::Promote {
                column: "version".into(),
                to: SkipprDataType::Long,
            },
            id_mode: IdMode::Sequential,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some("version"),
            iceberg_lacks: None,
        },
        Case {
            name: "promote_missing_column",
            op: SchemaAlterOp::Promote {
                column: "nope".into(),
                to: SkipprDataType::Long,
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("not found"),
            iceberg_has: None,
            iceberg_lacks: None,
        },
    ];

    for (from, to) in LEGAL_PROMOTES {
        cases.push(Case {
            name: "promote_legal",
            op: SchemaAlterOp::Promote {
                column: promote_column(from).into(),
                to: to.clone(),
            },
            id_mode: IdMode::Matching,
            expect_ok: true,
            err_contains: None,
            iceberg_has: Some(promote_column(from)),
            iceberg_lacks: None,
        });
    }
    for (from, to) in ILLEGAL_PROMOTES {
        cases.push(Case {
            name: "promote_illegal",
            op: SchemaAlterOp::Promote {
                column: promote_column(from).into(),
                to: to.clone(),
            },
            id_mode: IdMode::Matching,
            expect_ok: false,
            err_contains: Some("cannot promote"),
            iceberg_has: None,
            iceberg_lacks: None,
        });
    }
    cases
}

fn run_case(case: &Case) -> Result<Schema, String> {
    let mut meta = fixture_meta();
    let iceberg = fixture_iceberg(case.id_mode);
    let op = canonicalize_op(&meta, &case.op).map_err(|err| err.to_string())?;
    let field_id = field_id_for_op(&meta, &op).map_err(|err| err.to_string())?;
    apply(&mut meta, &op).map_err(|err| err.to_string())?;
    iceberg_schema_after_alter(&iceberg, &op, field_id)
}

fn case_label(case: &Case, idx: usize) -> String {
    if case.name == "promote_legal" || case.name == "promote_illegal" {
        format!("{}[{idx}] {:?}", case.name, case.op)
    } else {
        case.name.to_string()
    }
}

#[test]
fn alter_schema_matrix_is_fully_enumerated() {
    let mut failures = Vec::new();
    for (idx, case) in matrix().iter().enumerate() {
        let label = case_label(case, idx);
        match run_case(case) {
            Ok(schema) => {
                if !case.expect_ok {
                    failures.push(format!("{label}: expected error, got ok"));
                    continue;
                }
                if let Some(name) = case.iceberg_has {
                    if schema.field_by_name(name).is_none() {
                        failures.push(format!("{label}: Iceberg missing {name}"));
                    }
                }
                if let Some(name) = case.iceberg_lacks {
                    if schema.field_by_name(name).is_some() {
                        failures.push(format!("{label}: Iceberg still has {name}"));
                    }
                }
            }
            Err(err) => {
                if case.expect_ok {
                    failures.push(format!("{label}: expected ok, got {err}"));
                    continue;
                }
                if let Some(needle) = case.err_contains {
                    if !err.contains(needle) {
                        failures.push(format!("{label}: error {err:?} lacks {needle:?}"));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "ALTER matrix failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn alter_schema_matrix_covers_every_op_and_legal_promote() {
    let cases = matrix();
    assert!(cases
        .iter()
        .any(|c| matches!(c.op, SchemaAlterOp::Rename { .. }) && c.expect_ok));
    assert!(cases
        .iter()
        .any(|c| matches!(c.op, SchemaAlterOp::Rename { .. }) && !c.expect_ok));
    assert!(cases
        .iter()
        .any(|c| matches!(c.op, SchemaAlterOp::Merge { .. }) && c.expect_ok));
    assert!(cases
        .iter()
        .any(|c| matches!(c.op, SchemaAlterOp::Merge { .. }) && !c.expect_ok));
    assert!(cases
        .iter()
        .any(|c| matches!(c.op, SchemaAlterOp::Drop { .. }) && c.expect_ok));
    assert!(cases
        .iter()
        .any(|c| matches!(c.op, SchemaAlterOp::Drop { .. }) && !c.expect_ok));
    assert!(cases
        .iter()
        .any(|c| matches!(c.op, SchemaAlterOp::Promote { .. }) && c.expect_ok));
    assert!(cases
        .iter()
        .any(|c| matches!(c.op, SchemaAlterOp::Promote { .. }) && !c.expect_ok));
    assert!(cases
        .iter()
        .any(|c| matches!(c.id_mode, IdMode::Sequential)));
    assert!(cases.iter().any(|c| matches!(c.id_mode, IdMode::Matching)));

    for (from, to) in LEGAL_PROMOTES {
        assert!(
            is_iceberg_legal_promote(from, to),
            "{from:?} → {to:?} must stay legal"
        );
        assert!(
            cases.iter().any(|c| matches!(
                &c.op,
                SchemaAlterOp::Promote { column, to: got }
                    if column == promote_column(from) && got == to && c.expect_ok
            )),
            "matrix missing legal promote {from:?} → {to:?}"
        );
    }
    for (from, to) in ILLEGAL_PROMOTES {
        assert!(!is_iceberg_legal_promote(from, to));
        assert!(
            cases.iter().any(|c| matches!(
                &c.op,
                SchemaAlterOp::Promote { to: got, .. } if got == to && !c.expect_ok
            )),
            "matrix missing illegal promote {from:?} → {to:?}"
        );
    }
}

#[test]
fn alter_sql_matrix_parses_and_rejects_add() {
    let parsed = [
        (
            "ALTER TABLE s3_alter RENAME COLUMN region TO region_code",
            SchemaAlterOp::Rename {
                from: "region".into(),
                to: "region_code".into(),
            },
        ),
        (
            "ALTER TABLE s3_alter RENAME COLUMN region TO detail.region",
            SchemaAlterOp::Rename {
                from: "region".into(),
                to: "detail.region".into(),
            },
        ),
        (
            "ALTER TABLE s3_alter.s3_alter MERGE COLUMN price_string INTO price",
            SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
        ),
        (
            "ALTER TABLE s3_alter DROP COLUMN note",
            SchemaAlterOp::Drop {
                column: "note".into(),
            },
        ),
        (
            "ALTER TABLE s3_alter ALTER COLUMN version TYPE BIGINT",
            SchemaAlterOp::Promote {
                column: "version".into(),
                to: SkipprDataType::Long,
            },
        ),
        (
            "ALTER TABLE s3_alter ALTER COLUMN n_float TYPE DOUBLE",
            SchemaAlterOp::Promote {
                column: "n_float".into(),
                to: SkipprDataType::Double,
            },
        ),
        (
            "ALTER TABLE s3_alter ALTER COLUMN n_short TYPE INTEGER",
            SchemaAlterOp::Promote {
                column: "n_short".into(),
                to: SkipprDataType::Integer,
            },
        ),
    ];
    for (sql, expected) in parsed {
        let mut parser = SParser::new(sql).unwrap();
        match parser.parse_statement().unwrap() {
            Statement::AlterTable(stmt) => assert_eq!(stmt.op, expected, "{sql}"),
            other => panic!("{sql}: expected AlterTable, got {other:?}"),
        }
    }

    let mut add = SParser::new("ALTER TABLE s3_alter ADD COLUMN extra STRING").unwrap();
    let err = add.parse_statement().unwrap_err().to_string();
    assert!(err.contains("ADD COLUMN"), "{err}");

    let mut schema = SParser::new("ALTER SCHEMA s3_alter DROP COLUMN note").unwrap();
    let err = schema.parse_statement().unwrap_err().to_string();
    assert!(err.contains("TABLE"), "{err}");
}
