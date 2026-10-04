use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use iceberg::spec::{FormatVersion, Schema, TableMetadata, UnboundPartitionSpec};
use iceberg::{
    NamespaceIdent, TableCommit, TableCreation, TableIdent, TableRequirement, TableUpdate,
};
use serde_derive::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::RestError;
use crate::RestState;

#[derive(Deserialize)]
pub(crate) struct CreateNamespaceRequest {
    namespace: Vec<String>,
    #[serde(default)]
    properties: HashMap<String, String>,
}

#[derive(Deserialize)]
pub(crate) struct UpdateNamespaceRequest {
    #[serde(default)]
    updates: HashMap<String, String>,
    #[serde(default)]
    removals: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct CreateTableRequest {
    name: String,
    schema: Schema,
    #[serde(default, rename = "partition-spec")]
    partition_spec: Option<UnboundPartitionSpec>,
    #[serde(default)]
    properties: HashMap<String, String>,
    location: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct CommitTableRequest {
    #[serde(default)]
    identifier: Option<TableIdent>,
    #[serde(default)]
    requirements: Vec<TableRequirement>,
    #[serde(default)]
    updates: Vec<TableUpdate>,
}

#[derive(Deserialize)]
pub(crate) struct RenameTableRequest {
    source: TableIdent,
    destination: TableIdent,
}

#[derive(Serialize)]
pub(crate) struct LoadTableResult {
    #[serde(rename = "metadata-location")]
    metadata_location: String,
    metadata: TableMetadata,
    config: HashMap<String, String>,
}

pub fn parse_namespace(raw: &str) -> Result<NamespaceIdent, RestError> {
    let parts: Vec<String> = raw
        .split('\u{001f}')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect();
    NamespaceIdent::from_vec(parts).map_err(RestError::from)
}

fn namespace_key(ns: &NamespaceIdent) -> String {
    ns.to_string()
}

fn forbid_ingest_write(state: &RestState, ns: &NamespaceIdent) -> Result<(), RestError> {
    if state.ingest_namespaces.contains(&namespace_key(ns)) {
        return Err(RestError::Forbidden(format!(
            "ingest namespace '{ns}' is read-only over REST"
        )));
    }
    Ok(())
}

fn reject_pipeline_name(state: &RestState, ns: &NamespaceIdent) -> Result<(), RestError> {
    if state.pipeline_names.contains(&namespace_key(ns)) {
        return Err(RestError::AlreadyExists(format!(
            "namespace '{ns}' equals a pipeline name"
        )));
    }
    Ok(())
}

pub async fn list_namespaces(State(state): State<RestState>) -> Result<Json<Value>, RestError> {
    let namespaces = state.catalog.list_namespaces(None).await?;
    Ok(Json(json!({
        "namespaces": namespaces.iter().map(|ns| ns.as_ref()).collect::<Vec<_>>()
    })))
}

pub async fn create_namespace(
    State(state): State<RestState>,
    Json(req): Json<CreateNamespaceRequest>,
) -> Result<impl IntoResponse, RestError> {
    let ns = NamespaceIdent::from_vec(req.namespace).map_err(RestError::from)?;
    reject_pipeline_name(&state, &ns)?;
    forbid_ingest_write(&state, &ns)?;
    let created = state.catalog.create_namespace(&ns, req.properties).await?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "namespace": created.name().as_ref(),
            "properties": created.properties()
        })),
    ))
}

pub async fn get_namespace(
    State(state): State<RestState>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, RestError> {
    let ns = parse_namespace(&raw)?;
    let got = state.catalog.get_namespace(&ns).await?;
    Ok(Json(json!({
        "namespace": got.name().as_ref(),
        "properties": got.properties()
    })))
}

pub async fn namespace_exists(
    State(state): State<RestState>,
    Path(raw): Path<String>,
) -> Result<StatusCode, RestError> {
    let ns = parse_namespace(&raw)?;
    if state.catalog.namespace_exists(&ns).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(RestError::NoSuchNamespace(ns.to_string()))
    }
}

pub async fn drop_namespace(
    State(state): State<RestState>,
    Path(raw): Path<String>,
) -> Result<StatusCode, RestError> {
    let ns = parse_namespace(&raw)?;
    forbid_ingest_write(&state, &ns)?;
    state.catalog.drop_namespace(&ns).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn update_namespace(
    State(state): State<RestState>,
    Path(raw): Path<String>,
    Json(req): Json<UpdateNamespaceRequest>,
) -> Result<Json<Value>, RestError> {
    let ns = parse_namespace(&raw)?;
    forbid_ingest_write(&state, &ns)?;
    let current = state.catalog.get_namespace(&ns).await?;
    let mut properties = current.properties().clone();
    for key in req.removals {
        properties.remove(&key);
    }
    properties.extend(req.updates);
    state
        .catalog
        .update_namespace(&ns, properties.clone())
        .await?;
    Ok(Json(json!({
        "namespace": ns.as_ref(),
        "properties": properties
    })))
}

pub async fn list_tables(
    State(state): State<RestState>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, RestError> {
    let ns = parse_namespace(&raw)?;
    let identifiers = state.catalog.list_tables(&ns).await?;
    Ok(Json(json!({ "identifiers": identifiers })))
}

pub async fn create_table(
    State(state): State<RestState>,
    Path(raw): Path<String>,
    Json(req): Json<CreateTableRequest>,
) -> Result<Json<LoadTableResult>, RestError> {
    let ns = parse_namespace(&raw)?;
    forbid_ingest_write(&state, &ns)?;
    let location = req.location.unwrap_or_else(|| {
        skippr_iceberg_catalog::iceberg_table_location(
            &state.warehouse,
            &ns.as_ref().join("/"),
            &req.name,
        )
    });
    let table = state
        .catalog
        .create_table(
            &ns,
            TableCreation {
                name: req.name,
                location: Some(location),
                schema: req.schema,
                partition_spec: req.partition_spec,
                sort_order: None,
                properties: req.properties,
                format_version: FormatVersion::V2,
            },
        )
        .await?;
    load_result(table)
}

pub async fn load_table(
    State(state): State<RestState>,
    Path((raw, table)): Path<(String, String)>,
) -> Result<Json<LoadTableResult>, RestError> {
    let ns = parse_namespace(&raw)?;
    let ident = TableIdent::new(ns, table);
    let table = state.catalog.load_table(&ident).await?;
    load_result(table)
}

pub async fn table_exists(
    State(state): State<RestState>,
    Path((raw, table)): Path<(String, String)>,
) -> Result<StatusCode, RestError> {
    let ns = parse_namespace(&raw)?;
    let ident = TableIdent::new(ns, table);
    if state.catalog.table_exists(&ident).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(RestError::NoSuchTable(ident.to_string()))
    }
}

pub async fn commit_table(
    State(state): State<RestState>,
    Path((raw, table)): Path<(String, String)>,
    Json(req): Json<CommitTableRequest>,
) -> Result<Json<LoadTableResult>, RestError> {
    let ns = parse_namespace(&raw)?;
    forbid_ingest_write(&state, &ns)?;
    let ident = req.identifier.unwrap_or_else(|| TableIdent::new(ns, table));
    let table = state
        .catalog
        .update_table(TableCommit::from_rest(ident, req.requirements, req.updates))
        .await?;
    load_result(table)
}

pub async fn drop_table(
    State(state): State<RestState>,
    Path((raw, table)): Path<(String, String)>,
) -> Result<StatusCode, RestError> {
    let ns = parse_namespace(&raw)?;
    forbid_ingest_write(&state, &ns)?;
    let ident = TableIdent::new(ns, table);
    state.catalog.drop_table(&ident).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn rename_table(
    State(state): State<RestState>,
    Json(req): Json<RenameTableRequest>,
) -> Result<StatusCode, RestError> {
    forbid_ingest_write(&state, req.source.namespace())?;
    forbid_ingest_write(&state, req.destination.namespace())?;
    state
        .catalog
        .rename_table(&req.source, &req.destination)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn load_result(table: iceberg::table::Table) -> Result<Json<LoadTableResult>, RestError> {
    let metadata_location = table
        .metadata_location_result()
        .map_err(RestError::from)?
        .to_string();
    Ok(Json(LoadTableResult {
        metadata_location,
        metadata: table.metadata().clone(),
        config: HashMap::new(),
    }))
}
