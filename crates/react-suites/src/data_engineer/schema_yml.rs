use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, BTreeSet};

fn as_str(v: &Value) -> Option<&str> {
    v.as_str().map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn parse_dataset_id(dataset_id: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = dataset_id.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string(), parts[2].to_string()))
}

/// Canonical dbt `models/schema.yml` content for the given datasets.
///
/// Format:
/// - One source per `<database>` (Glue DB / schema), grouped under its `<catalog>`.
/// - Each table listed under `tables:`.
pub fn sources_yaml_from_dataset_ids(dataset_ids: &[String]) -> Result<String, String> {
    let mut by_cat_db: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for ds in dataset_ids {
        let (cat, db, table) = parse_dataset_id(ds).ok_or_else(|| {
            format!(
                "invalid dataset_id '{ds}'. Expected <catalog>.<schema>.<table> (e.g. AwsDataCatalog.test_raw.raw_customers)."
            )
        })?;
        by_cat_db.entry((cat, db)).or_default().insert(table);
    }

    let mut root = Mapping::new();
    root.insert(Value::String("version".to_string()), Value::Number(2.into()));

    let mut sources_seq: Vec<Value> = Vec::new();
    for ((cat, db), tables) in by_cat_db.into_iter() {
        let mut src = Mapping::new();
        src.insert(Value::String("name".to_string()), Value::String(db.clone()));
        src.insert(Value::String("database".to_string()), Value::String(cat));
        src.insert(Value::String("schema".to_string()), Value::String(db));

        let mut tables_seq: Vec<Value> = Vec::new();
        for t in tables.into_iter() {
            let mut tm = Mapping::new();
            tm.insert(Value::String("name".to_string()), Value::String(t));
            tables_seq.push(Value::Mapping(tm));
        }
        src.insert(Value::String("tables".to_string()), Value::Sequence(tables_seq));
        sources_seq.push(Value::Mapping(src));
    }

    root.insert(Value::String("sources".to_string()), Value::Sequence(sources_seq));
    serde_yaml::to_string(&Value::Mapping(root)).map_err(|e| e.to_string())
}

fn ensure_mapping_root(v: Value) -> Result<Mapping, String> {
    match v {
        Value::Mapping(m) => Ok(m),
        _ => Err("models/schema.yml must be a YAML mapping at top level".to_string()),
    }
}

fn mapping_get<'a>(m: &'a Mapping, key: &str) -> Option<&'a Value> {
    m.get(&Value::String(key.to_string()))
}

fn mapping_get_mut<'a>(m: &'a mut Mapping, key: &str) -> Option<&'a mut Value> {
    m.get_mut(&Value::String(key.to_string()))
}

fn source_identity(m: &Mapping) -> (String, String, String) {
    // name + (database?) + (schema?) for best-effort matching.
    let name = mapping_get(m, "name").and_then(as_str).unwrap_or("").to_string();
    let database = mapping_get(m, "database").and_then(as_str).unwrap_or("").to_string();
    let schema = mapping_get(m, "schema").and_then(as_str).unwrap_or("").to_string();
    (name, database, schema)
}

fn normalize_tables_seq(v: &Value) -> Vec<Mapping> {
    let mut out: Vec<Mapping> = Vec::new();
    let Some(seq) = v.as_sequence() else { return out };
    for it in seq {
        match it {
            Value::Mapping(m) => out.push(m.clone()),
            Value::String(s) => {
                let t = s.trim();
                if !t.is_empty() {
                    let mut tm = Mapping::new();
                    tm.insert(Value::String("name".to_string()), Value::String(t.to_string()));
                    out.push(tm);
                }
            }
            _ => {}
        }
    }
    out
}

fn tables_by_name(ms: Vec<Mapping>) -> BTreeMap<String, Mapping> {
    let mut out: BTreeMap<String, Mapping> = BTreeMap::new();
    for m in ms {
        let name = mapping_get(&m, "name")
            .and_then(as_str)
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        // Ensure stable key ordering: name first, then the rest.
        let mut stable = Mapping::new();
        stable.insert(Value::String("name".to_string()), Value::String(name.clone()));
        for (k, v) in m.into_iter() {
            if k == Value::String("name".to_string()) {
                continue;
            }
            stable.insert(k, v);
        }
        out.entry(name).or_insert(stable);
    }
    out
}

fn rebuild_source_with_tables(src: &Mapping, tables: Vec<Mapping>) -> Mapping {
    // Stable order: name, database, schema, tables, then any remaining keys.
    let mut out = Mapping::new();
    if let Some(v) = mapping_get(src, "name") {
        out.insert(Value::String("name".to_string()), v.clone());
    }
    if let Some(v) = mapping_get(src, "database") {
        out.insert(Value::String("database".to_string()), v.clone());
    }
    if let Some(v) = mapping_get(src, "schema") {
        out.insert(Value::String("schema".to_string()), v.clone());
    }
    out.insert(
        Value::String("tables".to_string()),
        Value::Sequence(tables.into_iter().map(Value::Mapping).collect()),
    );

    for (k, v) in src.iter() {
        if k == &Value::String("name".to_string())
            || k == &Value::String("database".to_string())
            || k == &Value::String("schema".to_string())
            || k == &Value::String("tables".to_string())
        {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Merge sources/tables implied by `dataset_ids` into an existing `models/schema.yml` text.
///
/// - Monotonic: adds missing sources/tables; does not remove existing ones.
/// - Stable ordering: sources sorted by name; tables sorted by name.
pub fn merge_sources_yaml(existing: Option<&str>, dataset_ids: &[String]) -> Result<String, String> {
    // Build canonical sources from dataset_ids, then merge into existing mapping.
    let canonical = sources_yaml_from_dataset_ids(dataset_ids)?;
    let canonical_v: Value = serde_yaml::from_str(&canonical).map_err(|e| e.to_string())?;
    let canonical_root = ensure_mapping_root(canonical_v)?;
    let canonical_sources = canonical_root
        .get(&Value::String("sources".to_string()))
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();

    let mut root = if let Some(s) = existing {
        if s.trim().is_empty() {
            Mapping::new()
        } else {
            let v: Value = serde_yaml::from_str(s).map_err(|e| e.to_string())?;
            ensure_mapping_root(v)?
        }
    } else {
        Mapping::new()
    };

    // Ensure version exists (do not override if already present).
    if mapping_get(&root, "version").is_none() {
        root.insert(Value::String("version".to_string()), Value::Number(2.into()));
    }

    // Load existing sources
    let mut existing_sources: Vec<Mapping> = Vec::new();
    if let Some(v) = mapping_get(&root, "sources") {
        if let Some(seq) = v.as_sequence() {
            for it in seq {
                if let Value::Mapping(m) = it {
                    existing_sources.push(m.clone());
                }
            }
        }
    }

    // Index existing sources by identity (best-effort).
    let mut idx: Vec<(usize, (String, String, String))> = existing_sources
        .iter()
        .enumerate()
        .map(|(i, m)| (i, source_identity(m)))
        .collect();

    for src_val in canonical_sources {
        let Value::Mapping(src_new) = src_val else { continue };
        let (n_name, n_db, n_schema) = source_identity(&src_new);
        if n_name.is_empty() {
            continue;
        }

        // Find matching existing source:
        // - Prefer exact match on (name, database, schema) when those are present.
        // - Otherwise match on name only.
        let mut found: Option<usize> = None;
        for (i, (e_name, e_db, e_schema)) in idx.iter() {
            if e_name != &n_name {
                continue;
            }
            if (!n_db.is_empty() && !e_db.is_empty() && e_db != &n_db) {
                continue;
            }
            if (!n_schema.is_empty() && !e_schema.is_empty() && e_schema != &n_schema) {
                continue;
            }
            found = Some(*i);
            break;
        }

        match found {
            Some(i) => {
                // Merge tables
                let mut cur = existing_sources[i].clone();
                let cur_tables = mapping_get(&cur, "tables").map(normalize_tables_seq).unwrap_or_default();
                let mut by_name = tables_by_name(cur_tables);

                let new_tables = mapping_get(&src_new, "tables").map(normalize_tables_seq).unwrap_or_default();
                for t in new_tables {
                    let tname = mapping_get(&t, "name").and_then(as_str).unwrap_or("").to_string();
                    if tname.is_empty() {
                        continue;
                    }
                    by_name.entry(tname).or_insert(t);
                }

                // Keep database/schema if already present; otherwise fill from canonical.
                if mapping_get(&cur, "database").is_none() {
                    if let Some(v) = mapping_get(&src_new, "database") {
                        cur.insert(Value::String("database".to_string()), v.clone());
                    }
                }
                if mapping_get(&cur, "schema").is_none() {
                    if let Some(v) = mapping_get(&src_new, "schema") {
                        cur.insert(Value::String("schema".to_string()), v.clone());
                    }
                }

                let tables_sorted: Vec<Mapping> = by_name.into_iter().map(|(_k, v)| v).collect();
                existing_sources[i] = rebuild_source_with_tables(&cur, tables_sorted);
            }
            None => {
                // Add new source as-is, but ensure stable key order.
                let new_tables = mapping_get(&src_new, "tables").map(normalize_tables_seq).unwrap_or_default();
                let by_name = tables_by_name(new_tables);
                let tables_sorted: Vec<Mapping> = by_name.into_iter().map(|(_k, v)| v).collect();
                existing_sources.push(rebuild_source_with_tables(&src_new, tables_sorted));
                idx.push((existing_sources.len() - 1, source_identity(existing_sources.last().unwrap())));
            }
        }
    }

    // Sort sources by `name` for stability.
    existing_sources.sort_by(|a, b| {
        let an = mapping_get(a, "name").and_then(as_str).unwrap_or("");
        let bn = mapping_get(b, "name").and_then(as_str).unwrap_or("");
        an.cmp(bn)
    });

    // Write back sources
    let sources_val = Value::Sequence(existing_sources.into_iter().map(Value::Mapping).collect());
    match mapping_get_mut(&mut root, "sources") {
        Some(v) => *v = sources_val,
        None => {
            root.insert(Value::String("sources".to_string()), sources_val);
        }
    }

    serde_yaml::to_string(&Value::Mapping(root)).map_err(|e| e.to_string())
}

fn merge_mapping_missing(dst: &mut Mapping, src: &Mapping) {
    // Insert keys from src that are missing in dst. Do not overwrite existing values.
    for (k, v) in src.iter() {
        if !dst.contains_key(k) {
            dst.insert(k.clone(), v.clone());
        }
    }
}

fn merge_table_missing(dst: &mut Mapping, src: &Mapping) {
    // Same semantics as merge_mapping_missing, but table-aware:
    // - never overwrite existing values
    // - merge nested mappings only for keys missing at top level
    merge_mapping_missing(dst, src);
}

/// Merge an incoming `models/schema.yml` YAML text into an existing schema.yml text.
///
/// - Monotonic: adds missing sources/tables; does not remove existing ones.
/// - For existing tables/sources, merges *missing* keys from the incoming YAML (does not overwrite).
/// - Stable ordering: sources sorted by name; tables sorted by name.
pub fn merge_schema_yml(existing: Option<&str>, incoming: &str) -> Result<String, String> {
    let incoming = incoming.trim();
    if incoming.is_empty() {
        // Nothing to merge; return existing (or empty).
        return Ok(existing.unwrap_or("").to_string());
    }

    let inc_v: Value = serde_yaml::from_str(incoming).map_err(|e| e.to_string())?;
    let inc_root = ensure_mapping_root(inc_v)?;
    let inc_sources = inc_root
        .get(&Value::String("sources".to_string()))
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();

    // Parse existing (or start empty)
    let mut root = if let Some(s) = existing {
        if s.trim().is_empty() {
            Mapping::new()
        } else {
            let v: Value = serde_yaml::from_str(s).map_err(|e| e.to_string())?;
            ensure_mapping_root(v)?
        }
    } else {
        Mapping::new()
    };

    // Ensure version exists (do not override if already present).
    if mapping_get(&root, "version").is_none() {
        root.insert(Value::String("version".to_string()), Value::Number(2.into()));
    }

    // Load existing sources
    let mut existing_sources: Vec<Mapping> = Vec::new();
    if let Some(v) = mapping_get(&root, "sources") {
        if let Some(seq) = v.as_sequence() {
            for it in seq {
                if let Value::Mapping(m) = it {
                    existing_sources.push(m.clone());
                }
            }
        }
    }
    let mut idx: Vec<(usize, (String, String, String))> = existing_sources
        .iter()
        .enumerate()
        .map(|(i, m)| (i, source_identity(m)))
        .collect();

    for src_val in inc_sources {
        let Value::Mapping(src_in) = src_val else { continue };
        let (n_name, n_db, n_schema) = source_identity(&src_in);
        if n_name.is_empty() {
            continue;
        }

        // Find matching existing source (same heuristic as merge_sources_yaml).
        let mut found: Option<usize> = None;
        for (i, (e_name, e_db, e_schema)) in idx.iter() {
            if e_name != &n_name {
                continue;
            }
            if (!n_db.is_empty() && !e_db.is_empty() && e_db != &n_db) {
                continue;
            }
            if (!n_schema.is_empty() && !e_schema.is_empty() && e_schema != &n_schema) {
                continue;
            }
            found = Some(*i);
            break;
        }

        match found {
            Some(i) => {
                let mut cur = existing_sources[i].clone();

                // Merge missing source-level keys from incoming.
                merge_mapping_missing(&mut cur, &src_in);

                // Merge tables
                let cur_tables = mapping_get(&cur, "tables")
                    .map(normalize_tables_seq)
                    .unwrap_or_default();
                let mut by_name = tables_by_name(cur_tables);

                let inc_tables = mapping_get(&src_in, "tables")
                    .map(normalize_tables_seq)
                    .unwrap_or_default();

                for t_in in inc_tables {
                    let tname = mapping_get(&t_in, "name").and_then(as_str).unwrap_or("").to_string();
                    if tname.is_empty() {
                        continue;
                    }
                    match by_name.get_mut(&tname) {
                        Some(existing_table) => {
                            merge_table_missing(existing_table, &t_in);
                        }
                        None => {
                            by_name.insert(tname, t_in);
                        }
                    }
                }

                let tables_sorted: Vec<Mapping> = by_name.into_iter().map(|(_k, v)| v).collect();
                existing_sources[i] = rebuild_source_with_tables(&cur, tables_sorted);
            }
            None => {
                // Add source; normalize tables ordering.
                let inc_tables = mapping_get(&src_in, "tables").map(normalize_tables_seq).unwrap_or_default();
                let by_name = tables_by_name(inc_tables);
                let tables_sorted: Vec<Mapping> = by_name.into_iter().map(|(_k, v)| v).collect();
                existing_sources.push(rebuild_source_with_tables(&src_in, tables_sorted));
                idx.push((existing_sources.len() - 1, source_identity(existing_sources.last().unwrap())));
            }
        }
    }

    existing_sources.sort_by(|a, b| {
        let an = mapping_get(a, "name").and_then(as_str).unwrap_or("");
        let bn = mapping_get(b, "name").and_then(as_str).unwrap_or("");
        an.cmp(bn)
    });

    let sources_val = Value::Sequence(existing_sources.into_iter().map(Value::Mapping).collect());
    match mapping_get_mut(&mut root, "sources") {
        Some(v) => *v = sources_val,
        None => {
            root.insert(Value::String("sources".to_string()), sources_val);
        }
    }

    serde_yaml::to_string(&Value::Mapping(root)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_sources(yml: &str) -> Vec<Mapping> {
        let v: Value = serde_yaml::from_str(yml).expect("yaml parse");
        let m = v.as_mapping().expect("root mapping");
        let sources = m
            .get(&Value::String("sources".to_string()))
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default();
        sources
            .into_iter()
            .filter_map(|v| v.as_mapping().cloned())
            .collect()
    }

    fn find_source<'a>(sources: &'a [Mapping], name: &str) -> Option<&'a Mapping> {
        sources.iter().find(|s| {
            s.get(&Value::String("name".to_string()))
                .and_then(|v| v.as_str())
                == Some(name)
        })
    }

    fn table_names(src: &Mapping) -> Vec<String> {
        src.get(&Value::String("tables".to_string()))
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_mapping().cloned())
            .filter_map(|m| {
                m.get(&Value::String("name".to_string()))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    #[test]
    fn merge_sources_yaml_is_monotonic_and_preserves_table_metadata() {
        let existing = r#"
version: 2
sources:
  - name: test_raw
    database: AwsDataCatalog
    schema: test_raw
    tables:
      - name: raw_customers
        description: "customers table"
"#;
        let dataset_ids = vec!["AwsDataCatalog.test_raw.raw_orders".to_string()];
        let merged = merge_sources_yaml(Some(existing), &dataset_ids).expect("merge ok");
        let sources = get_sources(&merged);
        let s = find_source(&sources, "test_raw").expect("source exists");
        let mut names = table_names(s);
        names.sort();
        assert_eq!(names, vec!["raw_customers".to_string(), "raw_orders".to_string()]);

        // Ensure existing description survived
        let tables = s
            .get(&Value::String("tables".to_string()))
            .and_then(|v| v.as_sequence())
            .unwrap();
        let cust = tables
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("raw_customers"))
            .unwrap();
        assert_eq!(
            cust.get("description").and_then(|v| v.as_str()),
            Some("customers table")
        );
    }

    #[test]
    fn merge_schema_yml_merges_incoming_without_overwriting_existing() {
        let existing = r#"
version: 2
sources:
  - name: test_raw
    database: AwsDataCatalog
    schema: test_raw
    loader: glue
    tables:
      - name: raw_customers
        description: "keep me"
"#;
        let incoming = r#"
version: 2
sources:
  - name: test_raw
    database: AwsDataCatalog
    schema: test_raw
    tables:
      - name: raw_customers
        description: "do not overwrite"
      - name: raw_orders
        description: "orders table"
"#;
        let merged = merge_schema_yml(Some(existing), incoming).expect("merge ok");
        let sources = get_sources(&merged);
        let s = find_source(&sources, "test_raw").expect("source exists");
        let mut names = table_names(s);
        names.sort();
        assert_eq!(names, vec!["raw_customers".to_string(), "raw_orders".to_string()]);

        // raw_customers description must remain the existing one
        let tables = s
            .get(&Value::String("tables".to_string()))
            .and_then(|v| v.as_sequence())
            .unwrap();
        let cust = tables
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("raw_customers"))
            .unwrap();
        assert_eq!(cust.get("description").and_then(|v| v.as_str()), Some("keep me"));

        // Source-level key 'loader' should remain
        assert_eq!(s.get(&Value::String("loader".to_string())).and_then(|v| v.as_str()), Some("glue"));
    }
}

