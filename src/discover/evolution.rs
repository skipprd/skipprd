use crate::discover::PipelineMetadata;
use crate::discover::{AnalyseSchema, Metadata, SkipprDataType, LAST_SUCCESSFUL_EVOLUTION};
use crate::ingest::fast_ingest::fast_set_value;
use crate::ingest::ingest::{discover_ingest, ResolvedFieldValue};
use arrow::error::ArrowError;
use serde_derive::{Deserialize, Serialize};
use serde_json::value::Value;
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::str::FromStr;
use tracing::info;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Evolution {
    pub type_string: SkipprDataType,
    pub new_field: String,
    pub sovled: bool,
}

#[derive(Clone, Debug)]
enum EvolutionType {
    // Cast,
    New,
    Rename,
    Merge,
    Default,
}

impl FromStr for EvolutionType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            // "cast" => Ok(EvolutionType::Cast),
            "new" => Ok(EvolutionType::New),
            "rename" => Ok(EvolutionType::Rename),
            "merge" => Ok(EvolutionType::Merge),
            "default" => Ok(EvolutionType::Default),
            _ => Err(()),
        }
    }
}

impl EvolutionType {
    #[allow(dead_code)]
    fn to_string(&self) -> String {
        match *self {
            // EvolutionType::Cast => "cast",
            EvolutionType::New => "new".to_string(),
            EvolutionType::Rename => "rename".to_string(),
            EvolutionType::Merge => "merge".to_string(),
            EvolutionType::Default => "default".to_string(),
        }
    }
}

impl Evolution {
    pub fn evolve_array_field(
        field: &String,
        value: &Value,
        parent_field: Option<&str>,
        parent_data_type: Option<&str>,
        metadata: &mut HashMap<String, Metadata>,
    ) {
        // Create new, evolved field. Will ensure field is included in schemas
        let temp_feild_name = &format!("{}_{}", field, "unknown");

        let mut update_schmea_mock = "no".to_string();

        discover_ingest(
            temp_feild_name,
            value,
            parent_field,
            parent_data_type,
            metadata,
            &mut update_schmea_mock,
        );

        let discoverd_data_type = metadata
            .get(temp_feild_name)
            .unwrap()
            .determined_type_values
            .clone()
            .unwrap_or(SkipprDataType::String);

        let new_feild_name = &format!("{}_array_{}", field, discoverd_data_type);

        //rename metadata[temp_feild_name] to new_feild_name
        let new = metadata.remove(temp_feild_name).unwrap();
        metadata.insert(new_feild_name.clone(), new);

        // println!("New data type for array field: '{}' is: '{}'", new_feild_name, discoverd_data_type);

        // Update existing field with the evolution to the new field
        let evo = Evolution {
            type_string: SkipprDataType::Array,
            new_field: new_feild_name.clone(),
            sovled: true,
        };

        metadata
            .get_mut(field)
            .unwrap()
            .evolution
            .insert("array".to_string(), evo.clone());
    }

    pub fn evolve_field(
        field: &String,
        value: &Value,
        parent_field: Option<&str>,
        parent_data_type: Option<&str>,
        metadata: &mut HashMap<String, Metadata>,
        updated_schema: &mut String,
        flatten: bool,
    ) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
        // println!("Handling value error for field: '{}'", field);

        let foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let discoverd_data_type = foo.resolve_field_type(
            metadata.clone().borrow_mut(),
            &field.to_string(),
            value.clone().borrow_mut(),
        );

        // println!("Evolving new data type: '{}' for field '{}' with value '{}' with current data type of '{}'", discoverd_data_type, field, value, metadata.get(field).unwrap().determined_type);

        if discoverd_data_type != SkipprDataType::Unknown {
            let new_feild_name = &format!("{}_{}", field, discoverd_data_type);

            match metadata.get(field).unwrap().evolution.get(new_feild_name) {
                Some(_evolution) => {
                    if discoverd_data_type == SkipprDataType::Array {
                        *updated_schema = "yes".to_string();

                        Evolution::evolve_array_field(
                            field,
                            value,
                            parent_field,
                            parent_data_type,
                            metadata,
                        );
                    } else {
                        // println!("Already have an evolution for field: '{}' to type: '{}'", field, discoverd_data_type);
                        // println!("Evolution: {:?}", evolution);
                    }
                }
                None => {
                    if discoverd_data_type == SkipprDataType::Array {
                        *updated_schema = "yes".to_string();

                        Evolution::evolve_array_field(
                            field,
                            value,
                            parent_field,
                            parent_data_type,
                            metadata,
                        );
                        return Ok(ResolvedFieldValue::new(
                            new_feild_name.clone(),
                            value.clone(),
                        ));
                    }

                    info!(
                        "Creating new evolution for field: '{}' to type: '{}'",
                        field, discoverd_data_type
                    );

                    *updated_schema = "yes".to_string();

                    // Create new, evolved field. Will ensure field is included in schemas
                    let new_feild_name = &format!("{}_{}", field, discoverd_data_type);

                    let mut update_schmea_mock = "no".to_string();

                    discover_ingest(
                        new_feild_name,
                        value,
                        parent_field,
                        parent_data_type,
                        metadata,
                        &mut update_schmea_mock,
                    );

                    // Update existing field with the evolution to the new field
                    let evo = Evolution {
                        type_string: discoverd_data_type.clone(),
                        new_field: new_feild_name.clone(),
                        sovled: true,
                    };

                    metadata
                        .get_mut(field)
                        .unwrap()
                        .evolution
                        .insert(new_feild_name.to_string(), evo.clone());
                }
            };

            match Evolution::apply_evolution_factory(field, value, metadata, flatten) {
                Ok(v) => Ok(v),
                Err(e) => {
                    // println!("#### Error applying evolution factory: {}", e);
                    Err(e)
                }
            }
        } else {
            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Unable to determine data type",
            )))
        }
    }

    pub fn apply_evolution_factory(
        field: &str,
        value: &Value,
        metadata: &mut HashMap<String, Metadata>,
        flatten: bool,
    ) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
        // println!("Applying evolution factory for field: '{}'", field);

        // get the cached last successful evolution for this field and try it first
        let mut found_value: Option<Result<ResolvedFieldValue, Box<dyn std::error::Error>>> = None;
        LAST_SUCCESSFUL_EVOLUTION.with(|last_evolution_refcell| {
            let last_evolution_guard = last_evolution_refcell.borrow();
            if let Some(evolution_key) = last_evolution_guard.get(field) {
                // Copy out needed data to avoid holding immutable borrows across mutation
                let (_cached_key, new_field_name, type_string_opt): (
                    String,
                    Option<String>,
                    Option<SkipprDataType>,
                ) = {
                    if let Some(field_metadata) = metadata.get(field) {
                        if let Some(evolution) = field_metadata.evolution.get(evolution_key) {
                            (
                                evolution_key.clone(),
                                Some(evolution.new_field.clone()),
                                Some(evolution.type_string.clone()),
                            )
                        } else {
                            (evolution_key.clone(), None, None)
                        }
                    } else {
                        (evolution_key.clone(), None, None)
                    }
                };

                if let (Some(new_field), Some(type_string)) = (new_field_name, type_string_opt) {
                    // Try fast mapping first; only discover if needed
                    match fast_set_value(
                        type_string.as_str(),
                        &new_field,
                        value,
                        metadata,
                        Some(false),
                        flatten,
                    ) {
                        Ok(v) => {
                            // Ensure minimal metadata exists for the evolved sibling when fast path succeeds
                            if !metadata.contains_key(&new_field) {
                                if let Ok(mut m) = Metadata::new() {
                                    m.determined_type = type_string.clone();
                                    m.out_field_name = new_field.clone();
                                    metadata.insert(new_field.clone(), m);
                                }
                            }
                            found_value =
                                Some(Ok(ResolvedFieldValue::new(new_field.clone(), v.value)));
                        }
                        Err(_) => {
                            let mut updated = "no".to_string();
                            discover_ingest(&new_field, value, None, None, metadata, &mut updated);
                            if let Ok(v2) = fast_set_value(
                                type_string.as_str(),
                                &new_field,
                                value,
                                metadata,
                                Some(false),
                                flatten,
                            ) {
                                if !metadata.contains_key(&new_field) {
                                    if let Ok(mut m) = Metadata::new() {
                                        m.determined_type = type_string.clone();
                                        m.out_field_name = new_field.clone();
                                        metadata.insert(new_field.clone(), m);
                                    }
                                }
                                found_value =
                                    Some(Ok(ResolvedFieldValue::new(new_field.clone(), v2.value)));
                            }
                        }
                    }
                }
            }
        });

        if let Some(resolved_value) = found_value {
            return resolved_value;
        }

        let mut errors = vec![];

        // iterate through the evolutions and try the existing ones
        match metadata.get(field) {
            Some(field_metadata) => {
                // Copy evolutions out to avoid holding immutable borrow while mutating metadata
                let evolutions: Vec<(String, Evolution)> = field_metadata
                    .evolution
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                // Rank evolutions by how specific/appropriate they are for the given value
                fn score_evolution(
                    value: &Value,
                    evolution: &Evolution,
                    metadata: &HashMap<String, Metadata>,
                ) -> i32 {
                    let mut s: i32 = 0;
                    match &evolution.type_string {
                        SkipprDataType::TimestampMilli => {
                            // Prefer millis when the numeric magnitude suggests ms (>= 1e12)
                            if let Some(i) = value.as_i64() {
                                if i >= 1_000_000_000_000 {
                                    s += 70;
                                } else {
                                    s += 10;
                                }
                            } else {
                                s += 30;
                            }
                        }
                        SkipprDataType::Timestamp => {
                            // Prefer seconds when the numeric magnitude suggests seconds (< 1e12)
                            if let Some(i) = value.as_i64() {
                                if i < 1_000_000_000_000 {
                                    s += 70;
                                } else {
                                    s += 10;
                                }
                            } else {
                                s += 30;
                            }
                        }
                        SkipprDataType::Double => {
                            s += 25;
                            if value.is_f64() {
                                s += 10;
                            }
                        }
                        SkipprDataType::Long => {
                            s += 18;
                            if value.is_i64() {
                                s += 5;
                            }
                        }
                        SkipprDataType::Integer => {
                            s += 16;
                            if value.is_i64() {
                                s += 4;
                            }
                        }
                        SkipprDataType::Map => {
                            s += 12;
                            if value.is_object() {
                                s += 5;
                            }
                            if let Some(m) = metadata.get(&evolution.new_field) {
                                if m.determined_type_values.is_some() {
                                    if m.determined_type_values == Some(SkipprDataType::Double) {
                                        s += 12;
                                    } else if m.determined_type_values == Some(SkipprDataType::Long)
                                    {
                                        s += 8;
                                    } else {
                                        s += 4;
                                    }
                                }
                            }
                        }
                        SkipprDataType::Array => {
                            s += 12;
                            if value.is_array() {
                                s += 5;
                            }
                            if let Some(m) = metadata.get(&evolution.new_field) {
                                if m.determined_type_values.is_some() {
                                    if m.determined_type_values == Some(SkipprDataType::Double) {
                                        s += 12;
                                    } else if m.determined_type_values == Some(SkipprDataType::Long)
                                    {
                                        s += 8;
                                    } else {
                                        s += 4;
                                    }
                                }
                            }
                        }
                        SkipprDataType::Record => {
                            s += 14;
                            if value.is_object() {
                                s += 4;
                            }
                        }
                        _ => {}
                    }
                    s
                }

                let mut ranked: Vec<(i32, (String, Evolution))> = evolutions
                    .into_iter()
                    .map(|(k, e)| (score_evolution(value, &e, metadata), (k, e)))
                    .collect();
                ranked.sort_by(|a, b| b.0.cmp(&a.0));

                for (_score, (evolution_key, evolution)) in ranked.into_iter() {
                    // Try fast mapping first; if it fails, discover then retry
                    match fast_set_value(
                        evolution.type_string.as_str(),
                        &evolution.new_field,
                        value,
                        metadata,
                        Some(false),
                        flatten,
                    ) {
                        Ok(v) => {
                            if !metadata.contains_key(&evolution.new_field) {
                                if let Ok(mut m) = Metadata::new() {
                                    m.determined_type = evolution.type_string.clone();
                                    m.out_field_name = evolution.new_field.clone();
                                    metadata.insert(evolution.new_field.clone(), m);
                                }
                            }
                            LAST_SUCCESSFUL_EVOLUTION.with(|last_evolution_refcell| {
                                let mut last_evolution_guard = last_evolution_refcell.borrow_mut();
                                last_evolution_guard
                                    .insert(field.to_string(), evolution_key.clone());
                            });
                            return Ok(ResolvedFieldValue::new(
                                evolution.new_field.clone(),
                                v.value,
                            ));
                        }
                        Err(first_err) => {
                            let mut updated = "no".to_string();
                            discover_ingest(
                                &evolution.new_field,
                                value,
                                None,
                                None,
                                metadata,
                                &mut updated,
                            );
                            match fast_set_value(
                                evolution.type_string.as_str(),
                                &evolution.new_field,
                                value,
                                metadata,
                                Some(false),
                                flatten,
                            ) {
                                Ok(v2) => {
                                    if !metadata.contains_key(&evolution.new_field) {
                                        if let Ok(mut m) = Metadata::new() {
                                            m.determined_type = evolution.type_string.clone();
                                            m.out_field_name = evolution.new_field.clone();
                                            metadata.insert(evolution.new_field.clone(), m);
                                        }
                                    }
                                    LAST_SUCCESSFUL_EVOLUTION.with(|last_evolution_refcell| {
                                        let mut last_evolution_guard =
                                            last_evolution_refcell.borrow_mut();
                                        last_evolution_guard
                                            .insert(field.to_string(), evolution_key.clone());
                                    });
                                    return Ok(ResolvedFieldValue::new(
                                        evolution.new_field.clone(),
                                        v2.value,
                                    ));
                                }
                                Err(second_err) => {
                                    errors.push(first_err);
                                    errors.push(second_err);
                                }
                            }
                        }
                    }
                }

                // throw Err() if no evolutions are Ok()
                // println!("No evolutions succeeded for field: '{}'", field);
                Err(Box::new(ArrowError::ParseError(format!(
                    "No schema evolutions succeeded for field: '{}', Errors: {}",
                    field,
                    errors
                        .iter()
                        .map(|e| e.to_string())
                        .collect::<Vec<String>>()
                        .join(", ")
                ))))
            }
            None => {
                // println!("No metadata for evolution field: '{}'", field);
                return Err(Box::new(ArrowError::ParseError(format!(
                    "No metadata for evolution field: '{}'",
                    field
                ))));
            }
        }
    }
}

// Proposal DTOs used by the sequencer/data-plane
#[derive(Clone, Debug)]
pub struct EvolutionSpec {
    pub parent: Option<String>,
    pub field: String,
    pub required_type: SkipprDataType,
    pub values_type: Option<SkipprDataType>,
}

#[derive(Clone, Debug)]
pub struct EvolutionProposal {
    pub namespace: String,
    pub fields: Vec<EvolutionSpec>,
}

pub fn infer_specs_for_record(
    record: &serde_json::Value,
    metadata: &HashMap<String, Metadata>,
) -> Vec<EvolutionSpec> {
    let mut specs = Vec::new();
    if let Some(obj) = record.as_object() {
        for (k, v) in obj.iter() {
            match metadata.get(k) {
                None => {
                    // brand new field
                    let (req, vals) = infer_required_type(v);
                    specs.push(EvolutionSpec {
                        parent: None,
                        field: k.clone(),
                        required_type: req,
                        values_type: vals,
                    });
                }
                Some(md) => {
                    // existing field but possibly different type -> propose sibling evolution
                    let analyser = AnalyseSchema { i: 0 };
                    // Work on a temp copy to reuse resolver
                    let mut temp = metadata.clone();
                    let detected = analyser.resolve_field_type(
                        temp.borrow_mut(),
                        &k.to_string(),
                        v.clone().borrow_mut(),
                    );
                    if detected != SkipprDataType::Unknown && detected != md.determined_type {
                        if detected == SkipprDataType::Array {
                            let (req, vals) = infer_required_type(v);
                            specs.push(EvolutionSpec {
                                parent: None,
                                field: k.clone(),
                                required_type: req,
                                values_type: vals,
                            });
                        } else {
                            specs.push(EvolutionSpec {
                                parent: None,
                                field: k.clone(),
                                required_type: detected,
                                values_type: None,
                            });
                        }
                    }
                }
            }
        }
    }
    specs
}

fn infer_required_type(value: &serde_json::Value) -> (SkipprDataType, Option<SkipprDataType>) {
    use serde_json::Value as V;
    match value {
        V::Null => (SkipprDataType::String, None),
        V::Bool(_) => (SkipprDataType::Boolean, None),
        V::Number(n) => {
            if n.is_i64() {
                (SkipprDataType::Long, None)
            } else {
                (SkipprDataType::Double, None)
            }
        }
        V::String(_) => (SkipprDataType::String, None),
        V::Array(arr) => {
            if let Some(V::Object(_)) = arr.get(0) {
                (SkipprDataType::Array, Some(SkipprDataType::Record))
            } else {
                (SkipprDataType::Array, Some(SkipprDataType::String))
            }
        }
        V::Object(_) => (SkipprDataType::Record, None),
    }
}

pub fn apply_specs_to_namespace(
    namespace: &str,
    specs: &[EvolutionSpec],
    pm: &mut PipelineMetadata,
) {
    if let Some(ns_meta) = pm.metadata.get_mut(namespace) {
        for s in specs.iter() {
            let target: &mut HashMap<String, Metadata> = match &s.parent {
                Some(parent) => match ns_meta.fields.get_mut(parent) {
                    Some(m) => &mut m.fields,
                    None => continue,
                },
                None => &mut ns_meta.fields,
            };
            let evolved_name =
                if s.required_type == SkipprDataType::Array && s.values_type.is_some() {
                    format!("{}_array_{}", s.field, s.values_type.as_ref().unwrap())
                } else {
                    format!("{}_{}", s.field, s.required_type)
                };
            if !target.contains_key(&evolved_name) {
                let mut md = Metadata::new().unwrap();
                md.determined_type = if s.required_type == SkipprDataType::Array {
                    s.values_type.clone().unwrap_or(SkipprDataType::String)
                } else {
                    s.required_type.clone()
                };
                md.out_field_name = evolved_name.clone();
                md.enabled = true;
                target.insert(evolved_name, md);
            }
        }
    }
}

#[cfg(test)]
mod tests_apply_evolution_factory_recordish {
    use super::*;
    use serde_json::json;

    fn root_with_water_flowmeter() -> HashMap<String, Metadata> {
        // root namespace fields
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // water_flowmeter (original) string with evolution to record sibling
        let mut wf = Metadata::new().unwrap();
        wf.determined_type = SkipprDataType::String;
        wf.out_field_name = "water_flowmeter".to_string();
        let evo = Evolution {
            type_string: SkipprDataType::Record,
            new_field: "water_flowmeter_record".to_string(),
            sovled: true,
        };
        wf.evolution
            .insert("water_flowmeter_record".to_string(), evo);
        root.insert("water_flowmeter".to_string(), wf);

        // water_flowmeter_record exists with some children but missing 'is_fitted'
        let mut wfr = Metadata::new().unwrap();
        wfr.determined_type = SkipprDataType::Record;
        // pre-populate a couple of children
        let mut data_valid = Metadata::new().unwrap();
        data_valid.determined_type = SkipprDataType::Boolean;
        data_valid.out_field_name = "data_valid".to_string();
        wfr.fields.insert("data_valid".to_string(), data_valid);

        root.insert("water_flowmeter_record".to_string(), wfr);
        root
    }

    #[test]
    fn test_record_evolution_discovers_new_child() {
        let flatten = false;
        let mut root = root_with_water_flowmeter();

        // Incoming object adds 'is_fitted': true (previously unseen)
        let obj = json!({
            "is_fitted": true,
            "data_valid": false
        });

        // Apply evolution factory on parent field; should discover child and map value
        let res = Evolution::apply_evolution_factory("water_flowmeter", &obj, &mut root, flatten)
            .expect("evolution should succeed");

        // Evolved field should be the record sibling
        assert_eq!(res.field, "water_flowmeter_record");

        // Metadata now contains the new child under the record sibling
        let record_md = root
            .get("water_flowmeter_record")
            .expect("record md present");
        assert!(
            record_md.fields.get("is_fitted").is_some(),
            "is_fitted should be discovered"
        );
        assert_eq!(
            record_md.fields.get("is_fitted").unwrap().determined_type,
            SkipprDataType::Boolean
        );
    }

    #[test]
    fn test_child_type_change_evolves_sibling_under_record() {
        let flatten = false;
        // Parent record with child temperature_degc as string; evolve to double
        let mut parent_fields: HashMap<String, Metadata> = HashMap::new();
        let mut temp = Metadata::new().unwrap();
        temp.determined_type = SkipprDataType::String;
        // Add an evolution entry to double sibling
        let evo = Evolution {
            type_string: SkipprDataType::Double,
            new_field: "temperature_degc_double".to_string(),
            sovled: true,
        };
        temp.evolution
            .insert("temperature_degc_double".to_string(), evo);
        parent_fields.insert("temperature_degc".to_string(), temp);

        // Incoming numeric value
        let v = json!(12.34);

        let out =
            Evolution::apply_evolution_factory("temperature_degc", &v, &mut parent_fields, flatten)
                .expect("child evolution should succeed");

        // Should return the evolved sibling and numeric value
        assert_eq!(out.field, "temperature_degc_double");
        assert!(out.value.is_number());

        // Ensure evolved sibling exists in metadata
        assert!(parent_fields.get("temperature_degc_double").is_some());
        assert_eq!(
            parent_fields
                .get("temperature_degc_double")
                .unwrap()
                .determined_type,
            SkipprDataType::Double
        );
    }
}

#[cfg(test)]
mod tests_evolution_chained {
    use super::*;
    use crate::discover::LAST_SUCCESSFUL_EVOLUTION;
    use serde_json::json;

    fn md() -> Metadata {
        Metadata::new().unwrap()
    }

    #[test]
    fn test_primitive_chain_string_to_long_to_double_with_cached_miss() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // Field 'v' initially string with two sibling evolutions: long and double
        let mut v = md();
        v.determined_type = SkipprDataType::String;
        v.out_field_name = "v".to_string();
        v.evolution.insert(
            "v_long".to_string(),
            Evolution {
                type_string: SkipprDataType::Long,
                new_field: "v_long".to_string(),
                sovled: true,
            },
        );
        v.evolution.insert(
            "v_double".to_string(),
            Evolution {
                type_string: SkipprDataType::Double,
                new_field: "v_double".to_string(),
                sovled: true,
            },
        );
        root.insert("v".to_string(), v);

        // Seed cache to the wrong evolution to simulate cached miss
        LAST_SUCCESSFUL_EVOLUTION.with(|cell| {
            cell.borrow_mut()
                .insert("v".to_string(), "v_long".to_string())
        });

        // Provide a float -> should resolve to v_double
        let val = json!(12.34);
        let out = Evolution::apply_evolution_factory("v", &val, &mut root, flatten)
            .expect("evolution succeeds");
        assert_eq!(out.field, "v_double");
        assert!(out.value.is_number());
    }

    #[test]
    fn test_timestamp_chain_string_to_ts_to_ts_milli() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // 'ts' has two evolutions: timestamp, then timestamp_milli
        let mut ts = md();
        ts.determined_type = SkipprDataType::String;
        ts.out_field_name = "ts".to_string();
        ts.evolution.insert(
            "ts_timestamp".to_string(),
            Evolution {
                type_string: SkipprDataType::Timestamp,
                new_field: "ts_timestamp".to_string(),
                sovled: true,
            },
        );
        ts.evolution.insert(
            "ts_timestamp_milli".to_string(),
            Evolution {
                type_string: SkipprDataType::TimestampMilli,
                new_field: "ts_timestamp_milli".to_string(),
                sovled: true,
            },
        );
        root.insert("ts".to_string(), ts);

        let val = json!(1700000000123i64); // ms
        let out = Evolution::apply_evolution_factory("ts", &val, &mut root, flatten)
            .expect("evolution succeeds");
        assert_eq!(out.field, "ts_timestamp_milli");
        assert!(out.value.is_number());
    }

    #[test]
    fn test_record_then_child_evolves_again() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // sensor evolves to sensor_record
        let mut sensor = md();
        sensor.determined_type = SkipprDataType::String;
        sensor.out_field_name = "sensor".to_string();
        sensor.evolution.insert(
            "sensor_record".to_string(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "sensor_record".to_string(),
                sovled: true,
            },
        );
        root.insert("sensor".to_string(), sensor);

        // Prime record sibling with one child 'a' as string and evolution to 'a_double'
        let mut rec = md();
        rec.determined_type = SkipprDataType::Record;
        let mut a = md();
        a.determined_type = SkipprDataType::String;
        a.out_field_name = "a".to_string();
        a.evolution.insert(
            "a_double".to_string(),
            Evolution {
                type_string: SkipprDataType::Double,
                new_field: "a_double".to_string(),
                sovled: true,
            },
        );
        rec.fields.insert("a".to_string(), a);
        root.insert("sensor_record".to_string(), rec);

        // First apply parent evolution
        let obj = json!({ "a": 1.23 });
        let r = Evolution::apply_evolution_factory("sensor", &obj, &mut root, flatten)
            .expect("parent evolve ok");
        assert_eq!(r.field, "sensor_record");

        // Now evolve child under the record
        if let Some(rec_md) = root.get_mut("sensor_record") {
            let out = Evolution::apply_evolution_factory(
                "a",
                &json!(1.23),
                rec_md.fields.as_mut(),
                flatten,
            )
            .expect("child evolve ok");
            assert_eq!(out.field, "a_double");
            assert!(out.value.is_number());
        } else {
            panic!("missing record metadata");
        }
    }

    #[test]
    fn test_array_values_type_chain() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // arr has two evolutions: array(long) then array(double)
        let mut arr = md();
        arr.determined_type = SkipprDataType::String;
        arr.out_field_name = "arr".to_string();
        arr.evolution.insert(
            "arr_array_long".to_string(),
            Evolution {
                type_string: SkipprDataType::Array,
                new_field: "arr_array_long".to_string(),
                sovled: true,
            },
        );
        arr.evolution.insert(
            "arr_array_double".to_string(),
            Evolution {
                type_string: SkipprDataType::Array,
                new_field: "arr_array_double".to_string(),
                sovled: true,
            },
        );
        root.insert("arr".to_string(), arr);

        // Prime metadata for the target array values type so fast mapping can work without full discovery
        let mut arr_double = md();
        arr_double.determined_type = SkipprDataType::Array;
        arr_double.determined_type_values = Some(SkipprDataType::Double);
        arr_double.out_field_name = "arr_array_double".to_string();
        root.insert("arr_array_double".to_string(), arr_double);

        let val = json!([1.1, 2.2, 3.3]);
        let out = Evolution::apply_evolution_factory("arr", &val, &mut root, flatten)
            .expect("array evolve ok");
        assert_eq!(out.field, "arr_array_double");
        assert!(out.value.is_array());
    }

    #[test]
    fn test_map_values_type_chain() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // attrs evolves to map<string,double>
        let mut attrs = md();
        attrs.determined_type = SkipprDataType::String;
        attrs.out_field_name = "attrs".to_string();
        attrs.evolution.insert(
            "attrs_map".to_string(),
            Evolution {
                type_string: SkipprDataType::Map,
                new_field: "attrs_map".to_string(),
                sovled: true,
            },
        );
        attrs.evolution.insert(
            "attrs_map_double".to_string(),
            Evolution {
                type_string: SkipprDataType::Map,
                new_field: "attrs_map_double".to_string(),
                sovled: true,
            },
        );
        root.insert("attrs".to_string(), attrs);

        // Prime target map values type
        let mut map_double = md();
        map_double.determined_type = SkipprDataType::Map;
        map_double.determined_type_values = Some(SkipprDataType::Double);
        map_double.out_field_name = "attrs_map_double".to_string();
        root.insert("attrs_map_double".to_string(), map_double);

        let val = json!({"k1": 1.2, "k2": 3.4});
        let out = Evolution::apply_evolution_factory("attrs", &val, &mut root, flatten)
            .expect("map evolve ok");
        assert_eq!(out.field, "attrs_map_double");
        assert!(out.value.is_object());
    }
}

#[cfg(test)]
mod tests_evolution_more_types {
    use super::*;
    use serde_json::json;

    fn md() -> Metadata {
        Metadata::new().unwrap()
    }

    #[test]
    fn test_string_to_boolean_sibling() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.out_field_name = "flag".to_string();
        f.evolution.insert(
            "flag_bool".to_string(),
            Evolution {
                type_string: SkipprDataType::Boolean,
                new_field: "flag_bool".to_string(),
                sovled: true,
            },
        );
        root.insert("flag".to_string(), f);

        let val = json!("true");
        let out = Evolution::apply_evolution_factory("flag", &val, &mut root, flatten)
            .expect("bool evolve ok");
        assert_eq!(out.field, "flag_bool");
        assert!(out.value.is_boolean());
    }

    #[test]
    fn test_string_to_date_sibling() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut d = md();
        d.determined_type = SkipprDataType::String;
        d.out_field_name = "d".to_string();
        d.evolution.insert(
            "d_date".to_string(),
            Evolution {
                type_string: SkipprDataType::Date,
                new_field: "d_date".to_string(),
                sovled: true,
            },
        );
        root.insert("d".to_string(), d);

        let val = json!("2024-01-02");
        let out = Evolution::apply_evolution_factory("d", &val, &mut root, flatten)
            .expect("date evolve ok");
        assert_eq!(out.field, "d_date");
    }

    #[test]
    fn test_timestamp_prefers_seconds_when_small() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut ts = md();
        ts.determined_type = SkipprDataType::Long;
        ts.out_field_name = "ts".to_string();
        ts.evolution.insert(
            "ts_timestamp".to_string(),
            Evolution {
                type_string: SkipprDataType::Timestamp,
                new_field: "ts_timestamp".to_string(),
                sovled: true,
            },
        );
        ts.evolution.insert(
            "ts_timestamp_milli".to_string(),
            Evolution {
                type_string: SkipprDataType::TimestampMilli,
                new_field: "ts_timestamp_milli".to_string(),
                sovled: true,
            },
        );
        root.insert("ts".to_string(), ts);

        let val = json!(1_700_000_000i64); // seconds-ish
        let out = Evolution::apply_evolution_factory("ts", &val, &mut root, flatten)
            .expect("ts evolve ok");
        assert_eq!(out.field, "ts_timestamp");
    }

    #[test]
    fn test_array_string_to_array_long() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut arr = md();
        arr.determined_type = SkipprDataType::String;
        arr.out_field_name = "arr".to_string();
        arr.evolution.insert(
            "arr_array_long".to_string(),
            Evolution {
                type_string: SkipprDataType::Array,
                new_field: "arr_array_long".to_string(),
                sovled: true,
            },
        );
        root.insert("arr".to_string(), arr);

        // Prime target array with values type = long so fast mapping can succeed
        let mut target = md();
        target.determined_type = SkipprDataType::Array;
        target.determined_type_values = Some(SkipprDataType::Long);
        target.out_field_name = "arr_array_long".to_string();
        root.insert("arr_array_long".to_string(), target);

        let val = json!(["1", "2", "3"]);
        let out = Evolution::apply_evolution_factory("arr", &val, &mut root, flatten)
            .expect("array long evolve ok");
        assert_eq!(out.field, "arr_array_long");
        assert!(out.value.is_array());
    }

    #[test]
    fn test_record_child_string_to_long() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // parent record exists with child n: string -> n_long
        let mut rec = md();
        rec.determined_type = SkipprDataType::Record;
        let mut n = md();
        n.determined_type = SkipprDataType::String;
        n.out_field_name = "n".to_string();
        n.evolution.insert(
            "n_long".to_string(),
            Evolution {
                type_string: SkipprDataType::Long,
                new_field: "n_long".to_string(),
                sovled: true,
            },
        );
        rec.fields.insert("n".to_string(), n);
        root.insert("rec".to_string(), rec);

        // value for child
        if let Some(rec_md) = root.get_mut("rec") {
            let out = Evolution::apply_evolution_factory(
                "n",
                &json!("1234"),
                rec_md.fields.as_mut(),
                flatten,
            )
            .expect("child cast ok");
            assert_eq!(out.field, "n_long");
            assert!(out.value.is_number());
        } else {
            panic!("missing rec");
        }
    }
}

#[cfg(test)]
mod tests_evolve_field {
    use super::*;
    use serde_json::{Map, Value};
    use std::collections::HashMap;

    fn setup_metadata() -> HashMap<String, Metadata> {
        let mut metadata = HashMap::new();
        let mut md = Metadata::new().unwrap();
        md.determined_type = SkipprDataType::String;
        md.out_field_name = "test_field".to_string();
        md.enabled = true;
        metadata.insert("test_field".to_string(), md);
        metadata
    }

    #[test]
    fn test_evolve_field_long() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::String("12356789101112".to_string());
        let flatten = false;
        let mut updated_schema = "no".to_string();

        let result = Evolution::evolve_field(
            &field,
            &value,
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            flatten,
        );

        let expected_data_type = SkipprDataType::Long;
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(
            metadata
                .get(&field)
                .unwrap()
                .evolution
                .get(&expected_new_field)
                .unwrap()
                .new_field,
            expected_new_field
        );
        // Assert the new evolved fields Metadata
        assert_eq!(
            metadata.get(&expected_new_field).unwrap().determined_type,
            expected_data_type
        );
        // Assert old field is unchanged
        assert_eq!(
            metadata.get(&field).unwrap().determined_type,
            SkipprDataType::String
        );
    }

    #[test]
    fn test_evolve_field_integer() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Number(serde_json::Number::from(10));
        let flatten = false;
        let mut updated_schema = "no".to_string();

        let result = Evolution::evolve_field(
            &field,
            &value,
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            flatten,
        );

        // The expected type should be integer, not timestamp with our more conservative timestamp detection
        let expected_data_type = SkipprDataType::Integer;
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(
            metadata
                .get(&field)
                .unwrap()
                .evolution
                .get(&expected_new_field)
                .unwrap()
                .new_field,
            expected_new_field
        );
        // Assert the new evolved fields Metadata
        assert_eq!(
            metadata.get(&expected_new_field).unwrap().determined_type,
            expected_data_type
        );
        // Assert old field is unchanged
        assert_eq!(
            metadata.get(&field).unwrap().determined_type,
            SkipprDataType::String
        );
    }

    #[test]
    fn test_evolve_field_boolean() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Bool(true);
        let flatten = false;

        let mut updated_schema = "no".to_string();

        let result = Evolution::evolve_field(
            &field,
            &value,
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            flatten,
        );

        let expected_data_type = SkipprDataType::Boolean;
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(
            metadata
                .get(&field)
                .unwrap()
                .evolution
                .get(&expected_new_field)
                .unwrap()
                .new_field,
            expected_new_field
        );
        // Assert the new evolved fields Metadata
        assert_eq!(
            metadata.get(&expected_new_field).unwrap().determined_type,
            expected_data_type
        );
        // Assert old field is unchanged
        assert_eq!(
            metadata.get(&field).unwrap().determined_type,
            SkipprDataType::String
        );
    }

    #[test]
    fn test_evolve_field_null() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Null;
        let flatten = false;
        let mut updated_schema = "no".to_string();

        let result = Evolution::evolve_field(
            &field,
            &value,
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            flatten,
        );

        let expected_data_type = SkipprDataType::String;
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(
            metadata
                .get(&field)
                .unwrap()
                .evolution
                .get(&expected_new_field)
                .unwrap()
                .new_field,
            expected_new_field
        );
        // The field is actually created, so check that it exists
        assert!(metadata.get(&expected_new_field).is_some());
        // Assert old field is unchanged
        assert_eq!(
            metadata.get(&field).unwrap().determined_type,
            SkipprDataType::String
        );
    }

    #[test]
    fn test_evolve_field_array() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Array(vec![Value::String("test".to_string())]);
        let flatten = false;

        let mut updated_schema = "no".to_string();

        let result = Evolution::evolve_field(
            &field,
            &value,
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            flatten,
        );

        // First we create a temporary field with "_unknown" suffix
        let _temp_field_name = format!("{}_{}", &field, "unknown");
        // Then the final field is derived as field + "_array_" + element data type (string)
        let expected_new_field = format!("{}_array_{}", &field, "string");

        assert!(result.is_ok());
        // Assert the Evolution - the key in the map is "array", not the new field name
        assert_eq!(
            metadata
                .get(&field)
                .unwrap()
                .evolution
                .get("array")
                .unwrap()
                .new_field,
            expected_new_field
        );
        // Assert the new evolved fields Metadata exists with the correct type
        assert!(metadata.get(&expected_new_field).is_some());
        // Assert old field is unchanged
        assert_eq!(
            metadata.get(&field).unwrap().determined_type,
            SkipprDataType::String
        );
    }

    #[test]
    fn test_evolve_field_map() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let mut value = Value::Object(serde_json::Map::new());
        let flatten = false;
        // Create a simple map with consistent value types
        value
            .as_object_mut()
            .unwrap()
            .insert("test".to_string(), Value::String("test".to_string()));
        value
            .as_object_mut()
            .unwrap()
            .insert("test2".to_string(), Value::String("test2".to_string()));
        let mut updated_schema = "no".to_string();

        let result = Evolution::evolve_field(
            &field,
            &value,
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            flatten,
        );

        assert!(result.is_ok());

        // Get the actual evolution that was created
        let field_metadata = metadata.get(&field).unwrap();
        assert_eq!(
            field_metadata.evolution.len(),
            1,
            "Should have exactly one evolution"
        );

        // Get the first (and only) evolution
        let (_, evolution) = field_metadata.evolution.iter().next().unwrap();

        // Assert the evolution points to a record type
        assert_eq!(evolution.type_string, SkipprDataType::Record);

        // Get the evolved field's metadata
        let evolved_field_metadata = metadata.get(&evolution.new_field).unwrap();
        assert_eq!(
            evolved_field_metadata.determined_type,
            SkipprDataType::Record
        );

        // Assert old field is unchanged
        assert_eq!(
            metadata.get(&field).unwrap().determined_type,
            SkipprDataType::String
        );
    }

    #[test]
    fn test_evolve_field_record() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let mut complex_struct = Map::new();
        complex_struct.insert("test".to_string(), Value::String("test".to_string()));
        complex_struct.insert("test2".to_string(), Value::Number(123.into()));
        complex_struct.insert(
            "test2".to_string(),
            Value::Array(vec![Value::String("test".to_string())]),
        );
        let value = Value::Object(complex_struct);
        let flatten = false;
        let mut updated_schema = "no".to_string();

        let result = Evolution::evolve_field(
            &field,
            &value,
            None,
            None,
            &mut metadata,
            &mut updated_schema,
            flatten,
        );

        let expected_data_type = SkipprDataType::Record;
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(
            metadata
                .get(&field)
                .unwrap()
                .evolution
                .get(&expected_new_field)
                .unwrap()
                .new_field,
            expected_new_field
        );
        // Assert the new evolved fields Metadata
        assert_eq!(
            metadata.get(&expected_new_field).unwrap().determined_type,
            expected_data_type
        );
        // Assert old field is unchanged
        assert_eq!(
            metadata.get(&field).unwrap().determined_type,
            SkipprDataType::String
        );
    }

    // #[test]
    // fn test_evolve_field_no_metadata() {
    //     let mut metadata = HashMap::new();
    //     let field = "nonexistent_field".to_string();
    //     let value = Value::String("hello".to_string());
    //
    //     let result = Evolution::evolve_field(&field, &value, &mut metadata);
    //
    //     assert!(result.is_err());
    // }

    // Add other edge cases and scenarios as needed.
}

/// Exhaustive evolution type matrix.
///
/// For every (source_type, target_type) pair, verify that `apply_evolution_factory`
/// either routes correctly or fails gracefully (no panics).
#[cfg(test)]
mod tests_evolution_type_matrix {
    use super::*;
    use serde_json::json;

    fn md() -> Metadata {
        Metadata::new().unwrap()
    }

    fn setup_field(
        name: &str,
        source_type: SkipprDataType,
        evo_type: SkipprDataType,
    ) -> HashMap<String, Metadata> {
        let evo_name = format!("{}_{}", name, evo_type);
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = source_type;
        f.out_field_name = name.to_string();
        f.evolution.insert(
            evo_name.clone(),
            Evolution {
                type_string: evo_type,
                new_field: evo_name,
                sovled: true,
            },
        );
        root.insert(name.to_string(), f);
        root
    }

    fn assert_evolves_ok(source: SkipprDataType, target: SkipprDataType, value: serde_json::Value) {
        let target_str = target.as_str();
        let mut root = setup_field("f", source.clone(), target);
        let expected_field = format!("f_{}", target_str);
        let res = Evolution::apply_evolution_factory("f", &value, &mut root, false);
        assert!(
            res.is_ok(),
            "evolution {:?} -> {} should succeed for {:?}, got {:?}",
            source,
            target_str,
            value,
            res.err()
        );
        assert_eq!(res.unwrap().field, expected_field);
    }

    fn assert_no_panic(source: SkipprDataType, target: SkipprDataType, value: serde_json::Value) {
        let mut root = setup_field("f", source, target);
        let _ = Evolution::apply_evolution_factory("f", &value, &mut root, false);
    }

    // ── string → X ──────────────────────────────────────────

    #[test]
    fn string_to_long() {
        assert_evolves_ok(SkipprDataType::String, SkipprDataType::Long, json!(42i64));
    }

    #[test]
    fn string_to_integer() {
        assert_evolves_ok(SkipprDataType::String, SkipprDataType::Integer, json!(42));
    }

    #[test]
    fn string_to_double() {
        assert_evolves_ok(SkipprDataType::String, SkipprDataType::Double, json!(3.15));
    }

    #[test]
    fn string_to_boolean() {
        assert_evolves_ok(SkipprDataType::String, SkipprDataType::Boolean, json!(true));
    }

    #[test]
    fn string_to_record() {
        assert_evolves_ok(
            SkipprDataType::String,
            SkipprDataType::Record,
            json!({"a": 1}),
        );
    }

    #[test]
    fn string_to_timestamp() {
        assert_evolves_ok(
            SkipprDataType::String,
            SkipprDataType::Timestamp,
            json!(1700000000i64),
        );
    }

    #[test]
    fn string_to_timestamp_milli() {
        assert_evolves_ok(
            SkipprDataType::String,
            SkipprDataType::TimestampMilli,
            json!(1700000000123i64),
        );
    }

    #[test]
    fn string_to_map() {
        let mut root = setup_field("f", SkipprDataType::String, SkipprDataType::Map);
        let mut map_md = md();
        map_md.determined_type = SkipprDataType::Map;
        map_md.determined_type_values = Some(SkipprDataType::String);
        root.insert("f_map".to_string(), map_md);
        let res = Evolution::apply_evolution_factory("f", &json!({"k": "v"}), &mut root, false);
        assert!(res.is_ok());
    }

    // ── integer → X ─────────────────────────────────────────

    #[test]
    fn integer_to_long() {
        assert_evolves_ok(
            SkipprDataType::Integer,
            SkipprDataType::Long,
            json!(2_147_483_648i64),
        );
    }

    #[test]
    fn integer_to_double() {
        assert_evolves_ok(SkipprDataType::Integer, SkipprDataType::Double, json!(3.15));
    }

    #[test]
    fn integer_to_string() {
        assert_no_panic(
            SkipprDataType::Integer,
            SkipprDataType::String,
            json!("hello"),
        );
    }

    #[test]
    fn integer_to_boolean() {
        assert_no_panic(
            SkipprDataType::Integer,
            SkipprDataType::Boolean,
            json!(true),
        );
    }

    #[test]
    fn integer_to_timestamp() {
        assert_evolves_ok(
            SkipprDataType::Integer,
            SkipprDataType::Timestamp,
            json!(1700000000i64),
        );
    }

    #[test]
    fn integer_to_timestamp_milli() {
        assert_evolves_ok(
            SkipprDataType::Integer,
            SkipprDataType::TimestampMilli,
            json!(1700000000123i64),
        );
    }

    #[test]
    fn integer_to_record() {
        assert_no_panic(
            SkipprDataType::Integer,
            SkipprDataType::Record,
            json!({"a": 1}),
        );
    }

    // ── long → X ────────────────────────────────────────────

    #[test]
    fn long_to_double() {
        assert_evolves_ok(SkipprDataType::Long, SkipprDataType::Double, json!(3.15));
    }

    #[test]
    fn long_to_string() {
        assert_no_panic(SkipprDataType::Long, SkipprDataType::String, json!("hello"));
    }

    #[test]
    fn long_to_boolean() {
        assert_no_panic(SkipprDataType::Long, SkipprDataType::Boolean, json!(false));
    }

    #[test]
    fn long_to_timestamp() {
        assert_evolves_ok(
            SkipprDataType::Long,
            SkipprDataType::Timestamp,
            json!(1700000000i64),
        );
    }

    #[test]
    fn long_to_timestamp_milli() {
        assert_evolves_ok(
            SkipprDataType::Long,
            SkipprDataType::TimestampMilli,
            json!(1700000000123i64),
        );
    }

    #[test]
    fn long_to_integer() {
        assert_evolves_ok(SkipprDataType::Long, SkipprDataType::Integer, json!(42));
    }

    // ── double → X ──────────────────────────────────────────

    #[test]
    fn double_to_string() {
        assert_no_panic(
            SkipprDataType::Double,
            SkipprDataType::String,
            json!("hello"),
        );
    }

    #[test]
    fn double_to_long() {
        assert_evolves_ok(SkipprDataType::Double, SkipprDataType::Long, json!(42i64));
    }

    #[test]
    fn double_to_boolean() {
        assert_no_panic(SkipprDataType::Double, SkipprDataType::Boolean, json!(true));
    }

    #[test]
    fn double_to_integer() {
        assert_evolves_ok(SkipprDataType::Double, SkipprDataType::Integer, json!(42));
    }

    #[test]
    fn double_to_timestamp() {
        assert_evolves_ok(
            SkipprDataType::Double,
            SkipprDataType::Timestamp,
            json!(1700000000i64),
        );
    }

    // ── boolean → X ─────────────────────────────────────────

    #[test]
    fn boolean_to_string() {
        assert_no_panic(
            SkipprDataType::Boolean,
            SkipprDataType::String,
            json!("hello"),
        );
    }

    #[test]
    fn boolean_to_integer() {
        assert_evolves_ok(SkipprDataType::Boolean, SkipprDataType::Integer, json!(1));
    }

    #[test]
    fn boolean_to_long() {
        assert_evolves_ok(SkipprDataType::Boolean, SkipprDataType::Long, json!(1i64));
    }

    #[test]
    fn boolean_to_double() {
        assert_evolves_ok(SkipprDataType::Boolean, SkipprDataType::Double, json!(3.15));
    }

    // ── record → X ──────────────────────────────────────────

    #[test]
    fn record_to_string() {
        assert_no_panic(
            SkipprDataType::Record,
            SkipprDataType::String,
            json!("fallback"),
        );
    }

    #[test]
    fn record_to_long() {
        assert_no_panic(SkipprDataType::Record, SkipprDataType::Long, json!(42i64));
    }

    // ── timestamp → X ───────────────────────────────────────

    #[test]
    fn timestamp_to_string() {
        assert_no_panic(
            SkipprDataType::Timestamp,
            SkipprDataType::String,
            json!("hello"),
        );
    }

    #[test]
    fn timestamp_to_long() {
        assert_evolves_ok(
            SkipprDataType::Timestamp,
            SkipprDataType::Long,
            json!(42i64),
        );
    }

    #[test]
    fn timestamp_to_double() {
        assert_evolves_ok(
            SkipprDataType::Timestamp,
            SkipprDataType::Double,
            json!(3.15),
        );
    }

    #[test]
    fn timestamp_to_timestamp_milli() {
        assert_evolves_ok(
            SkipprDataType::Timestamp,
            SkipprDataType::TimestampMilli,
            json!(1700000000123i64),
        );
    }

    // ── null → X ────────────────────────────────────────────

    #[test]
    fn null_to_string() {
        assert_no_panic(SkipprDataType::Null, SkipprDataType::String, json!("hello"));
    }

    #[test]
    fn null_to_long() {
        assert_no_panic(SkipprDataType::Null, SkipprDataType::Long, json!(42i64));
    }

    #[test]
    fn null_to_boolean() {
        assert_no_panic(SkipprDataType::Null, SkipprDataType::Boolean, json!(true));
    }

    #[test]
    fn null_to_record() {
        assert_no_panic(
            SkipprDataType::Null,
            SkipprDataType::Record,
            json!({"a": 1}),
        );
    }

    // ── edge cases ──────────────────────────────────────────

    #[test]
    fn null_value_falls_through() {
        assert_no_panic(SkipprDataType::Integer, SkipprDataType::String, json!(null));
    }

    #[test]
    fn empty_object_evolves_to_record() {
        assert_evolves_ok(SkipprDataType::String, SkipprDataType::Record, json!({}));
    }

    #[test]
    fn empty_array_evolves_to_array() {
        let mut root = setup_field("f", SkipprDataType::String, SkipprDataType::Array);
        let mut arr_md = md();
        arr_md.determined_type = SkipprDataType::Array;
        arr_md.determined_type_values = Some(SkipprDataType::String);
        root.insert("f_array".to_string(), arr_md);
        let res = Evolution::apply_evolution_factory("f", &json!([]), &mut root, false);
        assert!(res.is_ok(), "empty array evolution should succeed");
    }

    #[test]
    fn mixed_type_array_evolves() {
        let mut root = setup_field("f", SkipprDataType::String, SkipprDataType::Array);
        let mut arr_md = md();
        arr_md.determined_type = SkipprDataType::Array;
        arr_md.determined_type_values = Some(SkipprDataType::String);
        root.insert("f_array".to_string(), arr_md);
        let val = json!([1, "two", true, null, 3.15]);
        let res = Evolution::apply_evolution_factory("f", &val, &mut root, false);
        assert!(res.is_ok(), "mixed array should not panic");
    }

    #[test]
    fn evolution_with_flatten_enabled() {
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.out_field_name = "data".to_string();
        f.evolution.insert(
            "data_record".to_string(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "data_record".to_string(),
                sovled: true,
            },
        );
        root.insert("data".to_string(), f);
        let res = Evolution::apply_evolution_factory("data", &json!({"x": 1}), &mut root, true);
        assert!(res.is_ok(), "flatten=true should work");
    }

    #[test]
    fn unicode_field_name_evolves() {
        let mut root = setup_field("café", SkipprDataType::String, SkipprDataType::Long);
        let res = Evolution::apply_evolution_factory("café", &json!(42), &mut root, false);
        assert!(res.is_ok(), "unicode field name should evolve");
        assert_eq!(res.unwrap().field, "café_long");
    }
}

/// Depth matrix: evolution at nesting levels 1-5.
#[cfg(test)]
mod tests_evolution_depth_matrix {
    use super::*;
    use serde_json::json;

    fn md() -> Metadata {
        Metadata::new().unwrap()
    }

    fn make_record_with_child_evolution(
        child_name: &str,
        child_source: SkipprDataType,
        child_target: SkipprDataType,
    ) -> Metadata {
        let evo_name = format!("{}_{}", child_name, child_target);
        let mut rec = md();
        rec.determined_type = SkipprDataType::Record;
        let mut child = md();
        child.determined_type = child_source;
        child.out_field_name = child_name.to_string();
        child.evolution.insert(
            evo_name.clone(),
            Evolution {
                type_string: child_target,
                new_field: evo_name,
                sovled: true,
            },
        );
        rec.fields.insert(child_name.to_string(), child);
        rec
    }

    fn evolve_at_depth(
        root: &mut HashMap<String, Metadata>,
        path: &[&str],
        value: &serde_json::Value,
    ) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {
        if path.len() == 1 {
            return Evolution::apply_evolution_factory(path[0], value, root, false);
        }
        let first = path[0];
        let rec_name = format!("{}_record", first);
        if let Some(rec_md) = root.get_mut(&rec_name) {
            evolve_at_depth(&mut rec_md.fields, &path[1..], value)
        } else if let Some(rec_md) = root.get_mut(first) {
            evolve_at_depth(&mut rec_md.fields, &path[1..], value)
        } else {
            Err(format!("path segment '{}' not found", first).into())
        }
    }

    #[test]
    fn depth_1_flat_evolution() {
        let mut root = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.evolution.insert(
            "leaf_long".to_string(),
            Evolution {
                type_string: SkipprDataType::Long,
                new_field: "leaf_long".to_string(),
                sovled: true,
            },
        );
        root.insert("leaf".to_string(), f);
        let r = Evolution::apply_evolution_factory("leaf", &json!(42i64), &mut root, false);
        assert!(r.is_ok());
        assert_eq!(r.unwrap().field, "leaf_long");
    }

    #[test]
    fn depth_2_nested_evolution() {
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut outer = md();
        outer.determined_type = SkipprDataType::String;
        outer.evolution.insert(
            "outer_record".to_string(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "outer_record".to_string(),
                sovled: true,
            },
        );
        root.insert("outer".to_string(), outer);
        root.insert(
            "outer_record".to_string(),
            make_record_with_child_evolution("leaf", SkipprDataType::String, SkipprDataType::Long),
        );

        let r0 =
            Evolution::apply_evolution_factory("outer", &json!({"leaf": 42}), &mut root, false)
                .unwrap();
        assert_eq!(r0.field, "outer_record");

        let r1 = evolve_at_depth(&mut root, &["outer_record", "leaf"], &json!(42i64)).unwrap();
        assert_eq!(r1.field, "leaf_long");
    }

    #[test]
    fn depth_3_evolution() {
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut a = md();
        a.determined_type = SkipprDataType::String;
        a.evolution.insert(
            "a_record".to_string(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "a_record".to_string(),
                sovled: true,
            },
        );
        root.insert("a".to_string(), a);

        let mut a_rec = md();
        a_rec.determined_type = SkipprDataType::Record;
        let mut b = md();
        b.determined_type = SkipprDataType::String;
        b.evolution.insert(
            "b_record".to_string(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "b_record".to_string(),
                sovled: true,
            },
        );
        a_rec.fields.insert("b".to_string(), b);
        a_rec.fields.insert(
            "b_record".to_string(),
            make_record_with_child_evolution("c", SkipprDataType::String, SkipprDataType::Double),
        );
        root.insert("a_record".to_string(), a_rec);

        let r0 =
            Evolution::apply_evolution_factory("a", &json!({"b": {"c": 1.5}}), &mut root, false)
                .unwrap();
        assert_eq!(r0.field, "a_record");

        let r1 = evolve_at_depth(&mut root, &["a_record", "b"], &json!({"c": 1.5})).unwrap();
        assert_eq!(r1.field, "b_record");

        let r2 = evolve_at_depth(&mut root, &["a_record", "b_record", "c"], &json!(1.5)).unwrap();
        assert_eq!(r2.field, "c_double");
    }

    #[test]
    fn depth_4_evolution() {
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut d0 = md();
        d0.determined_type = SkipprDataType::String;
        d0.evolution.insert(
            "d0_record".into(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "d0_record".into(),
                sovled: true,
            },
        );
        root.insert("d0".into(), d0);

        let mut d0_rec = md();
        d0_rec.determined_type = SkipprDataType::Record;
        let mut d1 = md();
        d1.determined_type = SkipprDataType::String;
        d1.evolution.insert(
            "d1_record".into(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "d1_record".into(),
                sovled: true,
            },
        );
        d0_rec.fields.insert("d1".into(), d1);

        let mut d1_rec = md();
        d1_rec.determined_type = SkipprDataType::Record;
        let mut d2 = md();
        d2.determined_type = SkipprDataType::String;
        d2.evolution.insert(
            "d2_record".into(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "d2_record".into(),
                sovled: true,
            },
        );
        d1_rec.fields.insert("d2".into(), d2);
        d1_rec.fields.insert(
            "d2_record".into(),
            make_record_with_child_evolution("leaf", SkipprDataType::String, SkipprDataType::Long),
        );
        d0_rec.fields.insert("d1_record".into(), d1_rec);
        root.insert("d0_record".into(), d0_rec);

        let val = json!({"d1": {"d2": {"leaf": 99}}});
        let r0 = Evolution::apply_evolution_factory("d0", &val, &mut root, false).unwrap();
        assert_eq!(r0.field, "d0_record");

        let r3 = evolve_at_depth(
            &mut root,
            &["d0_record", "d1_record", "d2_record", "leaf"],
            &json!(99i64),
        )
        .unwrap();
        assert_eq!(r3.field, "leaf_long");
    }

    #[test]
    fn depth_5_evolution() {
        // l0 -> l0_record { l1 -> l1_record { l2 -> l2_record { l3 -> l3_record { leaf -> leaf_double } } } }
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut l0 = md();
        l0.determined_type = SkipprDataType::String;
        l0.evolution.insert(
            "l0_record".into(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "l0_record".into(),
                sovled: true,
            },
        );
        root.insert("l0".into(), l0);

        // l3_record: contains leaf
        let mut l3_rec = md();
        l3_rec.determined_type = SkipprDataType::Record;
        let mut leaf = md();
        leaf.determined_type = SkipprDataType::String;
        leaf.out_field_name = "leaf".into();
        leaf.evolution.insert(
            "leaf_double".into(),
            Evolution {
                type_string: SkipprDataType::Double,
                new_field: "leaf_double".into(),
                sovled: true,
            },
        );
        l3_rec.fields.insert("leaf".into(), leaf);

        // l2_record: contains l3 -> l3_record
        let mut l2_rec = md();
        l2_rec.determined_type = SkipprDataType::Record;
        let mut l3 = md();
        l3.determined_type = SkipprDataType::String;
        l3.evolution.insert(
            "l3_record".into(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "l3_record".into(),
                sovled: true,
            },
        );
        l2_rec.fields.insert("l3".into(), l3);
        l2_rec.fields.insert("l3_record".into(), l3_rec);

        // l1_record: contains l2 -> l2_record
        let mut l1_rec = md();
        l1_rec.determined_type = SkipprDataType::Record;
        let mut l2 = md();
        l2.determined_type = SkipprDataType::String;
        l2.evolution.insert(
            "l2_record".into(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "l2_record".into(),
                sovled: true,
            },
        );
        l1_rec.fields.insert("l2".into(), l2);
        l1_rec.fields.insert("l2_record".into(), l2_rec);

        // l0_record: contains l1 -> l1_record
        let mut l0_rec = md();
        l0_rec.determined_type = SkipprDataType::Record;
        let mut l1 = md();
        l1.determined_type = SkipprDataType::String;
        l1.evolution.insert(
            "l1_record".into(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "l1_record".into(),
                sovled: true,
            },
        );
        l0_rec.fields.insert("l1".into(), l1);
        l0_rec.fields.insert("l1_record".into(), l1_rec);

        root.insert("l0_record".into(), l0_rec);

        let val = json!({"l1": {"l2": {"l3": {"leaf": 3.15}}}});
        let r0 = Evolution::apply_evolution_factory("l0", &val, &mut root, false).unwrap();
        assert_eq!(r0.field, "l0_record");

        let r_leaf = evolve_at_depth(
            &mut root,
            &["l0_record", "l1_record", "l2_record", "l3_record", "leaf"],
            &json!(3.15),
        )
        .unwrap();
        assert_eq!(r_leaf.field, "leaf_double");
    }

    #[test]
    fn flatten_enabled_at_depth_2() {
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut outer = md();
        outer.determined_type = SkipprDataType::String;
        outer.evolution.insert(
            "outer_record".into(),
            Evolution {
                type_string: SkipprDataType::Record,
                new_field: "outer_record".into(),
                sovled: true,
            },
        );
        root.insert("outer".into(), outer);
        root.insert(
            "outer_record".into(),
            make_record_with_child_evolution("inner", SkipprDataType::String, SkipprDataType::Long),
        );

        let res =
            Evolution::apply_evolution_factory("outer", &json!({"inner": 42}), &mut root, true);
        assert!(res.is_ok(), "flatten=true at depth 2 should work");
    }

    #[test]
    fn all_scalar_types_evolve_at_depth_2() {
        let targets = [
            (SkipprDataType::Long, json!(42i64)),
            (SkipprDataType::Integer, json!(42)),
            (SkipprDataType::Double, json!(3.15)),
            (SkipprDataType::Boolean, json!(true)),
            (SkipprDataType::Timestamp, json!(1700000000i64)),
            (SkipprDataType::TimestampMilli, json!(1700000000123i64)),
        ];

        for (target, value) in targets {
            let mut root: HashMap<String, Metadata> = HashMap::new();
            let mut outer = md();
            outer.determined_type = SkipprDataType::String;
            outer.evolution.insert(
                "outer_record".into(),
                Evolution {
                    type_string: SkipprDataType::Record,
                    new_field: "outer_record".into(),
                    sovled: true,
                },
            );
            root.insert("outer".into(), outer);
            root.insert(
                "outer_record".into(),
                make_record_with_child_evolution("leaf", SkipprDataType::String, target.clone()),
            );

            let r0 = Evolution::apply_evolution_factory(
                "outer",
                &json!({"leaf": value.clone()}),
                &mut root,
                false,
            );
            assert!(r0.is_ok(), "parent evolve for target {:?}", target);

            let r1 = evolve_at_depth(&mut root, &["outer_record", "leaf"], &value);
            assert!(
                r1.is_ok(),
                "depth 2 evolution to {:?} should succeed",
                target
            );
            let expected = format!("leaf_{}", target);
            assert_eq!(r1.unwrap().field, expected);
        }
    }
}

/// Property-based fuzz tests for evolution.
#[cfg(test)]
mod tests_evolution_proptest {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn md() -> Metadata {
        Metadata::new().unwrap()
    }

    fn arb_skippr_type() -> impl Strategy<Value = SkipprDataType> {
        prop_oneof![
            Just(SkipprDataType::String),
            Just(SkipprDataType::Integer),
            Just(SkipprDataType::Long),
            Just(SkipprDataType::Double),
            Just(SkipprDataType::Boolean),
            Just(SkipprDataType::Record),
            Just(SkipprDataType::Timestamp),
            Just(SkipprDataType::TimestampMilli),
        ]
    }

    fn arb_json_value() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            Just(json!(null)),
            any::<bool>().prop_map(|b| json!(b)),
            any::<i32>().prop_map(|i| json!(i)),
            any::<i64>().prop_map(|i| json!(i)),
            (-1e15f64..1e15f64).prop_map(|f| json!(f)),
            "[a-zA-Z0-9_]{0,20}".prop_map(|s| json!(s)),
            Just(json!({})),
            Just(json!({"a": 1})),
            Just(json!([])),
            Just(json!([1, 2, 3])),
            Just(json!([{"x": 1}])),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn apply_evolution_factory_never_panics(
            source_type in arb_skippr_type(),
            evo_type in arb_skippr_type(),
            value in arb_json_value(),
        ) {
            let evo_name = format!("f_{}", evo_type);
            let mut root: HashMap<String, Metadata> = HashMap::new();
            let mut f = md();
            f.determined_type = source_type;
            f.out_field_name = "f".to_string();
            f.evolution.insert(
                evo_name.clone(),
                Evolution {
                    type_string: evo_type,
                    new_field: evo_name,
                    sovled: true,
                },
            );
            root.insert("f".to_string(), f);
            let _ = Evolution::apply_evolution_factory("f", &value, &mut root, false);
        }

        #[test]
        fn evolve_field_never_panics(
            value in arb_json_value(),
        ) {
            let mut metadata: HashMap<String, Metadata> = HashMap::new();
            let mut f = md();
            f.determined_type = SkipprDataType::String;
            f.out_field_name = "test_field".to_string();
            metadata.insert("test_field".to_string(), f);

            let mut updated = "no".to_string();
            let _ = Evolution::evolve_field(
                &"test_field".to_string(),
                &value,
                None,
                None,
                &mut metadata,
                &mut updated,
                false,
            );
        }

        #[test]
        fn evolve_field_with_flatten_never_panics(
            value in arb_json_value(),
        ) {
            let mut metadata: HashMap<String, Metadata> = HashMap::new();
            let mut f = md();
            f.determined_type = SkipprDataType::String;
            f.out_field_name = "test_field".to_string();
            metadata.insert("test_field".to_string(), f);

            let mut updated = "no".to_string();
            let _ = Evolution::evolve_field(
                &"test_field".to_string(),
                &value,
                None,
                None,
                &mut metadata,
                &mut updated,
                true,
            );
        }

        #[test]
        fn nested_object_evolution_never_panics(
            key1 in "[a-z]{1,5}",
            key2 in "[a-z]{1,5}",
            leaf in arb_json_value(),
        ) {
            let nested = json!({ key1.clone(): { key2: leaf } });
            let mut metadata: HashMap<String, Metadata> = HashMap::new();
            let mut f = md();
            f.determined_type = SkipprDataType::String;
            f.out_field_name = "n".to_string();
            metadata.insert("n".to_string(), f);

            let mut updated = "no".to_string();
            let _ = Evolution::evolve_field(
                &"n".to_string(),
                &nested,
                None,
                None,
                &mut metadata,
                &mut updated,
                false,
            );
        }
    }
}

/// Edge case and data-loss regression tests.
#[cfg(test)]
mod tests_evolution_edge_cases {
    use super::*;
    use serde_json::json;

    fn md() -> Metadata {
        Metadata::new().unwrap()
    }

    #[test]
    fn field_name_collision_with_evolution_sibling() {
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.out_field_name = "foo".to_string();
        f.evolution.insert(
            "foo_long".to_string(),
            Evolution {
                type_string: SkipprDataType::Long,
                new_field: "foo_long".to_string(),
                sovled: true,
            },
        );
        root.insert("foo".to_string(), f);

        // Pre-existing field with the same name as the evolution target
        let mut existing = md();
        existing.determined_type = SkipprDataType::Long;
        existing.out_field_name = "foo_long".to_string();
        root.insert("foo_long".to_string(), existing);

        let res = Evolution::apply_evolution_factory("foo", &json!(42i64), &mut root, false);
        assert!(res.is_ok(), "should handle collision gracefully");
        assert_eq!(res.unwrap().field, "foo_long");
    }

    #[test]
    fn empty_string_discovers_as_string() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.out_field_name = "test_field".to_string();
        metadata.insert("test_field".to_string(), f);

        let mut updated = "no".to_string();
        let _ = Evolution::evolve_field(
            &"test_field".to_string(),
            &json!(""),
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );
        // Empty string should not trigger evolution away from string
    }

    #[test]
    fn null_value_on_string_field_no_panic() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.out_field_name = "test_field".to_string();
        metadata.insert("test_field".to_string(), f);

        let mut updated = "no".to_string();
        let _ = Evolution::evolve_field(
            &"test_field".to_string(),
            &json!(null),
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );
    }

    #[test]
    fn numeric_overflow_i64_max_as_string() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.out_field_name = "big".to_string();
        metadata.insert("big".to_string(), f);

        let val = json!(i64::MAX);
        let mut updated = "no".to_string();
        let _ = Evolution::evolve_field(
            &"big".to_string(),
            &val,
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );
    }

    #[test]
    fn timestamp_boundary_epoch_zero() {
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.evolution.insert(
            "f_timestamp".into(),
            Evolution {
                type_string: SkipprDataType::Timestamp,
                new_field: "f_timestamp".into(),
                sovled: true,
            },
        );
        root.insert("f".into(), f);
        let _ = Evolution::apply_evolution_factory("f", &json!(0i64), &mut root, false);
    }

    #[test]
    fn timestamp_boundary_negative() {
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.evolution.insert(
            "f_timestamp".into(),
            Evolution {
                type_string: SkipprDataType::Timestamp,
                new_field: "f_timestamp".into(),
                sovled: true,
            },
        );
        root.insert("f".into(), f);
        let _ = Evolution::apply_evolution_factory("f", &json!(-1000i64), &mut root, false);
    }

    #[test]
    fn deeply_nested_empty_object() {
        let mut metadata: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        f.out_field_name = "deep".to_string();
        metadata.insert("deep".to_string(), f);

        let val = json!({"a": {"b": {"c": {}}}});
        let mut updated = "no".to_string();
        let _ = Evolution::evolve_field(
            &"deep".to_string(),
            &val,
            None,
            None,
            &mut metadata,
            &mut updated,
            false,
        );
    }

    #[test]
    fn no_metadata_for_field_returns_error() {
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let res = Evolution::apply_evolution_factory("nonexistent", &json!(42), &mut root, false);
        assert!(res.is_err(), "should error when field has no metadata");
    }

    #[test]
    fn no_evolutions_registered_returns_error() {
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut f = md();
        f.determined_type = SkipprDataType::String;
        root.insert("f".to_string(), f);
        let res = Evolution::apply_evolution_factory("f", &json!(42), &mut root, false);
        assert!(res.is_err(), "should error when no evolutions registered");
    }
}
