//! Writer CREATE / DROP / ALTER of declared compute. Reader processes fail closed, like ALTER.

use skippr_wal_catalog::{CatalogFence, KvOp, SkipprStore, StoreFamily};

use crate::discover::PipelineMetadata;
use crate::helpers::configuration::{ComputeItem, Config, SdlDerive, SdlDoc, SdlMeasure};
use crate::helpers::offsets::Offsets;
use crate::sqlrt::parser::Statement;

use super::bind::{bind_drop_compute, bind_statement, BindCatalog};
use super::journal::{self, ir_hash_kind, CatalogEntry, ComputeLog};
use super::parse::{
    desugar_create, desugar_stateless, ComputeAlteration, UnboundCompute, UnboundMeasure,
    UnboundMeasureKind, UnboundRelation, UnboundSelect,
};
use super::{ComputeKind, FieldId, Fqn, Ident, Measure, MeasureKind, NonEmpty, StatelessSelect};

#[derive(Debug, thiserror::Error)]
pub enum ComputeApplyError {
    #[error("{0}")]
    Message(String),
    #[error("CREATE / DROP compute requires a writer Skippr Store (same as ALTER TABLE)")]
    NeedsWriter,
    #[error("type: copy cannot set transform.compute sql or sdl")]
    CopyPipeline,
}

impl From<String> for ComputeApplyError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

pub async fn apply_query_statement(
    config: &Config,
    stmt: &Statement,
) -> Result<String, ComputeApplyError> {
    if config
        .pipeline_type()
        .map(|t| t == crate::pipeline_type::PipelineType::Copy)
        .unwrap_or(false)
    {
        return Err(ComputeApplyError::CopyPipeline);
    }
    let offsets = Offsets::init(config).map_err(|_| ComputeApplyError::NeedsWriter)?;
    let store = offsets
        .open_stream_state()
        .map_err(|_| ComputeApplyError::NeedsWriter)?;
    let fence = offsets
        .writer_fence()
        .ok_or(ComputeApplyError::NeedsWriter)?;
    let metadata = config
        .get_metadata()
        .await
        .map_err(|err| ComputeApplyError::Message(err.to_string()))?;
    match stmt {
        Statement::CreateView { name, as_select } => {
            apply_create(
                config,
                store.as_ref(),
                fence,
                &metadata,
                name,
                as_select,
                true,
            )
            .await
        }
        Statement::CreateTableAs { name, as_select } => {
            apply_create(
                config,
                store.as_ref(),
                fence,
                &metadata,
                name,
                as_select,
                false,
            )
            .await
        }
        Statement::DropCompute { name } => {
            drop_named(config, store.as_ref(), fence, &metadata, name).await
        }
        Statement::AlterCompute { name, alterations } => {
            apply_alter_compute(config, store.as_ref(), fence, &metadata, name, alterations).await
        }
        _ => Err(ComputeApplyError::Message("not a compute statement".into())),
    }
}

async fn apply_create(
    config: &Config,
    store: &dyn SkipprStore,
    fence: CatalogFence,
    metadata: &PipelineMetadata,
    name: &Fqn,
    as_select: &str,
    is_view: bool,
) -> Result<String, ComputeApplyError> {
    let workspace = config.get_workspace_name();
    let pipeline = config.get_pipeline_name();
    if name.pipeline.as_str() != pipeline {
        return Err(ComputeApplyError::Message(
            "CREATE name must match the bound pipeline".into(),
        ));
    }
    let key = journal::journal_key(&workspace, &pipeline)?;
    let log = journal::load(store, &key)?;
    let unbound = desugar_create(name.clone(), as_select, is_view)
        .map_err(|err| ComputeApplyError::Message(err.to_string()))?;
    let catalog = bind_catalog(config, metadata, log.kinds())?;
    if let Some(previous) = log.get(name) {
        let kind = bind_replace(&catalog, unbound)?;
        if ir_hash_kind(&previous.kind)? == ir_hash_kind(&kind)? {
            return Ok(format!("compute {name} unchanged"));
        }
        return Err(ComputeApplyError::Message(format!(
            "compute {name} already exists; ALTER COMPUTE to migrate"
        )));
    }
    let kind = bind_statement(&catalog, unbound)
        .map_err(|err| ComputeApplyError::Message(err.to_string()))?;
    let mut next = log;
    next.insert(kind)?;
    journal::commit_log(store, fence, &key, &next)?;
    Ok(format!("created {name}"))
}

fn bind_replace(
    catalog: &BindCatalog,
    unbound: UnboundCompute,
) -> Result<ComputeKind, ComputeApplyError> {
    let mut without = catalog.clone();
    let name = match &unbound {
        UnboundCompute::Stateless { name, .. }
        | UnboundCompute::View { name, .. }
        | UnboundCompute::Latest { name, .. }
        | UnboundCompute::KeyedTable { name, .. }
        | UnboundCompute::WindowedTable { name, .. }
        | UnboundCompute::StreamTableJoin { name, .. } => name.clone(),
    };
    without.existing.retain(|kind| kind.name() != &name);
    bind_statement(&without, unbound).map_err(|err| ComputeApplyError::Message(err.to_string()))
}

async fn drop_named(
    config: &Config,
    store: &dyn SkipprStore,
    fence: CatalogFence,
    metadata: &PipelineMetadata,
    name: &Fqn,
) -> Result<String, ComputeApplyError> {
    let workspace = config.get_workspace_name();
    let pipeline = config.get_pipeline_name();
    let key = journal::journal_key(&workspace, &pipeline)?;
    let log = journal::load(store, &key)?;
    let catalog = bind_catalog(config, metadata, log.kinds())?;
    bind_drop_compute(&catalog, name).map_err(|err| ComputeApplyError::Message(err.to_string()))?;
    let mut next = log;
    let removed = next
        .items
        .remove(&name.to_string())
        .expect("bind_drop_compute required the name");
    let mut ops = prefix_deletes(store, &pipeline, removed.kind.store_id().unwrap_or(""))?;
    ops.retain(|op| matches!(op, KvOp::Delete { .. }));
    if removed.kind.store_id().is_none() {
        ops.clear();
    }
    ops.push(journal::put_op(&key, &next)?);
    journal::commit_with(store, fence, ops)?;
    Ok(format!("dropped {name}"))
}

pub fn prefix_deletes(
    store: &dyn SkipprStore,
    pipeline: &str,
    id: &str,
) -> Result<Vec<KvOp>, ComputeApplyError> {
    if id.is_empty() {
        return Ok(Vec::new());
    }
    let mut ops = Vec::new();
    for kind in ["agg", "row", "dedup", "ent"] {
        let prefix = StoreFamily::StreamState.key(format!("{kind}/{pipeline}/{id}/").as_bytes());
        let page =
            skippr_wal_catalog::SkipprStoreRead::query_prefix(store, &prefix, None, usize::MAX)
                .map_err(|err| ComputeApplyError::Message(err.to_string()))?;
        for (key, _) in page {
            ops.push(KvOp::Delete { key });
        }
    }
    let meta = StoreFamily::StreamState.key(format!("meta/{pipeline}/{id}").as_bytes());
    if skippr_wal_catalog::SkipprStoreRead::get(store, &meta)
        .map_err(|err| ComputeApplyError::Message(err.to_string()))?
        .is_some()
    {
        ops.push(KvOp::Delete { key: meta });
    }
    Ok(ops)
}

/// Bind config items and put new names. Same hash is a no-op. Hash mismatch
/// requires `ALTER COMPUTE` (SQL/SDL) or `DROP` then `CREATE` (`type: py`).
/// Names in the journal that are not in config (ad-hoc CREATE) are kept.
pub fn admit_config(
    config: &Config,
    store: &dyn SkipprStore,
    fence: CatalogFence,
    metadata: &PipelineMetadata,
) -> Result<ComputeLog, ComputeApplyError> {
    let workspace = config.get_workspace_name();
    let pipeline = config.get_pipeline_name();
    let key = journal::journal_key(&workspace, &pipeline)?;
    let mut log = journal::load(store, &key)?;
    let items = config.get_transform_compute();
    if items.is_empty() {
        return Ok(log);
    }
    if matches!(
        config.pipeline_type(),
        Ok(crate::pipeline_type::PipelineType::Copy)
    ) && items.iter().any(|item| !item.is_py())
    {
        return Err(ComputeApplyError::CopyPipeline);
    }
    let bound = bind_config_items(config, metadata, &items, &log)?;
    let mut dirty = false;
    for kind in bound {
        let name = kind.catalog_key();
        let hash = ir_hash_kind(&kind)?;
        match log.items.get(&name) {
            Some(entry) if entry.hash == hash => {}
            Some(_) => {
                return Err(ComputeApplyError::Message(match &kind {
                    ComputeKind::Py { .. } => super::py::py_changed_message(kind.name()),
                    _ => format!("compute {name} changed; ALTER COMPUTE to migrate"),
                }));
            }
            None => {
                log.insert(kind)?;
                dirty = true;
            }
        }
    }
    if dirty {
        journal::commit_log(store, fence, &key, &log)?;
    }
    Ok(log)
}

fn bind_config_items(
    config: &Config,
    metadata: &PipelineMetadata,
    items: &[ComputeItem],
    log: &ComputeLog,
) -> Result<Vec<ComputeKind>, ComputeApplyError> {
    let pipeline = config.get_pipeline_name();
    let incoming: std::collections::BTreeSet<String> = items
        .iter()
        .filter_map(|item| Fqn::parse_item(&pipeline, item.name()).ok())
        .map(|fqn| fqn.to_string())
        .collect();
    let mut catalog = bind_catalog(config, metadata, log.kinds())?;
    catalog
        .existing
        .retain(|kind| !incoming.contains(&kind.catalog_key()));
    let mut kinds = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        if !seen.insert(item.name()) {
            return Err(ComputeApplyError::Message(format!(
                "duplicate compute name '{}'",
                item.name()
            )));
        }
        let kind = bind_item(config, &catalog, item)?;
        if catalog
            .ingest_namespaces
            .contains(kind.name().name.as_str())
        {
            return Err(ComputeApplyError::Message(format!(
                "compute name '{}' collides with ingest namespace",
                kind.name().name.as_str()
            )));
        }
        catalog.existing.push(kind.clone());
        kinds.push(kind);
    }
    Ok(kinds)
}

fn bind_item(
    config: &Config,
    catalog: &BindCatalog,
    item: &ComputeItem,
) -> Result<ComputeKind, ComputeApplyError> {
    let pipeline = config.get_pipeline_name();
    let name = Fqn::parse_item(&pipeline, item.name())?;
    if name.pipeline.as_str() != pipeline {
        return Err(ComputeApplyError::Message(
            "compute name must match the bound pipeline".into(),
        ));
    }
    match item {
        ComputeItem::Sql { value, .. } => bind_sql_item(config, catalog, name, value),
        ComputeItem::Sdl { value, .. } => {
            let unbound = sdl_to_unbound(&pipeline, &name, value)?;
            bind_statement(catalog, unbound)
                .map_err(|err| ComputeApplyError::Message(err.to_string()))
        }
        ComputeItem::Py { path, handler, .. } => {
            let dir = config.config_dir.as_deref().ok_or_else(|| {
                ComputeApplyError::Message("python compute path requires skippr.yml".into())
            })?;
            let load = super::py::admit_item(dir, path, handler)?;
            Ok(ComputeKind::Py {
                name,
                path: path.clone(),
                handler: handler.clone(),
                digest: load.digest,
            })
        }
    }
}

fn bind_sql_item(
    config: &Config,
    catalog: &BindCatalog,
    name: Fqn,
    sql: &str,
) -> Result<ComputeKind, ComputeApplyError> {
    let trimmed = sql.trim();
    if let Ok(mut parser) = crate::sqlrt::parser::SParser::new(trimmed) {
        if let Ok(stmt) = parser.parse_statement() {
            return match stmt {
                Statement::CreateView {
                    name: create_name,
                    as_select,
                } => {
                    reject_create_name(&name, &create_name)?;
                    bind_named_create(config, catalog, create_name, &as_select, true)
                }
                Statement::CreateTableAs {
                    name: create_name,
                    as_select,
                } => {
                    reject_create_name(&name, &create_name)?;
                    bind_named_create(config, catalog, create_name, &as_select, false)
                }
                Statement::DropCompute { .. } | Statement::AlterCompute { .. } => {
                    Err(ComputeApplyError::Message(
                        "DROP / ALTER in transform.compute is not a skin; ALTER COMPUTE to migrate"
                            .into(),
                    ))
                }
                other => Err(ComputeApplyError::Message(format!(
                    "transform.compute sql does not accept {other:?}"
                ))),
            };
        }
    }
    let unbound = desugar_stateless(name, trimmed)
        .map_err(|err| ComputeApplyError::Message(err.to_string()))?;
    bind_statement(catalog, unbound).map_err(|err| ComputeApplyError::Message(err.to_string()))
}

fn reject_create_name(item: &Fqn, create: &Fqn) -> Result<(), ComputeApplyError> {
    if item != create {
        return Err(ComputeApplyError::Message(format!(
            "CREATE name {create} must equal item name {item}"
        )));
    }
    Ok(())
}

fn bind_named_create(
    config: &Config,
    catalog: &BindCatalog,
    name: Fqn,
    as_select: &str,
    is_view: bool,
) -> Result<ComputeKind, ComputeApplyError> {
    if name.pipeline.as_str() != config.get_pipeline_name() {
        return Err(ComputeApplyError::Message(
            "CREATE name must match the bound pipeline".into(),
        ));
    }
    let unbound = desugar_create(name, as_select, is_view)
        .map_err(|err| ComputeApplyError::Message(err.to_string()))?;
    bind_statement(catalog, unbound).map_err(|err| ComputeApplyError::Message(err.to_string()))
}

fn sdl_to_unbound(
    pipeline: &str,
    item_name: &Fqn,
    doc: &SdlDoc,
) -> Result<UnboundCompute, ComputeApplyError> {
    let check_name = |raw: &Option<String>| -> Result<(), ComputeApplyError> {
        let Some(raw) = raw.as_deref() else {
            return Ok(());
        };
        let fqn = Fqn::parse_item(pipeline, raw)?;
        if &fqn != item_name {
            return Err(ComputeApplyError::Message(format!(
                "SDL name {fqn} must equal item name {item_name}"
            )));
        }
        Ok(())
    };
    match doc {
        SdlDoc::Stateless {
            name,
            target,
            filter,
            derive,
        } => {
            check_name(name)?;
            Ok(UnboundCompute::Stateless {
                name: item_name.clone(),
                target: parse_relation(target)?,
                select: UnboundSelect {
                    filter: filter.clone(),
                    derive: map_derive(derive),
                    star: derive.as_ref().map(|d| d.is_empty()).unwrap_or(true),
                },
            })
        }
        SdlDoc::View {
            name,
            input,
            filter,
            derive,
        } => {
            check_name(name)?;
            Ok(UnboundCompute::View {
                name: item_name.clone(),
                input: parse_relation(input)?,
                select: UnboundSelect {
                    filter: filter.clone(),
                    derive: map_derive(derive),
                    star: derive.as_ref().map(|d| d.is_empty()).unwrap_or(true),
                },
            })
        }
        SdlDoc::Latest {
            name,
            input,
            keys,
            order,
        } => {
            check_name(name)?;
            Ok(UnboundCompute::Latest {
                name: item_name.clone(),
                input: parse_relation(input)?,
                keys: keys.clone(),
                order: parse_order(order.as_deref().unwrap_or(&[]))?,
            })
        }
        SdlDoc::KeyedTable {
            name,
            input,
            keys,
            measures,
        } => {
            check_name(name)?;
            Ok(UnboundCompute::KeyedTable {
                name: item_name.clone(),
                input: parse_relation(input)?,
                keys: keys.clone(),
                measures: map_measures(measures)?,
            })
        }
        SdlDoc::WindowedTable {
            name,
            input,
            keys,
            window,
            event_time,
            measures,
        } => {
            check_name(name)?;
            Ok(UnboundCompute::WindowedTable {
                name: item_name.clone(),
                input: parse_relation(input)?,
                keys: keys.clone(),
                trunc_unit: super::parse_watermark_delay(window)
                    .map_err(|err| ComputeApplyError::Message(err.to_string()))?,
                event_time: event_time.clone(),
                measures: map_measures(measures)?,
            })
        }
        SdlDoc::StreamTableJoin {
            name,
            stream,
            table,
            on,
        } => {
            check_name(name)?;
            let table = Fqn::parse_item(pipeline, table)?;
            if table.pipeline.as_str() != pipeline {
                return Err(ComputeApplyError::Message(
                    "SDL table must match the bound pipeline".into(),
                ));
            }
            Ok(UnboundCompute::StreamTableJoin {
                name: item_name.clone(),
                stream: parse_relation(stream)?,
                table: Some(table),
                keys: on.clone(),
            })
        }
    }
}

fn parse_relation(raw: &str) -> Result<UnboundRelation, ComputeApplyError> {
    let (pipeline, name) = raw.split_once('.').ok_or_else(|| {
        ComputeApplyError::Message(format!("relation '{raw}' must be pipeline.namespace"))
    })?;
    if name.contains('.') {
        return Err(ComputeApplyError::Message(format!(
            "relation '{raw}' must be pipeline.namespace"
        )));
    }
    Ok(UnboundRelation {
        pipeline: pipeline.to_string(),
        name: name.to_string(),
    })
}

fn map_derive(derive: &Option<Vec<SdlDerive>>) -> Vec<(String, String)> {
    derive
        .as_ref()
        .map(|rows| {
            rows.iter()
                .map(|row| (row.name.clone(), row.expr.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn map_measures(measures: &[SdlMeasure]) -> Result<Vec<UnboundMeasure>, ComputeApplyError> {
    measures.iter().map(sdl_measure).collect()
}

fn sdl_measure(measure: &SdlMeasure) -> Result<UnboundMeasure, ComputeApplyError> {
    let kinds = [
        measure
            .sum
            .as_ref()
            .map(|f| UnboundMeasureKind::Sum(f.clone())),
        measure.count.as_ref().map(|_| UnboundMeasureKind::Count),
        measure
            .min
            .as_ref()
            .map(|f| UnboundMeasureKind::Min(f.clone())),
        measure
            .max
            .as_ref()
            .map(|f| UnboundMeasureKind::Max(f.clone())),
    ];
    let present: Vec<_> = kinds.into_iter().flatten().collect();
    if present.len() != 1 {
        return Err(ComputeApplyError::Message(format!(
            "measure '{}' must set exactly one of sum, count, min, max",
            measure.name
        )));
    }
    Ok(UnboundMeasure {
        name: measure.name.clone(),
        kind: present.into_iter().next().expect("len == 1"),
    })
}

fn parse_order(order: &[String]) -> Result<Vec<(String, bool)>, ComputeApplyError> {
    order
        .iter()
        .map(|raw| {
            let raw = raw.trim();
            let (col, desc) = if let Some(col) = raw
                .strip_suffix(" DESC")
                .or_else(|| raw.strip_suffix(" desc"))
            {
                (col.trim(), true)
            } else if let Some(col) = raw
                .strip_suffix(" ASC")
                .or_else(|| raw.strip_suffix(" asc"))
            {
                (col.trim(), false)
            } else {
                (raw, false)
            };
            if col.is_empty() {
                return Err(ComputeApplyError::Message("order column is empty".into()));
            }
            Ok((col.to_string(), desc))
        })
        .collect()
}

fn py_alter_rejected(name: &Fqn) -> ComputeApplyError {
    ComputeApplyError::Message(format!(
        "Py {name} is DROP and CREATE, not ALTER COMPUTE"
    ))
}

async fn apply_alter_compute(
    config: &Config,
    store: &dyn SkipprStore,
    fence: CatalogFence,
    metadata: &PipelineMetadata,
    name: &Fqn,
    alterations: &[ComputeAlteration],
) -> Result<String, ComputeApplyError> {
    let workspace = config.get_workspace_name();
    let pipeline = config.get_pipeline_name();
    let bound_name = qualify_alter_name(&pipeline, name)?;
    let key = journal::journal_key(&workspace, &pipeline)?;
    let mut log = journal::load(store, &key)?;
    let entry = log.get(&bound_name).cloned().ok_or_else(|| {
        ComputeApplyError::Message(format!("compute {bound_name} is not in the catalog"))
    })?;
    if matches!(entry.kind, ComputeKind::Py { .. }) {
        return Err(py_alter_rejected(&bound_name));
    }
    if entry.kind.is_stateless()
        && alterations
            .iter()
            .any(|alt| !matches!(alt, ComputeAlteration::SetFilter { .. }))
    {
        return Err(ComputeApplyError::Message(
            "Stateless compute only accepts ALTER COMPUTE … SET FILTER".into(),
        ));
    }
    let catalog = bind_catalog(config, metadata, log.kinds())?;
    let store_id = entry.kind.store_id().unwrap_or("").to_string();
    let next_kind = apply_alterations(&catalog, entry.kind, alterations)?;
    let next_hash = ir_hash_kind(&next_kind)?;
    if next_hash == entry.hash {
        return Ok(format!("compute {bound_name} unchanged"));
    }
    let mut ops = prefix_deletes(store, &pipeline, &store_id)?;
    log.items.insert(
        bound_name.to_string(),
        CatalogEntry {
            kind: next_kind,
            hash: next_hash,
        },
    );
    ops.push(journal::put_op(&key, &log)?);
    journal::commit_with(store, fence, ops)?;
    Ok(format!("altered {bound_name}"))
}

fn apply_alterations(
    catalog: &BindCatalog,
    kind: ComputeKind,
    alterations: &[ComputeAlteration],
) -> Result<ComputeKind, ComputeApplyError> {
    let mut kind = kind;
    for alteration in alterations {
        kind = apply_one(catalog, kind, alteration)?;
    }
    journal::canonicalize_kind(&kind).map_err(ComputeApplyError::Message)
}

fn apply_one(
    catalog: &BindCatalog,
    kind: ComputeKind,
    alteration: &ComputeAlteration,
) -> Result<ComputeKind, ComputeApplyError> {
    match (kind, alteration) {
        (
            ComputeKind::Stateless {
                name,
                target,
                mut select,
            },
            ComputeAlteration::SetFilter { expr },
        ) => {
            select.filter = Some(expr.clone());
            Ok(ComputeKind::Stateless {
                name,
                target,
                select,
            })
        }
        (ComputeKind::Stateless { .. }, _) => Err(ComputeApplyError::Message(
            "Stateless compute only accepts SET FILTER".into(),
        )),
        (
            ComputeKind::View {
                name,
                input,
                mut select,
            },
            ComputeAlteration::SetFilter { expr },
        ) => {
            select.filter = Some(expr.clone());
            Ok(ComputeKind::View {
                name,
                input,
                select,
            })
        }
        (
            ComputeKind::KeyedTable {
                name,
                input,
                keys,
                measures,
            },
            alteration,
        ) => {
            let measures = alter_measures(catalog, &input, measures, alteration)?;
            Ok(ComputeKind::KeyedTable {
                name,
                input,
                keys,
                measures,
            })
        }
        (
            ComputeKind::WindowedTable {
                name,
                input,
                keys,
                window,
                watermark,
                measures,
            },
            alteration,
        ) => {
            let measures = alter_measures(catalog, &input, measures, alteration)?;
            Ok(ComputeKind::WindowedTable {
                name,
                input,
                keys,
                window,
                watermark,
                measures,
            })
        }
        (ComputeKind::Latest { name, .. }, _) => Err(ComputeApplyError::Message(format!(
            "Latest {name} is DROP and CREATE, not ALTER COMPUTE"
        ))),
        (ComputeKind::StreamTableJoin { name, .. }, _) => Err(ComputeApplyError::Message(format!(
            "StreamTableJoin {name} is DROP and CREATE, not ALTER COMPUTE"
        ))),
        (ComputeKind::Py { name, .. }, _) => Err(py_alter_rejected(&name)),
        (other, ComputeAlteration::RenameColumn { from, to }) => {
            if other.name().name.as_str() == from {
                return Err(ComputeApplyError::Message(
                    "RENAME of the compute name is not ALTER COMPUTE; DROP and CREATE".into(),
                ));
            }
            let _ = to;
            Err(ComputeApplyError::Message(format!(
                "ALTER COMPUTE does not apply {alteration:?} to {}",
                other.name()
            )))
        }
        (other, _) => Err(ComputeApplyError::Message(format!(
            "ALTER COMPUTE does not apply {alteration:?} to {}",
            other.name()
        ))),
    }
}

fn alter_measures(
    catalog: &BindCatalog,
    input: &super::Relation,
    measures: NonEmpty<Measure>,
    alteration: &ComputeAlteration,
) -> Result<NonEmpty<Measure>, ComputeApplyError> {
    let mut rows = measures.as_slice().to_vec();
    match alteration {
        ComputeAlteration::SetMeasure { column, kind } => {
            let idx = rows
                .iter()
                .position(|m| m.name.as_str() == column)
                .ok_or_else(|| {
                    ComputeApplyError::Message(format!("measure '{column}' is not on this compute"))
                })?;
            rows[idx].kind = bind_unbound_measure_kind(catalog, input, kind)?;
        }
        ComputeAlteration::DropColumn { column } => {
            rows.retain(|m| m.name.as_str() != column);
        }
        ComputeAlteration::AddMeasure { name, kind } => {
            if rows.iter().any(|m| m.name.as_str() == name) {
                return Err(ComputeApplyError::Message(format!(
                    "measure '{name}' already exists"
                )));
            }
            rows.push(Measure {
                name: Ident::new(name.clone())?,
                kind: bind_unbound_measure_kind(catalog, input, kind)?,
            });
        }
        ComputeAlteration::RenameColumn { from, to } => {
            let idx = rows
                .iter()
                .position(|m| m.name.as_str() == from)
                .ok_or_else(|| {
                    ComputeApplyError::Message(format!("measure '{from}' is not on this compute"))
                })?;
            rows[idx].name = Ident::new(to.clone())?;
        }
        ComputeAlteration::SetFilter { .. } => {
            return Err(ComputeApplyError::Message(
                "SET FILTER is Stateless / View only".into(),
            ));
        }
    }
    NonEmpty::try_from_vec(rows).map_err(ComputeApplyError::Message)
}

fn bind_unbound_measure_kind(
    catalog: &BindCatalog,
    input: &super::Relation,
    kind: &UnboundMeasureKind,
) -> Result<MeasureKind, ComputeApplyError> {
    let fields = fields_of_input(catalog, input)?;
    match kind {
        UnboundMeasureKind::Count => Ok(MeasureKind::Count),
        UnboundMeasureKind::Sum(col) => Ok(MeasureKind::Sum(field_id(fields, col)?)),
        UnboundMeasureKind::Min(col) => Ok(MeasureKind::Min(field_id(fields, col)?)),
        UnboundMeasureKind::Max(col) => Ok(MeasureKind::Max(field_id(fields, col)?)),
    }
}

fn fields_of_input<'a>(
    catalog: &'a BindCatalog,
    input: &super::Relation,
) -> Result<&'a std::collections::BTreeMap<String, FieldId>, ComputeApplyError> {
    match input {
        super::Relation::PipelineTable {
            pipeline,
            namespace,
        } => catalog
            .ingest
            .get(&(
                pipeline.as_str().to_string(),
                namespace.as_str().to_string(),
            ))
            .ok_or_else(|| {
                ComputeApplyError::Message(format!(
                    "unknown ingest table {}.{}",
                    pipeline.as_str(),
                    namespace.as_str()
                ))
            }),
        other => Err(ComputeApplyError::Message(format!(
            "ALTER COMPUTE measures require an ingest input, not {other:?}"
        ))),
    }
}

fn field_id(
    fields: &std::collections::BTreeMap<String, FieldId>,
    col: &str,
) -> Result<FieldId, ComputeApplyError> {
    fields
        .get(col)
        .copied()
        .ok_or_else(|| ComputeApplyError::Message(format!("unknown column '{col}'")))
}

pub fn bind_catalog(
    config: &Config,
    metadata: &PipelineMetadata,
    existing: Vec<ComputeKind>,
) -> Result<BindCatalog, ComputeApplyError> {
    let pipeline = config.get_pipeline_name();
    let mut ingest = std::collections::BTreeMap::new();
    let mut ingest_namespaces = std::collections::BTreeSet::new();
    for (ns, meta) in &metadata.metadata {
        ingest_namespaces.insert(ns.clone());
        let mut fields = std::collections::BTreeMap::new();
        collect_fields(&mut fields, meta);
        ingest.insert((pipeline.clone(), ns.clone()), fields);
    }
    let event_time = event_time_col(config, &ingest, &pipeline);
    let watermark_delay = config
        .get_transform_config()
        .watermark_delay
        .as_deref()
        .map(super::parse_watermark_delay)
        .transpose()
        .map_err(|err| ComputeApplyError::Message(err.to_string()))?;
    Ok(BindCatalog {
        ingest,
        ingest_namespaces,
        existing,
        event_time,
        watermark_delay,
    })
}

fn collect_fields(
    out: &mut std::collections::BTreeMap<String, FieldId>,
    meta: &crate::discover::Metadata,
) {
    for (name, field) in meta.fields.iter() {
        if field.field_id != 0 {
            if let Ok(id) = FieldId::new(field.field_id) {
                out.insert(name.clone(), id);
            }
        }
        collect_fields(out, field);
    }
}

pub fn stateless_for_namespace(
    store: &dyn skippr_wal_catalog::SkipprStoreRead,
    workspace: &str,
    pipeline: &str,
    namespace: &str,
) -> Result<Option<StatelessSelect>, ComputeApplyError> {
    let key = journal::journal_key(workspace, pipeline)?;
    let log = journal::load(store, &key)?;
    Ok(log.kinds().into_iter().find_map(|kind| match kind {
        ComputeKind::Stateless {
            target: super::Relation::PipelineTable { namespace: ns, .. },
            select,
            ..
        } if ns.as_str() == namespace => Some(select),
        _ => None,
    }))
}

fn event_time_col(
    config: &Config,
    ingest: &std::collections::BTreeMap<
        (String, String),
        std::collections::BTreeMap<String, FieldId>,
    >,
    pipeline: &str,
) -> Option<super::EventTimeCol> {
    let raw = config.get_transform_event_time();
    let name = raw.split(',').next()?.trim();
    if name.is_empty() {
        return None;
    }
    ingest
        .values()
        .find_map(|fields| fields.get(name).copied().map(super::EventTimeCol::new))
        .or_else(|| {
            ingest
                .get(&(pipeline.to_string(), pipeline.to_string()))
                .and_then(|fields| fields.get(name).copied().map(super::EventTimeCol::new))
        })
}

pub fn resolve_ddl_pipeline(bound_pipeline: &str, name: &Fqn) -> Result<String, ComputeApplyError> {
    if name.is_bare() {
        if bound_pipeline.is_empty() {
            return Err(ComputeApplyError::Message(
                "compute name must be pipeline.ident when no pipeline is bound".into(),
            ));
        }
        Ok(bound_pipeline.to_string())
    } else if bound_pipeline.is_empty() || name.pipeline.as_str() == bound_pipeline {
        Ok(name.pipeline.as_str().to_string())
    } else {
        Err(ComputeApplyError::Message(
            "compute name must match the bound pipeline".into(),
        ))
    }
}

fn qualify_alter_name(pipeline: &str, name: &Fqn) -> Result<Fqn, ComputeApplyError> {
    if name.is_bare() {
        Ok(Fqn::from_parts(pipeline, name.name.as_str())?)
    } else if name.pipeline.as_str() == pipeline {
        Ok(name.clone())
    } else {
        Err(ComputeApplyError::Message(
            "ALTER COMPUTE name must match the bound pipeline".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_alter_compute_is_drop_create_not_stateless() {
        let src = include_str!("apply.rs");
        let gate = src
            .split("async fn apply_alter_compute")
            .nth(1)
            .unwrap()
            .split("fn apply_alterations")
            .next()
            .unwrap();
        assert!(
            gate.contains("ComputeKind::Py"),
            "ALTER COMPUTE on Py must fail before the Stateless SET FILTER gate"
        );
        assert!(
            !gate.contains("is_table_backed()"),
            "Py must not share the Stateless SET FILTER gate via is_table_backed"
        );
        let catalog = BindCatalog {
            ingest: Default::default(),
            ingest_namespaces: Default::default(),
            existing: Vec::new(),
            event_time: None,
            watermark_delay: None,
        };
        let kind = ComputeKind::Py {
            name: Fqn::from_parts("p", "enrich").unwrap(),
            path: "stamp.py".into(),
            handler: "transform".into(),
            digest: "abc".into(),
        };
        let err = apply_one(
            &catalog,
            kind,
            &ComputeAlteration::SetFilter {
                expr: "true".into(),
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("DROP and CREATE"), "{msg}");
        assert!(!msg.contains("Stateless"), "{msg}");
    }

    #[test]
    fn alter_name_foreign_pipeline_fails() {
        let name = Fqn::from_parts("other", "customer_totals").unwrap();
        let err = qualify_alter_name("invoices", &name).unwrap_err();
        assert!(err.to_string().contains("bound pipeline"), "{err}");
    }

    #[test]
    fn alter_name_bare_ident_binds_pipeline() {
        let name = Fqn::from_parts(Fqn::BARE, "positive_orders").unwrap();
        let bound = qualify_alter_name("invoices", &name).unwrap();
        assert_eq!(bound.to_string(), "invoices.positive_orders");
    }

    #[test]
    fn prefix_deletes_id_has_slash_boundary() {
        let src = include_str!("apply.rs");
        let fn_src = src
            .split("pub fn prefix_deletes")
            .nth(1)
            .unwrap()
            .split("/// Bind config items")
            .next()
            .unwrap();
        assert!(
            fn_src.contains("{kind}/{pipeline}/{id}/"),
            "DROP/ALTER must not prefix-delete a sibling id"
        );
        assert!(
            fn_src.contains("meta/{pipeline}/{id}"),
            "meta is an exact key, not a prefix"
        );
    }

    #[test]
    fn resolve_ddl_pipeline_bare_requires_bound() {
        let name = Fqn::from_parts(Fqn::BARE, "invoices").unwrap();
        let err = resolve_ddl_pipeline("", &name).unwrap_err();
        assert!(err.to_string().contains("pipeline.ident"), "{err}");
        let bound = resolve_ddl_pipeline("invoices", &name).unwrap();
        assert_eq!(bound, "invoices");
    }

    #[test]
    fn alter_name_same_as_pipeline_stays_qualified() {
        let name = Fqn::from_parts("invoices", "invoices").unwrap();
        let bound = qualify_alter_name("invoices", &name).unwrap();
        assert_eq!(bound, name);
        let err = qualify_alter_name("billing", &name).unwrap_err();
        assert!(err.to_string().contains("bound pipeline"), "{err}");
    }

    #[test]
    fn alter_name_matching_pipeline_keeps_fqn() {
        let name = Fqn::from_parts("invoices", "customer_totals").unwrap();
        let bound = qualify_alter_name("invoices", &name).unwrap();
        assert_eq!(bound, name);
    }

    #[test]
    fn alter_measure_binds_input_relation_not_first_ingest() {
        use crate::compute::{Measure, MeasureKind, PipelineName, Relation};
        use std::collections::{BTreeMap, BTreeSet};
        use std::time::Duration;
        let mut ingest = BTreeMap::new();
        ingest.insert(
            ("invoices".into(), "aaa".into()),
            BTreeMap::from([("amount".into(), FieldId::new(99).unwrap())]),
        );
        ingest.insert(
            ("invoices".into(), "orders".into()),
            BTreeMap::from([
                ("customer_id".into(), FieldId::new(1).unwrap()),
                ("amount".into(), FieldId::new(2).unwrap()),
            ]),
        );
        let catalog = BindCatalog {
            ingest,
            ingest_namespaces: BTreeSet::from(["aaa".into(), "orders".into()]),
            existing: Vec::new(),
            event_time: None,
            watermark_delay: Some(Duration::from_secs(1)),
        };
        let kind = ComputeKind::KeyedTable {
            name: Fqn::from_parts("invoices", "customer_totals").unwrap(),
            input: Relation::PipelineTable {
                pipeline: PipelineName::new("invoices").unwrap(),
                namespace: Ident::new("orders").unwrap(),
            },
            keys: NonEmpty::try_from_vec(vec![FieldId::new(1).unwrap()]).unwrap(),
            measures: NonEmpty::try_from_vec(vec![Measure {
                name: Ident::new("invoice_total").unwrap(),
                kind: MeasureKind::Sum(FieldId::new(2).unwrap()),
            }])
            .unwrap(),
        };
        let next = apply_alterations(
            &catalog,
            kind,
            &[ComputeAlteration::SetMeasure {
                column: "invoice_total".into(),
                kind: UnboundMeasureKind::Sum("amount".into()),
            }],
        )
        .unwrap();
        match next {
            ComputeKind::KeyedTable { measures, .. } => match measures.as_slice()[0].kind {
                MeasureKind::Sum(id) => assert_eq!(id.get(), 2),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }
}
