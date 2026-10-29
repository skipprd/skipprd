//! Engine-owned schema migrations. The skippr store journal is the apply SoT.
//!
//! Disk artifacts are `migrations/*.sql` only. Python `Migration.write()` emits
//! those files; the engine never loads `.py`.

mod apply;
mod file;
pub(crate) mod journal;
mod wal;

pub use apply::{
    apply_migration, apply_pending_files, apply_pending_with_ingest_lease, apply_query_alter,
    require_native_alter_support, schema_barrier_held,
};
pub use file::{load_sql_file, write_sql_file, LoadedMigration, MigrationFileError};
pub use journal::{
    journal_key, MigrationApply, MigrationChecksum, MigrationDirection, MigrationId,
    MigrationPhase, MigrationRecord,
};
pub use wal::rewrite_record_batch;

use crate::helpers::configuration::Config;
use std::path::PathBuf;

/// Directory next to skippr.yml (`migrations/`).
pub fn migrations_dir(_config: &Config) -> PathBuf {
    let file = Config::find_config_file();
    PathBuf::from(file)
        .parent()
        .map(|parent| parent.join("migrations"))
        .unwrap_or_else(|| PathBuf::from("migrations"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::schema_alter::SchemaAlterOp;
    use crate::plugins::AlterSupport;

    #[test]
    fn sql_file_parses_up_and_optional_down() {
        let sql = "\
-- migrate:up
ALTER TABLE bikehire.trips RENAME COLUMN region TO region_code;
-- migrate:down
ALTER TABLE bikehire.trips RENAME COLUMN region_code TO region;
";
        let loaded = load_sql_file("20261020_rename_region", sql).unwrap();
        assert_eq!(loaded.id.as_str(), "20261020_rename_region");
        assert_eq!(loaded.up.len(), 1);
        assert_eq!(loaded.down.as_ref().map(|ops| ops.len()), Some(1));
        assert!(matches!(
            loaded.up[0].op,
            SchemaAlterOp::Rename { ref from, ref to }
                if from == "region" && to == "region_code"
        ));
    }

    #[test]
    fn down_after_drop_or_merge_fails_closed() {
        let drop_down = "\
ALTER TABLE p.ns DROP COLUMN leftover;
-- migrate:down
ALTER TABLE p.ns RENAME COLUMN a TO b;
";
        let err = load_sql_file("bad_drop_down", drop_down).unwrap_err();
        assert!(
            format!("{err}").contains("down() is only valid for Rename or Promote"),
            "{err}"
        );

        let merge_down = "\
ALTER TABLE p.ns MERGE COLUMN price_string INTO price;
-- migrate:down
ALTER TABLE p.ns RENAME COLUMN a TO b;
";
        let err = load_sql_file("bad_merge_down", merge_down).unwrap_err();
        assert!(
            format!("{err}").contains("down() is only valid for Rename or Promote"),
            "{err}"
        );
    }

    #[test]
    fn query_alter_id_is_checksum_prefixed() {
        let checksum = MigrationChecksum::of("ALTER TABLE p.ns DROP COLUMN leftover;");
        let id = MigrationId::query(checksum);
        assert!(id.as_str().starts_with("query-"));
        assert_eq!(id.as_str().len(), "query-".len() + 64);
    }

    #[test]
    fn checksum_mismatch_is_a_typed_journal_error() {
        let a = MigrationChecksum::of("one");
        let b = MigrationChecksum::of("two");
        assert_ne!(a, b);
        let err = journal::JournalError::ChecksumMismatch {
            id: MigrationId::new("m1"),
            expected: a,
            actual: b,
        };
        assert!(format!("{err}").contains("checksum mismatch"));
    }

    #[test]
    fn alter_support_native_is_distinct_from_unsupported() {
        assert_ne!(AlterSupport::Native, AlterSupport::Unsupported);
    }

    #[test]
    fn pipeline_metadata_has_no_sql_field() {
        let src = include_str!("../discover/mod.rs");
        let prod = src.split("mod tests").next().unwrap();
        assert!(
            !prod.contains("pub sql:"),
            "PipelineMetadata.sql must be deleted"
        );
        assert!(
            !prod.contains("fn append_sql"),
            "append_sql must be deleted with PipelineMetadata.sql"
        );
    }
}
