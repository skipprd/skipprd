use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use react_core::keyspace::{encode_key_component, Keyspace};
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;

use crate::lineage_types::{
    edge_id, now_epoch_secs, LineageDirection, LineageEdge, LineageEdgeKind, LineageEvidenceSource,
    LineageGraphQuery, LineageGraphSnapshot, LineageNode, LineageNodeId, LineageNodeKind,
    LINEAGE_GRAPH_VERSION,
};

#[derive(Clone)]
pub struct LineageStore {
    storage: Arc<dyn StorageAdapter>,
    keyspace: Arc<dyn Keyspace>,
}

impl LineageStore {
    pub fn new(storage: Arc<dyn StorageAdapter>, keyspace: Arc<dyn Keyspace>) -> Self {
        Self { storage, keyspace }
    }

    pub async fn read_graph(
        &self,
        scope: &RequestScope,
    ) -> Result<Option<LineageGraphSnapshot>, String> {
        let key = self.graph_key(scope);
        match self.storage.get_json(&key).await {
            Ok(value) => serde_json::from_value::<LineageGraphSnapshot>(value)
                .map(Some)
                .map_err(|e| format!("failed to decode lineage graph: {e}")),
            Err(_) => Ok(None),
        }
    }

    pub async fn write_graph(
        &self,
        scope: &RequestScope,
        graph: &LineageGraphSnapshot,
    ) -> Result<(), String> {
        graph.validate()?;
        let key = self.graph_key(scope);
        let value = serde_json::to_value(graph)
            .map_err(|e| format!("failed to encode lineage graph: {e}"))?;
        self.storage
            .put_json(&key, &value)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn merge_graph(
        &self,
        scope: &RequestScope,
        next: LineageGraphSnapshot,
    ) -> Result<LineageGraphSnapshot, String> {
        let current = self.read_graph(scope).await?.unwrap_or_default();
        let merged = merge_graphs(current, next)?;
        self.write_graph(scope, &merged).await?;
        Ok(merged)
    }

    pub fn graph_key(&self, scope: &RequestScope) -> String {
        self.keyspace.scoped_key(scope, &["lineage", "graph.yaml"])
    }

    #[allow(dead_code)]
    pub fn asset_key(&self, scope: &RequestScope, asset_id: &str) -> String {
        self.keyspace.scoped_key(
            scope,
            &[
                "lineage",
                "assets",
                &format!("{}.yaml", encode_key_component(asset_id)),
            ],
        )
    }
}

pub fn merge_graphs(
    mut current: LineageGraphSnapshot,
    mut next: LineageGraphSnapshot,
) -> Result<LineageGraphSnapshot, String> {
    current.validate()?;
    next.validate()?;

    let mut nodes: BTreeMap<LineageNodeId, LineageNode> = current
        .nodes
        .into_iter()
        .map(|node| (node.id.clone(), node))
        .collect();
    for node in next.nodes.drain(..) {
        nodes.insert(node.id.clone(), node);
    }

    let mut edges = current
        .edges
        .into_iter()
        .map(|edge| (edge.id.clone(), edge))
        .collect::<BTreeMap<_, _>>();
    for edge in next.edges.drain(..) {
        edges.insert(edge.id.clone(), edge);
    }

    let next_replaces_query_history = next
        .nodes
        .iter()
        .any(|node| node.kind == LineageNodeKind::Query)
        || next
            .edges
            .iter()
            .any(|edge| edge.provenance.source == LineageEvidenceSource::WarehouseQueryHistory)
        || next.diagnostics.iter().any(|diagnostic| {
            diagnostic.source == Some(LineageEvidenceSource::WarehouseQueryHistory)
        });
    if next_replaces_query_history {
        current.diagnostics.retain(|diagnostic| {
            diagnostic.source != Some(LineageEvidenceSource::WarehouseQueryHistory)
        });
    }
    current.diagnostics.extend(next.diagnostics);
    current.version = LINEAGE_GRAPH_VERSION;
    current.built_at_epoch_secs = now_epoch_secs();
    current.nodes = nodes.into_values().collect();
    current.edges = edges.into_values().collect();
    current.validate()?;
    Ok(current)
}

pub fn slice_graph(
    graph: &LineageGraphSnapshot,
    query: &LineageGraphQuery,
) -> LineageGraphSnapshot {
    if query
        .field
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
    {
        return field_projection_graph(graph, query);
    }

    let Some(seed) = seed_node_id(graph, query) else {
        return entity_projection_graph(graph);
    };

    let include_upstream = matches!(
        query.direction,
        LineageDirection::Upstream | LineageDirection::Both
    );
    let include_downstream = matches!(
        query.direction,
        LineageDirection::Downstream | LineageDirection::Both
    );
    let mut selected = BTreeSet::from([seed.clone()]);
    let mut queue = VecDeque::from([seed]);

    while let Some(node_id) = queue.pop_front() {
        for edge in &graph.edges {
            if include_upstream
                && edge.to_node_id == node_id
                && selected.insert(edge.from_node_id.clone())
            {
                queue.push_back(edge.from_node_id.clone());
            }
            if include_downstream
                && edge.from_node_id == node_id
                && selected.insert(edge.to_node_id.clone())
            {
                queue.push_back(edge.to_node_id.clone());
            }
        }
    }

    let nodes = graph
        .nodes
        .iter()
        .filter(|node| selected.contains(&node.id))
        .cloned()
        .collect::<Vec<_>>();
    let edges = graph
        .edges
        .iter()
        .filter(|edge| selected.contains(&edge.from_node_id) && selected.contains(&edge.to_node_id))
        .cloned()
        .collect::<Vec<_>>();

    entity_projection_graph(&LineageGraphSnapshot {
        version: graph.version,
        built_at_epoch_secs: graph.built_at_epoch_secs,
        nodes,
        edges,
        diagnostics: graph.diagnostics.clone(),
    })
}

fn entity_projection_graph(graph: &LineageGraphSnapshot) -> LineageGraphSnapshot {
    let mut entity_ids = display_entity_ids(graph);
    let mut projected_edges = projected_entity_edges(graph, &entity_ids);
    remove_redundant_direct_edges(&mut projected_edges);
    remove_isolated_query_nodes(graph, &mut entity_ids, &mut projected_edges);
    let ranks = topology_ranks(&projected_edges, &entity_ids);
    let nodes = graph
        .nodes
        .iter()
        .filter(|node| entity_ids.contains(&node.id))
        .map(|node| annotate_node_rank(node.clone(), &ranks, "normal"))
        .collect::<Vec<_>>();
    let edges = projected_edges
        .into_iter()
        .map(|edge| annotate_edge_state(edge, "normal"))
        .collect::<Vec<_>>();
    LineageGraphSnapshot {
        version: graph.version,
        built_at_epoch_secs: graph.built_at_epoch_secs,
        nodes,
        edges,
        diagnostics: graph.diagnostics.clone(),
    }
}

fn field_projection_graph(
    graph: &LineageGraphSnapshot,
    query: &LineageGraphQuery,
) -> LineageGraphSnapshot {
    let field_ids = matching_field_node_ids(graph, query);
    if field_ids.is_empty() {
        return entity_projection_graph(graph);
    }

    let lineage_field_ids = connected_field_node_ids(graph, &field_ids, &query.direction);
    let mut entity_ids = display_entity_ids(graph);
    let highlighted_entity_ids = containing_entity_ids(graph, &lineage_field_ids);
    let mut projected_edges = projected_entity_edges(graph, &entity_ids);
    remove_redundant_direct_edges(&mut projected_edges);
    remove_isolated_query_nodes(graph, &mut entity_ids, &mut projected_edges);
    let mut ranks = topology_ranks(&projected_edges, &entity_ids);
    add_field_ranks(graph, &lineage_field_ids, &mut ranks);

    let mut selected_ids = entity_ids.clone();
    selected_ids.extend(lineage_field_ids.iter().cloned());

    let nodes = graph
        .nodes
        .iter()
        .filter(|node| selected_ids.contains(&node.id))
        .map(|node| {
            let state = if node.kind == LineageNodeKind::Field {
                "highlighted"
            } else if highlighted_entity_ids.contains(&node.id) {
                "highlighted"
            } else {
                "faded"
            };
            annotate_node_rank(node.clone(), &ranks, state)
        })
        .collect::<Vec<_>>();

    let mut edges = graph
        .edges
        .iter()
        .filter(|edge| {
            selected_ids.contains(&edge.from_node_id) && selected_ids.contains(&edge.to_node_id)
        })
        .map(|edge| {
            let highlighted = lineage_field_ids.contains(&edge.from_node_id)
                || lineage_field_ids.contains(&edge.to_node_id)
                || (highlighted_entity_ids.contains(&edge.from_node_id)
                    && highlighted_entity_ids.contains(&edge.to_node_id));
            annotate_edge_state(
                edge.clone(),
                if highlighted { "highlighted" } else { "faded" },
            )
        })
        .collect::<Vec<_>>();
    edges.extend(projected_edges.into_iter().filter_map(|edge| {
        if !selected_ids.contains(&edge.from_node_id) || !selected_ids.contains(&edge.to_node_id) {
            return None;
        }
        let state = if highlighted_entity_ids.contains(&edge.from_node_id)
            && highlighted_entity_ids.contains(&edge.to_node_id)
        {
            "highlighted"
        } else {
            "faded"
        };
        Some(annotate_edge_state(edge, state))
    }));
    let edges = edges
        .into_iter()
        .map(|edge| (edge.id.clone(), edge))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect::<Vec<_>>();

    LineageGraphSnapshot {
        version: graph.version,
        built_at_epoch_secs: graph.built_at_epoch_secs,
        nodes,
        edges,
        diagnostics: graph.diagnostics.clone(),
    }
}

fn matching_field_node_ids(
    graph: &LineageGraphSnapshot,
    query: &LineageGraphQuery,
) -> BTreeSet<LineageNodeId> {
    let asset = query
        .asset
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let field = query
        .field
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Field)
        .filter_map(|node| {
            let node_field = node.field.as_ref()?;
            if asset.is_some_and(|asset| node_field.dataset_id != asset) {
                return None;
            }
            if field.is_some_and(|field| node_field.field_path != field) {
                return None;
            }
            Some(node.id.clone())
        })
        .collect()
}

fn connected_field_node_ids(
    graph: &LineageGraphSnapshot,
    seeds: &BTreeSet<LineageNodeId>,
    direction: &LineageDirection,
) -> BTreeSet<LineageNodeId> {
    let include_upstream = matches!(
        direction,
        LineageDirection::Upstream | LineageDirection::Both
    );
    let include_downstream = matches!(
        direction,
        LineageDirection::Downstream | LineageDirection::Both
    );
    let field_ids = graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Field)
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let mut selected = seeds.clone();
    let mut queue = VecDeque::from_iter(seeds.iter().cloned());
    while let Some(node_id) = queue.pop_front() {
        for edge in &graph.edges {
            if include_upstream
                && edge.to_node_id == node_id
                && field_ids.contains(&edge.from_node_id)
                && selected.insert(edge.from_node_id.clone())
            {
                queue.push_back(edge.from_node_id.clone());
            }
            if include_downstream
                && edge.from_node_id == node_id
                && field_ids.contains(&edge.to_node_id)
                && selected.insert(edge.to_node_id.clone())
            {
                queue.push_back(edge.to_node_id.clone());
            }
        }
    }
    selected
}

fn containing_entity_ids(
    graph: &LineageGraphSnapshot,
    field_ids: &BTreeSet<LineageNodeId>,
) -> BTreeSet<LineageNodeId> {
    let mut out = BTreeSet::new();
    let field_datasets = graph
        .nodes
        .iter()
        .filter(|node| field_ids.contains(&node.id))
        .filter_map(|node| node.field.as_ref().map(|field| field.dataset_id.clone()))
        .collect::<BTreeSet<_>>();

    for node in &graph.nodes {
        if node.kind != LineageNodeKind::Field
            && node
                .dataset_id
                .as_ref()
                .is_some_and(|dataset_id| field_datasets.contains(dataset_id))
        {
            out.insert(node.id.clone());
        }
    }
    for edge in &graph.edges {
        if edge.kind == LineageEdgeKind::ContainsField && field_ids.contains(&edge.to_node_id) {
            out.insert(edge.from_node_id.clone());
        }
    }
    out
}

fn display_entity_ids(graph: &LineageGraphSnapshot) -> BTreeSet<LineageNodeId> {
    let internal_alias_ids = display_endpoint_map(graph)
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    graph
        .nodes
        .iter()
        .filter(|node| node.kind != LineageNodeKind::Field)
        .filter(|node| !internal_alias_ids.contains(&node.id))
        .map(|node| node.id.clone())
        .collect()
}

fn remove_isolated_query_nodes(
    graph: &LineageGraphSnapshot,
    entity_ids: &mut BTreeSet<LineageNodeId>,
    edges: &mut Vec<LineageEdge>,
) {
    let connected = edges
        .iter()
        .flat_map(|edge| [edge.from_node_id.clone(), edge.to_node_id.clone()])
        .collect::<BTreeSet<_>>();
    let isolated_queries = graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Query)
        .filter(|node| entity_ids.contains(&node.id))
        .filter(|node| !connected.contains(&node.id))
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    if isolated_queries.is_empty() {
        return;
    }
    entity_ids.retain(|node_id| !isolated_queries.contains(node_id));
    edges.retain(|edge| {
        !isolated_queries.contains(&edge.from_node_id)
            && !isolated_queries.contains(&edge.to_node_id)
    });
}

fn remove_redundant_direct_edges(edges: &mut Vec<LineageEdge>) {
    let all_edges = edges.clone();
    edges.retain(|edge| !has_alternate_node_route(&all_edges, edge));
}

fn has_alternate_node_route(edges: &[LineageEdge], candidate: &LineageEdge) -> bool {
    let mut outgoing: BTreeMap<LineageNodeId, Vec<LineageNodeId>> = BTreeMap::new();
    for edge in edges {
        if edge.id == candidate.id || edge.kind == LineageEdgeKind::ContainsField {
            continue;
        }
        outgoing
            .entry(edge.from_node_id.clone())
            .or_default()
            .push(edge.to_node_id.clone());
    }

    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::from([(candidate.from_node_id.clone(), 0usize)]);
    while let Some((node_id, depth)) = queue.pop_front() {
        if !visited.insert(node_id.clone()) {
            continue;
        }
        for next in outgoing.get(&node_id).into_iter().flatten() {
            let next_depth = depth + 1;
            if next == &candidate.to_node_id && next_depth >= 2 {
                return true;
            }
            if next != &candidate.to_node_id {
                queue.push_back((next.clone(), next_depth));
            }
        }
    }
    false
}

fn add_field_ranks(
    graph: &LineageGraphSnapshot,
    field_ids: &BTreeSet<LineageNodeId>,
    ranks: &mut BTreeMap<LineageNodeId, usize>,
) {
    let entity_by_dataset = graph
        .nodes
        .iter()
        .filter(|node| node.kind != LineageNodeKind::Field)
        .filter_map(|node| {
            node.dataset_id
                .as_ref()
                .map(|dataset| (dataset.clone(), node.id.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    for node in &graph.nodes {
        if !field_ids.contains(&node.id) {
            continue;
        }
        let containing_rank = node
            .field
            .as_ref()
            .and_then(|field| entity_by_dataset.get(&field.dataset_id))
            .and_then(|entity_id| ranks.get(entity_id))
            .copied()
            .or_else(|| {
                graph.edges.iter().find_map(|edge| {
                    (edge.kind == LineageEdgeKind::ContainsField && edge.to_node_id == node.id)
                        .then(|| ranks.get(&edge.from_node_id).copied())
                        .flatten()
                })
            })
            .unwrap_or(0);
        ranks.insert(node.id.clone(), containing_rank);
    }
}

fn topology_ranks(
    edges: &[LineageEdge],
    node_ids: &BTreeSet<LineageNodeId>,
) -> BTreeMap<LineageNodeId, usize> {
    let mut indegree = node_ids
        .iter()
        .map(|id| (id.clone(), 0usize))
        .collect::<BTreeMap<_, _>>();
    let mut outgoing: BTreeMap<LineageNodeId, Vec<LineageNodeId>> = BTreeMap::new();
    for edge in edges {
        if !node_ids.contains(&edge.from_node_id)
            || !node_ids.contains(&edge.to_node_id)
            || edge.kind == LineageEdgeKind::ContainsField
        {
            continue;
        }
        outgoing
            .entry(edge.from_node_id.clone())
            .or_default()
            .push(edge.to_node_id.clone());
        *indegree.entry(edge.to_node_id.clone()).or_default() += 1;
    }

    let mut ranks = node_ids
        .iter()
        .map(|id| (id.clone(), 0usize))
        .collect::<BTreeMap<_, _>>();
    let mut queue = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(id.clone()))
        .collect::<VecDeque<_>>();
    while let Some(id) = queue.pop_front() {
        let rank = ranks.get(&id).copied().unwrap_or(0);
        for target in outgoing.get(&id).into_iter().flatten() {
            ranks
                .entry(target.clone())
                .and_modify(|current| *current = (*current).max(rank + 1))
                .or_insert(rank + 1);
            if let Some(degree) = indegree.get_mut(target) {
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    queue.push_back(target.clone());
                }
            }
        }
    }
    ranks
}

fn projected_entity_edges(
    graph: &LineageGraphSnapshot,
    entity_ids: &BTreeSet<LineageNodeId>,
) -> Vec<LineageEdge> {
    let display_endpoints = display_endpoint_map(graph);
    let field_to_entity = field_to_entity_map(graph);
    let mut out = BTreeMap::new();
    for edge in &graph.edges {
        if edge.kind == LineageEdgeKind::ContainsField {
            continue;
        }

        if let (Some(from), Some(to)) = (
            resolve_display_endpoint(&edge.from_node_id, &display_endpoints, entity_ids),
            resolve_display_endpoint(&edge.to_node_id, &display_endpoints, entity_ids),
        ) {
            if from != to {
                let mut projected = edge.clone();
                projected.from_node_id = from;
                projected.to_node_id = to;
                projected.id = edge_id(
                    projected.kind.clone(),
                    &projected.from_node_id,
                    &projected.to_node_id,
                );
                if projected.from_node_id != edge.from_node_id
                    || projected.to_node_id != edge.to_node_id
                {
                    projected
                        .metadata
                        .insert("_lineage_projected".to_string(), "true".to_string());
                }
                out.insert(projected.id.clone(), projected);
            }
            continue;
        }

        if !matches!(
            edge.kind,
            LineageEdgeKind::FieldDerivesFrom | LineageEdgeKind::AggregatesFrom
        ) {
            continue;
        }
        let Some(from_entity) = field_to_entity.get(&edge.from_node_id) else {
            continue;
        };
        let Some(to_entity) = field_to_entity.get(&edge.to_node_id) else {
            continue;
        };
        if from_entity == to_entity {
            continue;
        }
        let mut projected = edge.clone();
        projected.from_node_id = from_entity.clone();
        projected.to_node_id = to_entity.clone();
        projected.id = edge_id(
            projected.kind.clone(),
            &projected.from_node_id,
            &projected.to_node_id,
        );
        projected
            .metadata
            .insert("_lineage_projected".to_string(), "true".to_string());
        out.insert(projected.id.clone(), projected);
    }
    out.into_values().collect()
}

fn display_endpoint_map(graph: &LineageGraphSnapshot) -> BTreeMap<LineageNodeId, LineageNodeId> {
    let mut out = materialized_endpoint_map(graph);
    out.extend(dbt_source_endpoint_map(graph));
    out
}

fn materialized_endpoint_map(
    graph: &LineageGraphSnapshot,
) -> BTreeMap<LineageNodeId, LineageNodeId> {
    let node_kinds = graph
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node.kind.clone()))
        .collect::<BTreeMap<_, _>>();
    graph
        .edges
        .iter()
        .filter(|edge| edge.kind == LineageEdgeKind::Materializes)
        .filter(|edge| node_kinds.get(&edge.from_node_id) != Some(&LineageNodeKind::Query))
        .map(|edge| (edge.from_node_id.clone(), edge.to_node_id.clone()))
        .collect()
}

fn dbt_source_endpoint_map(graph: &LineageGraphSnapshot) -> BTreeMap<LineageNodeId, LineageNodeId> {
    let node_kinds = graph
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node.kind.clone()))
        .collect::<BTreeMap<_, _>>();
    graph
        .edges
        .iter()
        .filter(|edge| edge.kind == LineageEdgeKind::SelectsFrom)
        .filter(|edge| {
            node_kinds.get(&edge.from_node_id) == Some(&LineageNodeKind::WarehouseTable)
                && node_kinds.get(&edge.to_node_id) == Some(&LineageNodeKind::DbtSource)
        })
        .map(|edge| (edge.to_node_id.clone(), edge.from_node_id.clone()))
        .collect()
}

fn resolve_display_endpoint(
    node_id: &LineageNodeId,
    materialized_endpoints: &BTreeMap<LineageNodeId, LineageNodeId>,
    entity_ids: &BTreeSet<LineageNodeId>,
) -> Option<LineageNodeId> {
    let resolved = materialized_endpoints
        .get(node_id)
        .cloned()
        .unwrap_or_else(|| node_id.clone());
    entity_ids.contains(&resolved).then_some(resolved)
}

fn field_to_entity_map(graph: &LineageGraphSnapshot) -> BTreeMap<LineageNodeId, LineageNodeId> {
    let display_ids = display_entity_ids(graph);
    let entity_by_dataset = graph
        .nodes
        .iter()
        .filter(|node| node.kind != LineageNodeKind::Field)
        .filter(|node| display_ids.contains(&node.id))
        .filter_map(|node| {
            node.dataset_id
                .as_ref()
                .map(|dataset| (dataset.clone(), node.id.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut out = BTreeMap::new();
    for node in &graph.nodes {
        if node.kind != LineageNodeKind::Field {
            continue;
        }
        if let Some(entity_id) = node
            .field
            .as_ref()
            .and_then(|field| entity_by_dataset.get(&field.dataset_id))
        {
            out.insert(node.id.clone(), entity_id.clone());
        }
    }
    for edge in &graph.edges {
        if edge.kind == LineageEdgeKind::ContainsField {
            out.entry(edge.to_node_id.clone())
                .or_insert_with(|| edge.from_node_id.clone());
        }
    }
    out
}

fn annotate_node_rank(
    mut node: LineageNode,
    ranks: &BTreeMap<LineageNodeId, usize>,
    state: &str,
) -> LineageNode {
    if let Some(rank) = ranks.get(&node.id) {
        node.metadata
            .insert("_lineage_rank".to_string(), rank.to_string());
    }
    node.metadata
        .insert("_lineage_state".to_string(), state.to_string());
    node
}

fn annotate_edge_state(mut edge: LineageEdge, state: &str) -> LineageEdge {
    edge.metadata
        .insert("_lineage_state".to_string(), state.to_string());
    edge
}

fn seed_node_id(graph: &LineageGraphSnapshot, query: &LineageGraphQuery) -> Option<LineageNodeId> {
    let asset = query
        .asset
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let field = query
        .field
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (asset, field) {
        (None, None) => None,
        (Some(asset), Some(field)) => graph.nodes.iter().find_map(|node| {
            let node_field = node.field.as_ref()?;
            if node_field.dataset_id == asset && node_field.field_path == field {
                Some(node.id.clone())
            } else {
                None
            }
        }),
        (Some(asset), None) => graph.nodes.iter().find_map(|node| {
            if node.id.as_str() == asset || node.dataset_id.as_deref() == Some(asset) {
                Some(node.id.clone())
            } else {
                None
            }
        }),
        (None, Some(field)) => graph.nodes.iter().find_map(|node| {
            let node_field = node.field.as_ref()?;
            if node_field.field_path == field {
                Some(node.id.clone())
            } else {
                None
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::lineage_types::{
        edge_id, LineageEdge, LineageEdgeKind, LineageEvidenceSource, LineageFieldRef,
        LineageNodeKind, LineageProvenance,
    };

    use super::*;

    fn node(id: &str) -> LineageNode {
        LineageNode {
            id: LineageNodeId::generated(id),
            label: id.to_string(),
            kind: LineageNodeKind::WarehouseTable,
            dataset_id: Some(id.to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        }
    }

    fn field_node(dataset_id: &str, field_path: &str) -> LineageNode {
        LineageNode {
            id: LineageNodeId::generated(format!("field:{dataset_id}#{field_path}")),
            label: field_path.to_string(),
            kind: LineageNodeKind::Field,
            dataset_id: Some(dataset_id.to_string()),
            field: Some(LineageFieldRef {
                dataset_id: dataset_id.to_string(),
                field_path: field_path.to_string(),
                field_id: None,
            }),
            path: None,
            metadata: BTreeMap::new(),
        }
    }

    fn edge(kind: LineageEdgeKind, from: &LineageNodeId, to: &LineageNodeId) -> LineageEdge {
        LineageEdge {
            id: edge_id(kind.clone(), from, to),
            from_node_id: from.clone(),
            to_node_id: to.clone(),
            kind,
            provenance: LineageProvenance::observed(LineageEvidenceSource::Catalog, None),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn merge_graphs_replaces_matching_nodes_and_edges() {
        let a = node("a");
        let b = node("b");
        let edge = edge(LineageEdgeKind::SelectsFrom, &a.id, &b.id);
        let current = LineageGraphSnapshot {
            nodes: vec![a.clone()],
            ..Default::default()
        };
        let next = LineageGraphSnapshot {
            nodes: vec![a, b],
            edges: vec![edge],
            ..Default::default()
        };
        let merged = merge_graphs(current, next).unwrap();
        assert_eq!(merged.nodes.len(), 2);
        assert_eq!(merged.edges.len(), 1);
    }

    #[test]
    fn default_slice_hides_fields_and_adds_topology_ranks() {
        let a = node("raw.orders");
        let b = node("analytics.orders");
        let field = field_node("raw.orders", "order_id");
        let graph = LineageGraphSnapshot {
            nodes: vec![a.clone(), b.clone(), field.clone()],
            edges: vec![
                edge(LineageEdgeKind::ContainsField, &a.id, &field.id),
                edge(LineageEdgeKind::SelectsFrom, &a.id, &b.id),
            ],
            ..Default::default()
        };

        let got = slice_graph(
            &graph,
            &LineageGraphQuery {
                asset: None,
                field: None,
                direction: LineageDirection::Both,
            },
        );
        assert!(got
            .nodes
            .iter()
            .all(|node| node.kind != LineageNodeKind::Field));
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == a.id)
                .and_then(|node| node.metadata.get("_lineage_rank"))
                .map(String::as_str),
            Some("0")
        );
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == b.id)
                .and_then(|node| node.metadata.get("_lineage_rank"))
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn field_slice_highlights_related_entities_and_fades_unrelated_entities() {
        let a = node("raw.orders");
        let b = node("analytics.orders");
        let c = node("analytics.customers");
        let a_field = field_node("raw.orders", "order_id");
        let b_field = field_node("analytics.orders", "order_id");
        let graph = LineageGraphSnapshot {
            nodes: vec![
                a.clone(),
                b.clone(),
                c.clone(),
                a_field.clone(),
                b_field.clone(),
            ],
            edges: vec![
                edge(LineageEdgeKind::ContainsField, &a.id, &a_field.id),
                edge(LineageEdgeKind::ContainsField, &b.id, &b_field.id),
                edge(LineageEdgeKind::FieldDerivesFrom, &a_field.id, &b_field.id),
            ],
            ..Default::default()
        };

        let got = slice_graph(
            &graph,
            &LineageGraphQuery {
                asset: Some("analytics.orders".to_string()),
                field: Some("order_id".to_string()),
                direction: LineageDirection::Both,
            },
        );
        assert!(got
            .nodes
            .iter()
            .any(|node| node.kind == LineageNodeKind::Field));
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == a.id)
                .and_then(|node| node.metadata.get("_lineage_state"))
                .map(String::as_str),
            Some("highlighted")
        );
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == c.id)
                .and_then(|node| node.metadata.get("_lineage_state"))
                .map(String::as_str),
            Some("faded")
        );
    }

    #[test]
    fn entity_projection_resolves_materialized_model_duplicates() {
        let source = node("raw.orders");
        let model = LineageNode {
            id: LineageNodeId::generated("dbt_model:model.project.orders"),
            label: "orders_model".to_string(),
            kind: LineageNodeKind::DbtModel,
            dataset_id: Some("analytics.orders".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        };
        let table = node("analytics.orders");
        let query = LineageNode {
            id: LineageNodeId::generated("query:q1"),
            label: "q1".to_string(),
            kind: LineageNodeKind::Query,
            dataset_id: None,
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        };
        let graph = LineageGraphSnapshot {
            nodes: vec![source.clone(), model.clone(), table.clone(), query.clone()],
            edges: vec![
                edge(LineageEdgeKind::SelectsFrom, &source.id, &model.id),
                edge(LineageEdgeKind::Materializes, &model.id, &table.id),
                edge(LineageEdgeKind::SelectsFrom, &table.id, &query.id),
            ],
            ..Default::default()
        };

        let got = slice_graph(
            &graph,
            &LineageGraphQuery {
                asset: None,
                field: None,
                direction: LineageDirection::Both,
            },
        );
        assert!(got.nodes.iter().all(|node| node.id != model.id));
        assert!(got.edges.iter().any(|edge| {
            edge.from_node_id == source.id
                && edge.to_node_id == table.id
                && edge.metadata.get("_lineage_projected").map(String::as_str) == Some("true")
        }));
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == query.id)
                .and_then(|node| node.metadata.get("_lineage_rank"))
                .map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn entity_projection_resolves_dbt_source_aliases_to_warehouse_tables() {
        let table = node("analytics.raw.bike_hire");
        let dbt_source = LineageNode {
            id: LineageNodeId::generated("dbt_source:source.project.raw_bike_hire"),
            label: "analytics.raw.bike_hire".to_string(),
            kind: LineageNodeKind::DbtSource,
            dataset_id: Some("analytics.raw.bike_hire".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        };
        let model = LineageNode {
            id: LineageNodeId::generated("dbt_model:model.project.stg_bike_hire"),
            label: "stg_bike_hire".to_string(),
            kind: LineageNodeKind::DbtModel,
            dataset_id: Some("analytics.silver.stg_bike_hire".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        };
        let graph = LineageGraphSnapshot {
            nodes: vec![table.clone(), dbt_source.clone(), model.clone()],
            edges: vec![
                edge(LineageEdgeKind::SelectsFrom, &table.id, &dbt_source.id),
                edge(LineageEdgeKind::SelectsFrom, &dbt_source.id, &model.id),
            ],
            ..Default::default()
        };

        let got = slice_graph(
            &graph,
            &LineageGraphQuery {
                asset: None,
                field: None,
                direction: LineageDirection::Both,
            },
        );

        assert!(got.nodes.iter().all(|node| node.id != dbt_source.id));
        assert!(got
            .edges
            .iter()
            .any(|edge| edge.from_node_id == table.id && edge.to_node_id == model.id));
    }

    #[test]
    fn entity_projection_keeps_materializing_query_as_dag_step() {
        let raw = node("analytics.raw.bike_hire");
        let staging = node("bike_hire_silver.bike_hire.stg_raw_bike_hire");
        let raw_field = field_node("analytics.raw.bike_hire", "ride_id");
        let staging_field = field_node("bike_hire_silver.bike_hire.stg_raw_bike_hire", "ride_id");
        let query = LineageNode {
            id: LineageNodeId::generated("query:create_staging"),
            label: "create_staging".to_string(),
            kind: LineageNodeKind::Query,
            dataset_id: None,
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        };
        let graph = LineageGraphSnapshot {
            nodes: vec![
                raw.clone(),
                query.clone(),
                staging.clone(),
                raw_field.clone(),
                staging_field.clone(),
            ],
            edges: vec![
                edge(LineageEdgeKind::ContainsField, &raw.id, &raw_field.id),
                edge(
                    LineageEdgeKind::ContainsField,
                    &staging.id,
                    &staging_field.id,
                ),
                edge(
                    LineageEdgeKind::FieldDerivesFrom,
                    &raw_field.id,
                    &staging_field.id,
                ),
                edge(LineageEdgeKind::SelectsFrom, &raw.id, &query.id),
                edge(LineageEdgeKind::Materializes, &query.id, &staging.id),
            ],
            ..Default::default()
        };

        let got = slice_graph(
            &graph,
            &LineageGraphQuery {
                asset: None,
                field: None,
                direction: LineageDirection::Both,
            },
        );

        assert!(got.nodes.iter().any(|node| node.id == query.id));
        assert!(got
            .edges
            .iter()
            .any(|edge| edge.from_node_id == raw.id && edge.to_node_id == query.id));
        assert!(got
            .edges
            .iter()
            .any(|edge| edge.from_node_id == query.id && edge.to_node_id == staging.id));
        assert!(!got
            .edges
            .iter()
            .any(|edge| edge.from_node_id == raw.id && edge.to_node_id == staging.id));
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == staging.id)
                .and_then(|node| node.metadata.get("_lineage_rank"))
                .map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn entity_projection_hides_isolated_query_history_nodes() {
        let table = node("analytics.orders");
        let linked_query = LineageNode {
            id: LineageNodeId::generated("query:linked"),
            label: "linked".to_string(),
            kind: LineageNodeKind::Query,
            dataset_id: None,
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        };
        let isolated_query = LineageNode {
            id: LineageNodeId::generated("query:isolated"),
            label: "isolated".to_string(),
            kind: LineageNodeKind::Query,
            dataset_id: None,
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        };
        let graph = LineageGraphSnapshot {
            nodes: vec![table.clone(), linked_query.clone(), isolated_query.clone()],
            edges: vec![edge(
                LineageEdgeKind::SelectsFrom,
                &table.id,
                &linked_query.id,
            )],
            ..Default::default()
        };

        let got = slice_graph(
            &graph,
            &LineageGraphQuery {
                asset: None,
                field: None,
                direction: LineageDirection::Both,
            },
        );
        assert!(got.nodes.iter().any(|node| node.id == linked_query.id));
        assert!(got.nodes.iter().all(|node| node.id != isolated_query.id));
    }
}
