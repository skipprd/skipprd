use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use react_core::keyspace::{encode_key_component, Keyspace};
use react_core::scope::RequestScope;
use react_core::storage::StorageAdapter;

use crate::lineage_types::{
    canonical_dataset_id, canonical_field_path, edge_id, now_epoch_secs, LineageDirection,
    LineageEdge, LineageEdgeKind, LineageEvidenceSource, LineageGraphQuery, LineageGraphSnapshot,
    LineageNode, LineageNodeId, LineageNodeKind, LINEAGE_GRAPH_VERSION,
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

/// Removes warehouse query-history evidence from a persisted graph before re-import.
pub fn strip_query_history_evidence(mut graph: LineageGraphSnapshot) -> LineageGraphSnapshot {
    let query_node_ids = graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Query)
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    graph.nodes.retain(|node| {
        if node.kind == LineageNodeKind::Query {
            return false;
        }
        if node.kind == LineageNodeKind::Field {
            return !node
                .dataset_id
                .as_deref()
                .is_some_and(|dataset_id| dataset_id.starts_with("query:"));
        }
        true
    });
    graph.edges.retain(|edge| {
        edge.provenance.source != Some(LineageEvidenceSource::WarehouseQueryHistory)
            && !query_node_ids.contains(&edge.from_node_id)
            && !query_node_ids.contains(&edge.to_node_id)
    });
    graph.diagnostics.retain(|diagnostic| {
        diagnostic.source != Some(LineageEvidenceSource::WarehouseQueryHistory)
    });
    graph
}

pub fn merge_graphs(
    mut current: LineageGraphSnapshot,
    mut next: LineageGraphSnapshot,
) -> Result<LineageGraphSnapshot, String> {
    current.validate()?;
    next.validate()?;
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

    let mut nodes: BTreeMap<LineageNodeId, LineageNode> = current
        .nodes
        .into_iter()
        .map(|node| (node.id.clone(), node))
        .collect();
    for node in next.nodes.drain(..) {
        nodes
            .entry(node.id.clone())
            .and_modify(|existing| merge_lineage_node(existing, &node))
            .or_insert(node);
    }

    let mut edges = current
        .edges
        .into_iter()
        .map(|edge| (edge.id.clone(), edge))
        .collect::<BTreeMap<_, _>>();
    for edge in next.edges.drain(..) {
        edges.insert(edge.id.clone(), edge);
    }

    if next_replaces_query_history {
        current.diagnostics.retain(|diagnostic| {
            diagnostic.source != Some(LineageEvidenceSource::WarehouseQueryHistory)
        });
    }
    for diagnostic in next.diagnostics {
        if !current.diagnostics.contains(&diagnostic) {
            current.diagnostics.push(diagnostic);
        }
    }
    current.version = LINEAGE_GRAPH_VERSION;
    current.built_at_epoch_secs = now_epoch_secs();
    current.nodes = nodes.into_values().collect();
    current.edges = edges.into_values().collect();
    current.validate()?;
    Ok(current)
}

pub fn merge_lineage_node(existing: &mut LineageNode, incoming: &LineageNode) {
    if existing.label.trim().is_empty() {
        existing.label = incoming.label.clone();
    }
    if existing.dataset_id.is_none() {
        existing.dataset_id = incoming.dataset_id.clone();
    }
    if existing.field.is_none() {
        existing.field = incoming.field.clone();
    }
    if existing.path.is_none() {
        existing.path = incoming.path.clone();
    }
    for (key, value) in &incoming.metadata {
        if value.trim().is_empty() {
            continue;
        }
        if key == "schema_fields" {
            merge_schema_fields_metadata(&mut existing.metadata, value);
            continue;
        }
        existing
            .metadata
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
}

fn merge_schema_fields_metadata(metadata: &mut BTreeMap<String, String>, incoming_json: &str) {
    let incoming_fields = parse_schema_fields_json(incoming_json);
    if incoming_fields.is_empty() {
        return;
    }
    match metadata.get("schema_fields") {
        None => {
            metadata.insert("schema_fields".to_string(), incoming_json.to_string());
        }
        Some(existing_json) => {
            let mut merged = parse_schema_fields_json(existing_json);
            for field in incoming_fields {
                if !merged.iter().any(|existing| existing.name == field.name) {
                    merged.push(field);
                }
            }
            if let Ok(json) = serde_json::to_string(&merged) {
                metadata.insert("schema_fields".to_string(), json);
            }
        }
    }
}

fn parse_schema_fields_json(raw: &str) -> Vec<crate::providers::SkipprFieldSchema> {
    serde_json::from_str(raw).unwrap_or_default()
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
    let mut ranks = topology_ranks(&projected_edges, &entity_ids);
    let field_ids = field_node_ids(graph);
    add_field_ranks(graph, &field_ids, &mut ranks);
    let mut nodes = graph
        .nodes
        .iter()
        .filter(|node| entity_ids.contains(&node.id))
        .map(|node| annotate_node_rank(node.clone(), &ranks, "normal"))
        .collect::<Vec<_>>();
    nodes.extend(schema_field_nodes(graph, &ranks, &BTreeSet::new()));
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
    let field_ids = field_node_ids(graph);
    add_field_ranks(graph, &field_ids, &mut ranks);

    let mut selected_ids = entity_ids.clone();
    selected_ids.extend(lineage_field_ids.iter().cloned());

    let mut nodes = graph
        .nodes
        .iter()
        .filter(|node| selected_ids.contains(&node.id))
        .map(|node| {
            let state = if node.kind == LineageNodeKind::Field
                || highlighted_entity_ids.contains(&node.id)
            {
                "highlighted"
            } else {
                "faded"
            };
            annotate_node_rank(node.clone(), &ranks, state)
        })
        .collect::<Vec<_>>();
    nodes.extend(schema_field_nodes(graph, &ranks, &lineage_field_ids));

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
        .filter(|s| !s.is_empty())
        .map(canonical_dataset_id);
    let field = query
        .field
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(canonical_field_path);
    graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Field)
        .filter_map(|node| {
            let node_field = node.field.as_ref()?;
            if asset
                .as_ref()
                .is_some_and(|asset| canonical_dataset_id(&node_field.dataset_id) != *asset)
            {
                return None;
            }
            if field
                .as_ref()
                .is_some_and(|field| canonical_field_path(&node_field.field_path) != *field)
            {
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
    if include_upstream {
        selected.extend(connected_field_node_ids_one_way(
            graph, seeds, &field_ids, true,
        ));
    }
    if include_downstream {
        selected.extend(connected_field_node_ids_one_way(
            graph, seeds, &field_ids, false,
        ));
    }
    selected
}

fn connected_field_node_ids_one_way(
    graph: &LineageGraphSnapshot,
    seeds: &BTreeSet<LineageNodeId>,
    field_ids: &BTreeSet<LineageNodeId>,
    upstream: bool,
) -> BTreeSet<LineageNodeId> {
    let mut selected = BTreeSet::new();
    let mut queue = VecDeque::from_iter(seeds.iter().cloned());
    while let Some(node_id) = queue.pop_front() {
        for edge in &graph.edges {
            let next = if upstream && edge.to_node_id == node_id {
                &edge.from_node_id
            } else if !upstream && edge.from_node_id == node_id {
                &edge.to_node_id
            } else {
                continue;
            };
            if field_ids.contains(next) && selected.insert(next.clone()) {
                queue.push_back(next.clone());
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

fn field_node_ids(graph: &LineageGraphSnapshot) -> BTreeSet<LineageNodeId> {
    graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Field)
        .map(|node| node.id.clone())
        .collect()
}

fn schema_field_nodes(
    graph: &LineageGraphSnapshot,
    ranks: &BTreeMap<LineageNodeId, usize>,
    highlighted_field_ids: &BTreeSet<LineageNodeId>,
) -> Vec<LineageNode> {
    graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Field)
        .filter(|node| !highlighted_field_ids.contains(&node.id))
        .map(|node| {
            let mut node = annotate_node_rank(node.clone(), ranks, "normal");
            node.metadata
                .insert("_lineage_schema_only".to_string(), "true".to_string());
            node
        })
        .collect()
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
        .filter(|edge| {
            !matches!(
                node_kinds.get(&edge.from_node_id),
                Some(LineageNodeKind::Query | LineageNodeKind::DbtModel)
            )
        })
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
    let mut upstream_field = BTreeMap::<LineageNodeId, Vec<LineageNodeId>>::new();
    for edge in &graph.edges {
        if matches!(
            edge.kind,
            LineageEdgeKind::FieldDerivesFrom | LineageEdgeKind::AggregatesFrom
        ) {
            upstream_field
                .entry(edge.to_node_id.clone())
                .or_default()
                .push(edge.from_node_id.clone());
        }
    }
    let mut out = BTreeMap::new();
    for edge in &graph.edges {
        if edge.kind != LineageEdgeKind::ContainsField {
            continue;
        }
        if display_ids.contains(&edge.from_node_id) {
            out.insert(edge.to_node_id.clone(), edge.from_node_id.clone());
        }
    }
    let mut pending: VecDeque<LineageNodeId> = graph
        .nodes
        .iter()
        .filter(|node| node.kind == LineageNodeKind::Field)
        .filter(|node| !out.contains_key(&node.id))
        .map(|node| node.id.clone())
        .collect();
    while let Some(field_id) = pending.pop_front() {
        if out.contains_key(&field_id) {
            continue;
        }
        let mut resolved = None;
        for parent in upstream_field.get(&field_id).into_iter().flatten() {
            if let Some(entity_id) = out.get(parent) {
                resolved = Some(entity_id.clone());
                break;
            }
            if !pending.iter().any(|pending_id| pending_id == parent) {
                pending.push_back(parent.clone());
            }
        }
        if let Some(entity_id) = resolved {
            out.insert(field_id, entity_id);
            continue;
        }
        let Some(node) = graph.nodes.iter().find(|node| node.id == field_id) else {
            continue;
        };
        if let Some(entity_id) = node
            .field
            .as_ref()
            .and_then(|field| entity_by_dataset.get(&field.dataset_id))
        {
            out.insert(field_id, entity_id.clone());
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
        dataset_node_id, edge_id, LineageDiagnostic, LineageDiagnosticSeverity, LineageEdge,
        LineageEdgeKind, LineageEvidenceSource, LineageFieldRef, LineageNodeKind,
        LineageProvenance,
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

    fn typed_node(id: &str, kind: LineageNodeKind, dataset_id: Option<&str>) -> LineageNode {
        LineageNode {
            id: LineageNodeId::generated(id),
            label: id.to_string(),
            kind,
            dataset_id: dataset_id.map(str::to_string),
            field: None,
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
    fn merge_graphs_preserves_existing_node_metadata_on_collision() {
        let mut current_node = node("analytics.orders");
        current_node
            .metadata
            .insert("provider_brand".to_string(), "snowflake".to_string());
        let mut next_node = node("analytics.orders");
        next_node
            .metadata
            .insert("catalog".to_string(), "analytics".to_string());
        let current = LineageGraphSnapshot {
            nodes: vec![current_node],
            ..Default::default()
        };
        let next = LineageGraphSnapshot {
            nodes: vec![next_node],
            ..Default::default()
        };

        let merged = merge_graphs(current, next).unwrap();
        let node = merged
            .nodes
            .iter()
            .find(|node| node.id == LineageNodeId::generated("analytics.orders"))
            .expect("merged node exists");
        assert_eq!(
            node.metadata.get("provider_brand").map(String::as_str),
            Some("snowflake")
        );
        assert_eq!(
            node.metadata.get("catalog").map(String::as_str),
            Some("analytics")
        );
    }

    #[test]
    fn merge_graphs_deduplicates_matching_diagnostics() {
        let diagnostic = LineageDiagnostic {
            severity: LineageDiagnosticSeverity::Warning,
            message: "warehouse provider unavailable".to_string(),
            source: Some(LineageEvidenceSource::WarehouseQueryHistory),
        };
        let current = LineageGraphSnapshot {
            diagnostics: vec![diagnostic.clone()],
            ..Default::default()
        };
        let next = LineageGraphSnapshot {
            diagnostics: vec![diagnostic.clone(), diagnostic],
            ..Default::default()
        };

        let merged = merge_graphs(current, next).unwrap();
        assert_eq!(merged.diagnostics.len(), 1);
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
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == field.id)
                .and_then(|node| node.metadata.get("_lineage_schema_only"))
                .map(String::as_str),
            Some("true")
        );
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
        let c_field = field_node("analytics.customers", "order_id");
        let graph = LineageGraphSnapshot {
            nodes: vec![
                a.clone(),
                b.clone(),
                c.clone(),
                a_field.clone(),
                b_field.clone(),
                c_field.clone(),
            ],
            edges: vec![
                edge(LineageEdgeKind::ContainsField, &a.id, &a_field.id),
                edge(LineageEdgeKind::ContainsField, &b.id, &b_field.id),
                edge(LineageEdgeKind::ContainsField, &c.id, &c_field.id),
                edge(LineageEdgeKind::FieldDerivesFrom, &a_field.id, &b_field.id),
                edge(LineageEdgeKind::FieldDerivesFrom, &a_field.id, &c_field.id),
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
                .find(|node| node.id == b_field.id)
                .and_then(|node| node.metadata.get("_lineage_schema_only")),
            None
        );
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
        assert_eq!(
            got.edges
                .iter()
                .find(|edge| edge.from_node_id == a.id && edge.to_node_id == c.id)
                .and_then(|edge| edge.metadata.get("_lineage_state"))
                .map(String::as_str),
            Some("faded")
        );
    }

    #[test]
    fn field_slice_includes_downstream_field_usage() {
        let a = node("raw.orders");
        let b = node("analytics.orders");
        let n = node("mart.orders");
        let a_field = field_node("raw.orders", "order_id");
        let b_field = field_node("analytics.orders", "order_id");
        let n_field = field_node("mart.orders", "order_id");
        let graph = LineageGraphSnapshot {
            nodes: vec![
                a.clone(),
                b.clone(),
                n.clone(),
                a_field.clone(),
                b_field.clone(),
                n_field.clone(),
            ],
            edges: vec![
                edge(LineageEdgeKind::ContainsField, &a.id, &a_field.id),
                edge(LineageEdgeKind::ContainsField, &b.id, &b_field.id),
                edge(LineageEdgeKind::ContainsField, &n.id, &n_field.id),
                edge(LineageEdgeKind::FieldDerivesFrom, &a_field.id, &b_field.id),
                edge(LineageEdgeKind::FieldDerivesFrom, &b_field.id, &n_field.id),
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
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == n.id)
                .and_then(|node| node.metadata.get("_lineage_state"))
                .map(String::as_str),
            Some("highlighted")
        );
        assert_eq!(
            got.edges
                .iter()
                .find(|edge| edge.from_node_id == b_field.id && edge.to_node_id == n_field.id)
                .and_then(|edge| edge.metadata.get("_lineage_state"))
                .map(String::as_str),
            Some("highlighted")
        );
    }

    #[test]
    fn field_slice_traverses_source_pipeline_table_model_and_query_fields() {
        let source = typed_node(
            "raw:s3:bucket/prefix",
            LineageNodeKind::RawSource,
            Some("source:s3:bucket/prefix"),
        );
        let pipeline = typed_node(
            "pipeline:bike_hire",
            LineageNodeKind::Pipeline,
            Some("bike_hire"),
        );
        let raw = node("analytics.raw.bike_hire");
        let model = node("bike_hire_gold.bike_hire");
        let query = typed_node("query:q1", LineageNodeKind::Query, Some("query:q1"));
        let source_field = field_node("source:s3:bucket/prefix", "EVENT_DATE");
        let pipeline_field = field_node("bike_hire", "EVENT_DATE");
        let raw_field = field_node("analytics.raw.bike_hire", "EVENT_DATE");
        let model_field = field_node("bike_hire_gold.bike_hire", "EVENT_DATE");
        let query_field = field_node("query:q1", "EVENT_DATE");
        let graph = LineageGraphSnapshot {
            nodes: vec![
                source.clone(),
                pipeline.clone(),
                raw.clone(),
                model.clone(),
                query.clone(),
                source_field.clone(),
                pipeline_field.clone(),
                raw_field.clone(),
                model_field.clone(),
                query_field.clone(),
            ],
            edges: vec![
                edge(LineageEdgeKind::Ingests, &source.id, &pipeline.id),
                edge(LineageEdgeKind::Ingests, &pipeline.id, &raw.id),
                edge(LineageEdgeKind::SelectsFrom, &raw.id, &model.id),
                edge(LineageEdgeKind::SelectsFrom, &model.id, &query.id),
                edge(LineageEdgeKind::ContainsField, &source.id, &source_field.id),
                edge(
                    LineageEdgeKind::ContainsField,
                    &pipeline.id,
                    &pipeline_field.id,
                ),
                edge(LineageEdgeKind::ContainsField, &raw.id, &raw_field.id),
                edge(LineageEdgeKind::ContainsField, &model.id, &model_field.id),
                edge(LineageEdgeKind::ContainsField, &query.id, &query_field.id),
                edge(
                    LineageEdgeKind::FieldDerivesFrom,
                    &source_field.id,
                    &pipeline_field.id,
                ),
                edge(
                    LineageEdgeKind::FieldDerivesFrom,
                    &pipeline_field.id,
                    &raw_field.id,
                ),
                edge(
                    LineageEdgeKind::FieldDerivesFrom,
                    &raw_field.id,
                    &model_field.id,
                ),
                edge(
                    LineageEdgeKind::FieldDerivesFrom,
                    &model_field.id,
                    &query_field.id,
                ),
            ],
            ..Default::default()
        };

        let got = slice_graph(
            &graph,
            &LineageGraphQuery {
                asset: Some("analytics.raw.bike_hire".to_string()),
                field: Some("EVENT_DATE".to_string()),
                direction: LineageDirection::Both,
            },
        );
        for id in [
            &source.id,
            &pipeline.id,
            &raw.id,
            &model.id,
            &query.id,
            &source_field.id,
            &pipeline_field.id,
            &raw_field.id,
            &model_field.id,
            &query_field.id,
        ] {
            assert_eq!(
                got.nodes
                    .iter()
                    .find(|node| &node.id == id)
                    .and_then(|node| node.metadata.get("_lineage_state"))
                    .map(String::as_str),
                Some("highlighted"),
                "expected {id} to be highlighted"
            );
        }
    }

    #[test]
    fn strip_query_history_evidence_removes_query_nodes_and_edges() {
        let query = typed_node("query:q1", LineageNodeKind::Query, Some("query:q1"));
        let table = node("analytics.raw.bike_hire");
        let graph = LineageGraphSnapshot {
            nodes: vec![query.clone(), table.clone()],
            edges: vec![LineageEdge {
                id: edge_id(LineageEdgeKind::SelectsFrom, &table.id, &query.id),
                from_node_id: table.id.clone(),
                to_node_id: query.id.clone(),
                kind: LineageEdgeKind::SelectsFrom,
                provenance: LineageProvenance::unverified(
                    LineageEvidenceSource::WarehouseQueryHistory,
                    Some("q1".to_string()),
                    70,
                ),
                metadata: BTreeMap::new(),
            }],
            diagnostics: vec![LineageDiagnostic {
                severity: LineageDiagnosticSeverity::Warning,
                message: "query history warning".to_string(),
                source: Some(LineageEvidenceSource::WarehouseQueryHistory),
            }],
            ..Default::default()
        };

        let stripped = strip_query_history_evidence(graph);
        assert!(!stripped
            .nodes
            .iter()
            .any(|node| node.kind == LineageNodeKind::Query));
        assert!(stripped.edges.is_empty());
        assert!(stripped.diagnostics.is_empty());
    }

    #[test]
    fn field_slice_highlights_pipeline_and_source_when_ingest_hidden() {
        let source = typed_node(
            "raw:source",
            LineageNodeKind::RawSource,
            Some("source:raw"),
        );
        let pipeline = typed_node(
            "pipeline:bike_hire",
            LineageNodeKind::Pipeline,
            Some("bike_hire"),
        );
        let ingest = LineageNode {
            id: dataset_node_id("analytics.raw.bike_hire", LineageNodeKind::IngestTable),
            label: "analytics.raw.bike_hire".to_string(),
            kind: LineageNodeKind::IngestTable,
            dataset_id: Some("analytics.raw.bike_hire".to_string()),
            field: None,
            path: None,
            metadata: BTreeMap::new(),
        };
        let warehouse = node("analytics.raw.bike_hire");
        let source_field = field_node("source:raw", "event_date");
        let pipeline_field = field_node("bike_hire", "event_date");
        let warehouse_field = field_node("analytics.raw.bike_hire", "event_date");
        let graph = LineageGraphSnapshot {
            nodes: vec![
                source.clone(),
                pipeline.clone(),
                ingest.clone(),
                warehouse.clone(),
                source_field.clone(),
                pipeline_field.clone(),
                warehouse_field.clone(),
            ],
            edges: vec![
                edge(LineageEdgeKind::Ingests, &source.id, &pipeline.id),
                edge(LineageEdgeKind::Ingests, &pipeline.id, &ingest.id),
                edge(LineageEdgeKind::Materializes, &ingest.id, &warehouse.id),
                edge(LineageEdgeKind::ContainsField, &source.id, &source_field.id),
                edge(
                    LineageEdgeKind::ContainsField,
                    &pipeline.id,
                    &pipeline_field.id,
                ),
                edge(
                    LineageEdgeKind::ContainsField,
                    &ingest.id,
                    &warehouse_field.id,
                ),
                edge(
                    LineageEdgeKind::FieldDerivesFrom,
                    &source_field.id,
                    &pipeline_field.id,
                ),
                edge(
                    LineageEdgeKind::FieldDerivesFrom,
                    &pipeline_field.id,
                    &warehouse_field.id,
                ),
            ],
            ..Default::default()
        };

        let got = slice_graph(
            &graph,
            &LineageGraphQuery {
                asset: Some("analytics.raw.bike_hire".to_string()),
                field: Some("event_date".to_string()),
                direction: LineageDirection::Both,
            },
        );
        for id in [&source.id, &pipeline.id, &warehouse.id] {
            assert_eq!(
                got.nodes
                    .iter()
                    .find(|node| &node.id == id)
                    .and_then(|node| node.metadata.get("_lineage_state"))
                    .map(String::as_str),
                Some("highlighted"),
                "expected {id} to be highlighted"
            );
        }
        assert!(!got.nodes.iter().any(|node| node.id == ingest.id));
    }

    #[test]
    fn entity_projection_keeps_dbt_model_as_visible_transform() {
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
        assert!(got
            .edges
            .iter()
            .any(|edge| { edge.from_node_id == source.id && edge.to_node_id == model.id }));
        assert!(got
            .edges
            .iter()
            .any(|edge| { edge.from_node_id == model.id && edge.to_node_id == table.id }));
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == model.id)
                .and_then(|node| node.metadata.get("_lineage_rank"))
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == table.id)
                .and_then(|node| node.metadata.get("_lineage_rank"))
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(
            got.nodes
                .iter()
                .find(|node| node.id == query.id)
                .and_then(|node| node.metadata.get("_lineage_rank"))
                .map(String::as_str),
            Some("3")
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
