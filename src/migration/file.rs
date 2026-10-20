use crate::discover::schema_alter::SchemaAlterOp;
use crate::sqlrt::parser::{SParser, Statement};
use sha2::{Digest, Sha256};
use std::path::Path;

use super::journal::{MigrationChecksum, MigrationId};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MigrationFileError {
    #[error("{0}")]
    Parse(String),
    #[error("down() is only valid for Rename or Promote; {id} includes {op}")]
    DownNotReversible { id: String, op: String },
    #[error("migration '{0}' has no ALTER TABLE statements")]
    Empty(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlterStmt {
    pub pipeline: String,
    pub namespace: String,
    pub op: SchemaAlterOp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedMigration {
    pub id: MigrationId,
    pub checksum: MigrationChecksum,
    pub up: Vec<AlterStmt>,
    pub down: Option<Vec<AlterStmt>>,
    pub up_sql: String,
}

pub fn load_sql_file(id: &str, sql: &str) -> Result<LoadedMigration, MigrationFileError> {
    let (up_sql, down_sql) = split_up_down(sql);
    let up = parse_alters(&up_sql)?;
    if up.is_empty() {
        return Err(MigrationFileError::Empty(id.to_string()));
    }
    let checksum = MigrationChecksum::of_file(up_sql.trim(), down_sql.as_deref().map(str::trim));
    let down = match down_sql {
        Some(down_sql) if !down_sql.trim().is_empty() => {
            let down = parse_alters(&down_sql)?;
            reject_irreversible_down(id, &up, &down)?;
            Some(down)
        }
        _ => None,
    };
    Ok(LoadedMigration {
        id: MigrationId::new(id),
        checksum,
        up,
        down,
        up_sql: up_sql.trim().to_string(),
    })
}

pub fn write_sql_file(
    dir: &Path,
    id: &str,
    up_sql: &str,
    down_sql: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    let path = dir.join(format!("{id}.sql"));
    let mut body = up_sql.trim().to_string();
    body.push('\n');
    if let Some(down) = down_sql.map(str::trim).filter(|s| !s.is_empty()) {
        body.push_str("\n-- migrate:down\n");
        body.push_str(down);
        body.push('\n');
    }
    std::fs::write(&path, body).map_err(|err| err.to_string())?;
    Ok(path)
}

pub fn load_sql_path(path: &Path) -> Result<LoadedMigration, MigrationFileError> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| MigrationFileError::Parse("migration file has no stem".into()))?;
    let sql =
        std::fs::read_to_string(path).map_err(|err| MigrationFileError::Parse(err.to_string()))?;
    load_sql_file(stem, &sql)
}

pub fn list_sql_files(dir: &Path) -> Result<Vec<LoadedMigration>, MigrationFileError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|err| MigrationFileError::Parse(err.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|err| MigrationFileError::Parse(err.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        files.push(path);
    }
    files.sort();
    files.into_iter().map(|path| load_sql_path(&path)).collect()
}

fn split_up_down(sql: &str) -> (String, Option<String>) {
    let mut up = String::new();
    let mut down = None;
    let mut in_down = false;
    for line in sql.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("-- migrate:down") {
            in_down = true;
            down = Some(String::new());
            continue;
        }
        if trimmed.eq_ignore_ascii_case("-- migrate:up") {
            continue;
        }
        if in_down {
            if let Some(buf) = down.as_mut() {
                buf.push_str(line);
                buf.push('\n');
            }
        } else {
            up.push_str(line);
            up.push('\n');
        }
    }
    (up, down)
}

fn parse_alters(sql: &str) -> Result<Vec<AlterStmt>, MigrationFileError> {
    let stmts = SParser::parse_sql_with_dialect(
        sql,
        &datafusion::sql::sqlparser::dialect::GenericDialect {},
    )
    .map_err(|err| MigrationFileError::Parse(err.to_string()))?;
    let mut out = Vec::new();
    for stmt in stmts {
        match stmt {
            Statement::AlterTable(alter) => {
                let pipeline = format!("{}", alter.pipeline).replace('"', "");
                let namespace = alter
                    .namespace
                    .as_ref()
                    .map(|ns| format!("{ns}").replace('"', ""))
                    .unwrap_or_else(|| pipeline.clone());
                out.push(AlterStmt {
                    pipeline,
                    namespace,
                    op: alter.op,
                });
            }
            other => {
                return Err(MigrationFileError::Parse(format!(
                    "migrations may only contain ALTER TABLE; found {other:?}"
                )));
            }
        }
    }
    Ok(out)
}

fn reject_irreversible_down(
    id: &str,
    up: &[AlterStmt],
    down: &[AlterStmt],
) -> Result<(), MigrationFileError> {
    let irreversible =
        |op: &SchemaAlterOp| matches!(op, SchemaAlterOp::Drop { .. } | SchemaAlterOp::Merge { .. });
    if up.iter().any(|stmt| irreversible(&stmt.op))
        || down.iter().any(|stmt| irreversible(&stmt.op))
    {
        let op = up
            .iter()
            .chain(down.iter())
            .find(|stmt| irreversible(&stmt.op))
            .map(|stmt| op_name(&stmt.op))
            .unwrap_or("Drop/Merge");
        return Err(MigrationFileError::DownNotReversible {
            id: id.to_string(),
            op: op.to_string(),
        });
    }
    Ok(())
}

fn op_name(op: &SchemaAlterOp) -> &'static str {
    match op {
        SchemaAlterOp::Rename { .. } => "Rename",
        SchemaAlterOp::Merge { .. } => "Merge",
        SchemaAlterOp::Drop { .. } => "Drop",
        SchemaAlterOp::Promote { .. } => "Promote",
    }
}

impl MigrationChecksum {
    pub fn of(sql: &str) -> Self {
        Self::of_file(sql, None)
    }

    pub fn of_file(up: &str, down: Option<&str>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(up.as_bytes());
        if let Some(down) = down.filter(|s| !s.is_empty()) {
            hasher.update(b"\n-- migrate:down\n");
            hasher.update(down.as_bytes());
        }
        Self(format!("{:x}", hasher.finalize()))
    }
}
