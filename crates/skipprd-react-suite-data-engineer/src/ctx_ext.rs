use std::sync::Arc;

use react_core::agent::AgentCtx;
use react_core::suite::SuiteCtx;

use super::de_config::ProvidersResolved;
use super::providers::{
    CatalogProvider, DatasetCatalogProvider, DbtProvider, QueryProvider, SkipprProvider,
    WarehouseProvider,
};

// TODO(items-83-85): The capability newtypes, cap_accessors macro, wire_sctx_capabilities,
// and copy_capabilities_to_actx are used by sibling suites (e.g. `kb`). Extract them into a
// shared top-level module (e.g. `crate::shared_caps`) so suites don't reach into
// `data_engineer` internals. This also requires relocating the provider traits from
// `data_engineer::providers` and `ProvidersResolved` from `data_engineer::de_config`.
//
// Adding a new provider requires changes in 5 places:
//   1. Cap struct definition below
//   2. cap_accessors! macro invocation (generates sctx_* and actx_* fns)
//   3. wire_sctx_capabilities — set the capability on SuiteCtx
//   4. copy_capabilities_to_actx — copy from SuiteCtx to AgentCtx
//   5. Provider trait + re-export in providers/mod.rs
pub struct WarehouseCap(pub Arc<dyn WarehouseProvider>);
pub struct DbtCap(pub Arc<dyn DbtProvider>);
pub struct QueryCap(pub Arc<dyn QueryProvider>);
pub struct DatasetsCap(pub Arc<dyn DatasetCatalogProvider>);
pub struct CatalogCap(pub Arc<dyn CatalogProvider>);
pub struct SkipprCap(pub Arc<dyn SkipprProvider>);
pub struct ProvidersCfgCap(pub ProvidersResolved);

pub(crate) fn actx_warehouse(ctx: &AgentCtx) -> Option<Arc<dyn WarehouseProvider>> {
    ctx.capability::<WarehouseCap>().map(|c| c.0.clone())
}

pub(crate) fn sctx_dbt(ctx: &SuiteCtx) -> Option<Arc<dyn DbtProvider>> {
    ctx.capability::<DbtCap>().map(|c| c.0.clone())
}

pub(crate) fn actx_dbt(ctx: &AgentCtx) -> Option<Arc<dyn DbtProvider>> {
    ctx.capability::<DbtCap>().map(|c| c.0.clone())
}

pub(crate) fn sctx_query(ctx: &SuiteCtx) -> Option<Arc<dyn QueryProvider>> {
    ctx.capability::<QueryCap>().map(|c| c.0.clone())
}

pub(crate) fn actx_query(ctx: &AgentCtx) -> Option<Arc<dyn QueryProvider>> {
    ctx.capability::<QueryCap>().map(|c| c.0.clone())
}

pub(crate) fn sctx_datasets(ctx: &SuiteCtx) -> Option<Arc<dyn DatasetCatalogProvider>> {
    ctx.capability::<DatasetsCap>().map(|c| c.0.clone())
}

pub(crate) fn sctx_catalog(ctx: &SuiteCtx) -> Option<Arc<dyn CatalogProvider>> {
    ctx.capability::<CatalogCap>().map(|c| c.0.clone())
}

pub(crate) fn actx_catalog(ctx: &AgentCtx) -> Option<Arc<dyn CatalogProvider>> {
    ctx.capability::<CatalogCap>().map(|c| c.0.clone())
}

pub(crate) fn sctx_skippr(ctx: &SuiteCtx) -> Option<Arc<dyn SkipprProvider>> {
    ctx.capability::<SkipprCap>().map(|c| c.0.clone())
}

pub(crate) fn actx_providers_cfg(ctx: &AgentCtx) -> Option<ProvidersResolved> {
    ctx.capability::<ProvidersCfgCap>().map(|c| c.0.clone())
}

/// Wire data_engineer capabilities into a SuiteCtx from a ProvidersResolved.
pub fn wire_sctx_capabilities(
    sctx: &mut SuiteCtx,
    warehouse: Arc<dyn WarehouseProvider>,
    query: Option<Arc<dyn QueryProvider>>,
    datasets: Option<Arc<dyn DatasetCatalogProvider>>,
    catalog: Option<Arc<dyn CatalogProvider>>,
    dbt: Option<Arc<dyn DbtProvider>>,
    skippr: Option<Arc<dyn SkipprProvider>>,
    providers_cfg: ProvidersResolved,
) {
    sctx.set_capability(Arc::new(WarehouseCap(warehouse)));
    if let Some(q) = query {
        sctx.set_capability(Arc::new(QueryCap(q)));
    }
    if let Some(ds) = datasets {
        sctx.set_capability(Arc::new(DatasetsCap(ds)));
    }
    if let Some(cat) = catalog {
        sctx.set_capability(Arc::new(CatalogCap(cat)));
    }
    if let Some(d) = dbt {
        sctx.set_capability(Arc::new(DbtCap(d)));
    }
    if let Some(s) = skippr {
        sctx.set_capability(Arc::new(SkipprCap(s)));
    }
    sctx.set_capability(Arc::new(ProvidersCfgCap(providers_cfg)));
}

/// Copy data_engineer capabilities from SuiteCtx to a new AgentCtx.
///
/// Public so sibling suites (e.g. `kb`) can reuse the same provider wiring
/// without reaching into `data_engineer` internals.
pub fn copy_capabilities_to_actx(sctx: &SuiteCtx, actx: &mut AgentCtx) {
    if let Some(w) = sctx.capability::<WarehouseCap>() {
        actx.set_capability(w);
    }
    if let Some(d) = sctx.capability::<DbtCap>() {
        actx.set_capability(d);
    }
    if let Some(q) = sctx.capability::<QueryCap>() {
        actx.set_capability(q);
    }
    if let Some(ds) = sctx.capability::<DatasetsCap>() {
        actx.set_capability(ds);
    }
    if let Some(cat) = sctx.capability::<CatalogCap>() {
        actx.set_capability(cat);
    }
    if let Some(s) = sctx.capability::<SkipprCap>() {
        actx.set_capability(s);
    }
    if let Some(cfg) = sctx.capability::<ProvidersCfgCap>() {
        actx.set_capability(cfg);
    }
}
