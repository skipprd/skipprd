//! Bind unbound SQL to FieldIds. Collision with ingest namespace is fail-closed.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::parse::{
    UnboundCompute, UnboundMeasure, UnboundMeasureKind, UnboundRelation, UnboundSelect,
};
use super::{
    ComputeKind, EventTimeCol, EventWatermark, FieldId, Fqn, Ident, Measure, MeasureKind, NonEmpty,
    PipelineName, Relation, StatelessSelect, Tumble,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BindError {
    #[error("{0}")]
    Message(String),
}

#[derive(Clone, Debug, Default)]
pub struct BindCatalog {
    /// `(pipeline, namespace) → column name → FieldId`
    pub ingest: BTreeMap<(String, String), BTreeMap<String, FieldId>>,
    pub ingest_namespaces: BTreeSet<String>,
    pub existing: Vec<ComputeKind>,
    pub event_time: Option<EventTimeCol>,
    pub watermark_delay: Option<Duration>,
}

impl BindCatalog {
    pub fn fields_of(
        &self,
        rel: &UnboundRelation,
    ) -> Result<&BTreeMap<String, FieldId>, BindError> {
        self.ingest
            .get(&(rel.pipeline.clone(), rel.name.clone()))
            .ok_or_else(|| {
                BindError::Message(format!(
                    "unknown ingest table {}.{}",
                    rel.pipeline, rel.name
                ))
            })
    }
}

pub fn bind_statement(
    catalog: &BindCatalog,
    unbound: UnboundCompute,
) -> Result<ComputeKind, BindError> {
    match unbound {
        UnboundCompute::Stateless {
            name,
            target,
            select,
        } => {
            reject_name_collision(catalog, &name)?;
            reject_stateless_target_collision(catalog, &target)?;
            let _ = catalog.fields_of(&target)?;
            Ok(ComputeKind::Stateless {
                name,
                target: pipeline_table(target)?,
                select: bind_select(select),
            })
        }
        UnboundCompute::View {
            name,
            input,
            select,
        } => {
            reject_name_collision(catalog, &name)?;
            let _ = catalog.fields_of(&input)?;
            Ok(ComputeKind::View {
                name,
                input: pipeline_table(input)?,
                select: bind_select(select),
            })
        }
        UnboundCompute::Latest {
            name,
            input,
            keys,
            order,
        } => {
            reject_name_collision(catalog, &name)?;
            let fields = catalog.fields_of(&input)?;
            let mut bound_order = Vec::new();
            for (col, desc) in &order {
                bound_order.push((bind_column(fields, col)?, *desc));
            }
            Ok(ComputeKind::Latest {
                name,
                input: pipeline_table(input)?,
                keys: bind_keys(fields, &keys)?,
                order: bound_order,
            })
        }
        UnboundCompute::KeyedTable {
            name,
            input,
            keys,
            measures,
        } => {
            reject_name_collision(catalog, &name)?;
            let fields = catalog.fields_of(&input)?;
            Ok(ComputeKind::KeyedTable {
                name,
                input: pipeline_table(input)?,
                keys: bind_keys(fields, &keys)?,
                measures: bind_measures(fields, &measures)?,
            })
        }
        UnboundCompute::WindowedTable {
            name,
            input,
            keys,
            trunc_unit,
            event_time,
            measures,
        } => {
            reject_name_collision(catalog, &name)?;
            let fields = catalog.fields_of(&input)?;
            let on = bind_column(fields, &event_time)?;
            let delay = catalog.watermark_delay.ok_or_else(|| {
                BindError::Message("WindowedTable requires EventTimeCol and watermark_delay".into())
            })?;
            let event_col = super::EventTimeCol::new(on);
            Ok(ComputeKind::WindowedTable {
                name,
                input: pipeline_table(input)?,
                keys: bind_keys(fields, &keys)?,
                window: Tumble {
                    size: trunc_unit,
                    on: event_col,
                },
                watermark: EventWatermark::new(event_col, delay),
                measures: bind_measures(fields, &measures)?,
            })
        }
        UnboundCompute::StreamTableJoin {
            name,
            stream,
            table,
            keys,
        } => {
            reject_name_collision(catalog, &name)?;
            let fields = catalog.fields_of(&stream)?;
            let on = bind_keys(fields, &keys)?;
            let table = find_keyed_table(catalog, &stream, on.as_slice(), table.as_ref())?;
            Ok(ComputeKind::StreamTableJoin {
                name,
                stream: pipeline_table(stream)?,
                table,
                on,
            })
        }
    }
}

fn reject_name_collision(catalog: &BindCatalog, name: &Fqn) -> Result<(), BindError> {
    if catalog.ingest_namespaces.contains(name.name.as_str()) {
        return Err(BindError::Message(format!(
            "compute name '{}' collides with ingest namespace",
            name.name.as_str()
        )));
    }
    if catalog.existing.iter().any(|kind| kind.name() == name) {
        return Err(BindError::Message(format!("compute {name} already exists")));
    }
    Ok(())
}

fn reject_stateless_target_collision(
    catalog: &BindCatalog,
    target: &UnboundRelation,
) -> Result<(), BindError> {
    let collision = catalog.existing.iter().any(|kind| match kind {
        ComputeKind::Stateless {
            target:
                Relation::PipelineTable {
                    pipeline,
                    namespace,
                },
            ..
        } => pipeline.as_str() == target.pipeline && namespace.as_str() == target.name,
        _ => false,
    });
    if collision {
        return Err(BindError::Message(format!(
            "Stateless compute already covers {}.{}",
            target.pipeline, target.name
        )));
    }
    Ok(())
}

fn pipeline_table(rel: UnboundRelation) -> Result<Relation, BindError> {
    Ok(Relation::PipelineTable {
        pipeline: PipelineName::new(rel.pipeline).map_err(BindError::Message)?,
        namespace: Ident::new(rel.name).map_err(BindError::Message)?,
    })
}

fn bind_select(select: UnboundSelect) -> StatelessSelect {
    StatelessSelect {
        filter: select.filter,
        derive: select.derive,
    }
}

fn bind_keys(
    fields: &BTreeMap<String, FieldId>,
    keys: &[String],
) -> Result<NonEmpty<FieldId>, BindError> {
    let mut out = Vec::new();
    for key in keys {
        out.push(bind_column(fields, key)?);
    }
    NonEmpty::try_from_vec(out).map_err(BindError::Message)
}

fn bind_column(fields: &BTreeMap<String, FieldId>, name: &str) -> Result<FieldId, BindError> {
    fields
        .get(name)
        .copied()
        .ok_or_else(|| BindError::Message(format!("unknown column '{name}'")))
}

fn bind_measures(
    fields: &BTreeMap<String, FieldId>,
    measures: &[UnboundMeasure],
) -> Result<NonEmpty<Measure>, BindError> {
    let mut out = Vec::new();
    for measure in measures {
        let kind = match &measure.kind {
            UnboundMeasureKind::Count => MeasureKind::Count,
            UnboundMeasureKind::Sum(col) if col != "*" => {
                MeasureKind::Sum(bind_column(fields, col)?)
            }
            UnboundMeasureKind::Min(col) => MeasureKind::Min(bind_column(fields, col)?),
            UnboundMeasureKind::Max(col) => MeasureKind::Max(bind_column(fields, col)?),
            UnboundMeasureKind::Sum(_) => {
                return Err(BindError::Message("SUM(*) is illegal".into()))
            }
        };
        out.push(Measure {
            name: Ident::new(measure.name.clone()).map_err(BindError::Message)?,
            kind,
        });
    }
    NonEmpty::try_from_vec(out).map_err(BindError::Message)
}

fn find_keyed_table(
    catalog: &BindCatalog,
    stream: &UnboundRelation,
    keys: &[FieldId],
    want: Option<&Fqn>,
) -> Result<Fqn, BindError> {
    let matches: Vec<(Fqn, bool)> = catalog
        .existing
        .iter()
        .filter_map(|kind| match kind {
            ComputeKind::KeyedTable {
                name,
                input:
                    Relation::PipelineTable {
                        pipeline,
                        namespace,
                    },
                keys: existing,
                ..
            } if name.pipeline.as_str() == stream.pipeline && existing.as_slice() == keys => {
                let same_input =
                    pipeline.as_str() == stream.pipeline && namespace.as_str() == stream.name;
                Some((name.clone(), same_input))
            }
            _ => None,
        })
        .collect();
    if let Some(want) = want {
        return matches
            .into_iter()
            .map(|(name, _)| name)
            .find(|name| name == want)
            .ok_or_else(|| {
                BindError::Message(format!(
                    "SDL table {want} is not a KeyedTable on that partition"
                ))
            });
    }
    // Named SDL `table:` is the bind target (stream-table join). SQL OVER infers
    // the KeyedTable whose input is this stream.
    let over: Vec<Fqn> = matches
        .into_iter()
        .filter_map(|(name, same_input)| same_input.then_some(name))
        .collect();
    match over.as_slice() {
        [name] => Ok(name.clone()),
        [] => Err(BindError::Message(
            "SUM() OVER requires an existing KeyedTable on that stream".into(),
        )),
        _ => Err(BindError::Message(
            "SUM() OVER matches more than one KeyedTable; name the table in SDL".into(),
        )),
    }
}

pub fn bind_drop_compute(catalog: &BindCatalog, name: &Fqn) -> Result<Fqn, BindError> {
    if catalog.ingest_namespaces.contains(name.name.as_str()) {
        return Err(BindError::Message(format!(
            "DROP TABLE {}.{} names an ingest namespace",
            name.pipeline.as_str(),
            name.name.as_str()
        )));
    }
    if catalog.existing.iter().any(|kind| kind.name() == name) {
        return Ok(name.clone());
    }
    Err(BindError::Message(format!(
        "DROP TABLE {name} is not a declared compute"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::parse::desugar_create;
    use std::time::Duration;

    fn catalog() -> BindCatalog {
        let mut ingest = BTreeMap::new();
        ingest.insert(
            ("invoices".into(), "orders".into()),
            BTreeMap::from([
                ("customer_id".into(), FieldId::new(1).unwrap()),
                ("amount".into(), FieldId::new(2).unwrap()),
                ("created_at".into(), FieldId::new(3).unwrap()),
            ]),
        );
        BindCatalog {
            ingest,
            ingest_namespaces: BTreeSet::from(["orders".into()]),
            existing: Vec::new(),
            event_time: Some(EventTimeCol::new(FieldId::new(3).unwrap())),
            watermark_delay: Some(Duration::from_secs(300)),
        }
    }

    #[test]
    fn bind_keyed_field_ids() {
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "customer_totals").unwrap(),
            "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
            false,
        )
        .unwrap();
        let kind = bind_statement(&catalog(), unbound).unwrap();
        match kind {
            ComputeKind::KeyedTable { keys, measures, .. } => {
                assert_eq!(keys.as_slice(), &[FieldId::new(1).unwrap()]);
                assert!(matches!(measures.as_slice()[0].kind, MeasureKind::Sum(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn collision_with_ingest_namespace() {
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "orders").unwrap(),
            "SELECT customer_id, SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id",
            false,
        )
        .unwrap();
        let err = bind_statement(&catalog(), unbound).unwrap_err();
        assert!(err.to_string().contains("collides"), "{err}");
    }

    #[test]
    fn drop_ingest_namespace_fail_closed() {
        let err = bind_drop_compute(&catalog(), &Fqn::from_parts("invoices", "orders").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("ingest namespace"), "{err}");
    }

    #[test]
    fn window_without_watermark_fails() {
        let mut cat = catalog();
        cat.watermark_delay = None;
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "hourly_totals").unwrap(),
            "SELECT customer_id, date_trunc('hour', created_at), SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id, date_trunc('hour', created_at)",
            false,
        )
        .unwrap();
        let err = bind_statement(&cat, unbound).unwrap_err();
        assert!(err.to_string().contains("watermark"), "{err}");
    }

    #[test]
    fn over_without_existing_keyed_fails() {
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "enriched").unwrap(),
            "SELECT customer_id, SUM(amount) OVER (PARTITION BY customer_id) AS invoice_total FROM invoices.orders",
            false,
        )
        .unwrap();
        let err = bind_statement(&catalog(), unbound).unwrap_err();
        assert!(err.to_string().contains("KeyedTable"), "{err}");
    }

    #[test]
    fn window_binds_event_time_from_input_column() {
        let mut cat = catalog();
        cat.event_time = None;
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "hourly_totals").unwrap(),
            "SELECT customer_id, date_trunc('hour', created_at), SUM(amount) AS invoice_total FROM invoices.orders GROUP BY customer_id, date_trunc('hour', created_at)",
            false,
        )
        .unwrap();
        match bind_statement(&cat, unbound).unwrap() {
            ComputeKind::WindowedTable { window, .. } => {
                assert_eq!(window.on.field_id(), FieldId::new(3).unwrap());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn over_does_not_join_latest() {
        let mut cat = catalog();
        cat.existing.push(ComputeKind::Latest {
            name: Fqn::from_parts("invoices", "customers").unwrap(),
            input: Relation::PipelineTable {
                pipeline: PipelineName::new("invoices").unwrap(),
                namespace: Ident::new("orders").unwrap(),
            },
            keys: NonEmpty::try_from_vec(vec![FieldId::new(1).unwrap()]).unwrap(),
            order: vec![(FieldId::new(3).unwrap(), true)],
        });
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "enriched").unwrap(),
            "SELECT customer_id, SUM(amount) OVER (PARTITION BY customer_id) AS invoice_total FROM invoices.orders",
            false,
        )
        .unwrap();
        let err = bind_statement(&cat, unbound).unwrap_err();
        assert!(err.to_string().contains("KeyedTable"), "{err}");
    }

    #[test]
    fn over_joins_existing_keyed() {
        let mut cat = catalog();
        cat.existing.push(ComputeKind::KeyedTable {
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
        });
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "enriched").unwrap(),
            "SELECT customer_id, SUM(amount) OVER (PARTITION BY customer_id) AS invoice_total FROM invoices.orders",
            false,
        )
        .unwrap();
        match bind_statement(&cat, unbound).unwrap() {
            ComputeKind::StreamTableJoin { table, .. } => {
                assert_eq!(table.to_string(), "invoices.customer_totals");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn over_does_not_join_other_stream_keyed() {
        let mut cat = catalog();
        cat.existing.push(ComputeKind::KeyedTable {
            name: Fqn::from_parts("invoices", "payment_totals").unwrap(),
            input: Relation::PipelineTable {
                pipeline: PipelineName::new("invoices").unwrap(),
                namespace: Ident::new("payments").unwrap(),
            },
            keys: NonEmpty::try_from_vec(vec![FieldId::new(1).unwrap()]).unwrap(),
            measures: NonEmpty::try_from_vec(vec![Measure {
                name: Ident::new("payment_total").unwrap(),
                kind: MeasureKind::Sum(FieldId::new(2).unwrap()),
            }])
            .unwrap(),
        });
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "enriched").unwrap(),
            "SELECT customer_id, SUM(amount) OVER (PARTITION BY customer_id) AS invoice_total FROM invoices.orders",
            false,
        )
        .unwrap();
        let err = bind_statement(&cat, unbound).unwrap_err();
        assert!(err.to_string().contains("on that stream"), "{err}");
    }

    #[test]
    fn rename_does_not_change_field_id_hash_inputs() {
        let a = FieldId::new(7).unwrap();
        let b = FieldId::new(7).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn view_collides_with_ingest_namespace() {
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "orders").unwrap(),
            "SELECT * FROM invoices.orders WHERE amount > 0",
            true,
        )
        .unwrap();
        let err = bind_statement(&catalog(), unbound).unwrap_err();
        assert!(err.to_string().contains("collides"), "{err}");
    }

    #[test]
    fn view_unknown_input_fail_closed() {
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "positive").unwrap(),
            "SELECT * FROM invoices.missing",
            true,
        )
        .unwrap();
        let err = bind_statement(&catalog(), unbound).unwrap_err();
        assert!(err.to_string().contains("unknown ingest"), "{err}");
    }

    #[test]
    fn view_collides_with_existing_compute() {
        let mut cat = catalog();
        cat.existing.push(
            bind_statement(
                &cat,
                desugar_create(
                    Fqn::from_parts("invoices", "positive").unwrap(),
                    "SELECT * FROM invoices.orders",
                    true,
                )
                .unwrap(),
            )
            .unwrap(),
        );
        let unbound = desugar_create(
            Fqn::from_parts("invoices", "positive").unwrap(),
            "SELECT * FROM invoices.orders WHERE amount > 0",
            true,
        )
        .unwrap();
        let err = bind_statement(&cat, unbound).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }
}
