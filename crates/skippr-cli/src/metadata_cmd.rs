use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::Value;

use skipprd::discover::metadata_apply::{apply_namespace_field_rows, MetadataFieldRow};
use skipprd::discover::{Metadata, PipelineMetadata};
use skipprd::helpers::configuration::Config;

use crate::{is_json_output, print_json};

#[derive(Subcommand, Debug)]
pub enum MetadataAction {
    /// Show persisted pipeline metadata (per-namespace field list).
    Show(MetadataShowArgs),
    /// Apply reviewed schema fields for one namespace.
    Apply(MetadataApplyArgs),
}

#[derive(Parser, Debug, Clone)]
pub struct MetadataShowArgs {
    /// The pipeline to use.
    #[arg(short, long)]
    pub pipeline: String,
    /// Output mode: json or text.
    #[arg(long, default_value = "json")]
    pub output: String,
}

#[derive(Parser, Debug, Clone)]
pub struct MetadataApplyArgs {
    /// The pipeline to use.
    #[arg(short, long)]
    pub pipeline: String,
    /// Namespace to update.
    #[arg(long)]
    pub namespace: String,
    /// Path to JSON file `{ "fields": [ { "name", "field_type", "nullable" } ] }`, or `-` for stdin.
    #[arg(long)]
    pub schema: Option<PathBuf>,
    /// Inline schema JSON (same shape as `--schema` file). Used when passing JSON from the IDE.
    #[arg(long = "schema-json")]
    pub schema_json: Option<String>,
    /// Mark schema as evolved so schema sinks are synced (default: true).
    #[arg(long, default_value_t = true)]
    pub evolved: bool,
    /// Output mode: json or text.
    #[arg(long, default_value = "json")]
    pub output: String,
}

#[derive(Serialize)]
struct MetadataShowResponse {
    ok: bool,
    pipeline: String,
    namespaces: Vec<MetadataNamespaceJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct MetadataNamespaceJson {
    namespace: String,
    fields: Value,
}

#[derive(Serialize)]
struct MetadataApplyResponse {
    ok: bool,
    namespace: String,
    fields_written: usize,
    evolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn schema_fields_json(metadata: &Metadata) -> Value {
    let mut fields = metadata.field_details();
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    serde_json::json!(fields
        .into_iter()
        .map(|(name, field_type, nullable)| {
            serde_json::json!({
                "name": name,
                "field_type": field_type,
                "nullable": nullable
            })
        })
        .collect::<Vec<_>>())
}

fn emit_metadata_json<T: Serialize>(output: &str, value: &T) {
    if is_json_output(output) {
        print_json(value);
    } else {
        print_json(value);
    }
}

fn parse_schema_fields(raw: &str) -> Result<Vec<MetadataFieldRow>, String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid schema JSON: {e}"))?;
    let fields_value = match value.get("fields") {
        Some(fields) => fields,
        _ if value.is_array() => &value,
        _ => {
            return Err("schema JSON must contain a \"fields\" array".to_string());
        }
    };
    let fields_array = fields_value
        .as_array()
        .ok_or_else(|| "schema \"fields\" must be an array".to_string())?;
    let mut rows = Vec::with_capacity(fields_array.len());
    for item in fields_array {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "each field must have a non-empty \"name\"".to_string())?;
        let field_type = item
            .get("field_type")
            .or_else(|| item.get("type"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("field '{name}' must have \"field_type\""))?;
        let nullable = item
            .get("nullable")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        rows.push(MetadataFieldRow {
            name: name.to_string(),
            field_type: field_type.to_string(),
            nullable,
        });
    }
    Ok(rows)
}

async fn load_schema_fields(args: &MetadataApplyArgs) -> Result<Vec<MetadataFieldRow>, String> {
    if let Some(inline) = args.schema_json.as_ref().filter(|s| !s.trim().is_empty()) {
        return parse_schema_fields(inline);
    }
    let Some(path) = args.schema.as_ref() else {
        return Err("one of --schema or --schema-json is required".to_string());
    };
    let raw = if path.as_os_str() == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .map_err(|e| format!("failed to read schema from stdin: {e}"))?;
        buf
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read schema file '{}': {e}", path.display()))?
    };
    parse_schema_fields(&raw)
}

pub async fn run_metadata_show(output: &str) {
    let pipeline_name = Config::get_pipeline_name();
    let response = match Config::get_metadata().await {
        Ok(pipeline_metadata) => metadata_show_ok(&pipeline_metadata),
        Err(_) => MetadataShowResponse {
            ok: true,
            pipeline: pipeline_name,
            namespaces: Vec::new(),
            error: None,
        },
    };
    if !response.ok {
        emit_metadata_json(output, &response);
        std::process::exit(1);
    }
    emit_metadata_json(output, &response);
}

fn metadata_show_ok(pipeline_metadata: &PipelineMetadata) -> MetadataShowResponse {
    let mut namespaces: Vec<MetadataNamespaceJson> = pipeline_metadata
        .metadata
        .iter()
        .map(|(namespace, metadata)| MetadataNamespaceJson {
            namespace: namespace.clone(),
            fields: schema_fields_json(metadata),
        })
        .collect();
    namespaces.sort_by(|a, b| a.namespace.cmp(&b.namespace));
    MetadataShowResponse {
        ok: true,
        pipeline: pipeline_metadata.name.clone(),
        namespaces,
        error: None,
    }
}

pub async fn run_metadata_apply(args: &MetadataApplyArgs) {
    let output = args.output.clone();
    let namespace = args.namespace.clone();
    let evolved = args.evolved;
    let fields = match load_schema_fields(args).await {
        Ok(fields) => fields,
        Err(error) => {
            emit_metadata_json(
                &output,
                &MetadataApplyResponse {
                    ok: false,
                    namespace,
                    fields_written: 0,
                    evolved,
                    error: Some(error),
                },
            );
            std::process::exit(1);
        }
    };
    match apply_namespace_field_rows(&args.namespace, &fields, evolved).await {
        Ok(fields_written) => {
            emit_metadata_json(
                &output,
                &MetadataApplyResponse {
                    ok: true,
                    namespace: args.namespace.clone(),
                    fields_written,
                    evolved,
                    error: None,
                },
            );
        }
        Err(error) => {
            emit_metadata_json(
                &output,
                &MetadataApplyResponse {
                    ok: false,
                    namespace: args.namespace.clone(),
                    fields_written: 0,
                    evolved,
                    error: Some(error),
                },
            );
            std::process::exit(1);
        }
    }
}
