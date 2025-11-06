use serde_json::value::Value;
use crate::discover::{AnalyseSchema, LAST_SUCCESSFUL_EVOLUTION, Metadata};
use std::collections::HashMap;
use arrow::error::ArrowError;
use crate::ingest::fast_ingest::fast_set_value;
use std::str::FromStr;
use std::borrow::BorrowMut;
use serde_derive::{Deserialize, Serialize};
use crate::ingest::ingest::{discover_ingest, ResolvedFieldValue};
use crate::discover::PipelineMetadata;


#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Evolution {
    pub type_string: String,
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
            &mut update_schmea_mock
        );

        let discoverd_data_type = metadata.get(temp_feild_name).unwrap().determined_type_values.clone();

        let new_feild_name = &format!("{}_array_{}", field, discoverd_data_type);

        //rename metadata[temp_feild_name] to new_feild_name
        let new = metadata.remove(temp_feild_name).unwrap();
        metadata.insert(new_feild_name.clone(), new);

        // println!("New data type for array field: '{}' is: '{}'", new_feild_name, discoverd_data_type);

        // Update existing field with the evolution to the new field
        let evo = Evolution {
            type_string: "array".to_string(),
            new_field: new_feild_name.clone(),
            sovled: true,
        };

        metadata.get_mut(field).unwrap().evolution.insert("array".to_string(), evo.clone());
    }

    pub fn evolve_field(
        field: &String,
        value: &Value,
        parent_field: Option<&str>,
        parent_data_type: Option<&str>,
        metadata: &mut HashMap<String, Metadata>,
        updated_schema: &mut String,
        flatten: bool
    ) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {

        // println!("Handling value error for field: '{}'", field);

        let foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let discoverd_data_type = foo.resolve_field_type(metadata.clone().borrow_mut(), &field.to_string(), value.clone().borrow_mut());


        // println!("Evolving new data type: '{}' for field '{}' with value '{}' with current data type of '{}'", discoverd_data_type, field, value, metadata.get(field).unwrap().determined_type);

        if discoverd_data_type != "" {

            let new_feild_name = &format!("{}_{}", field, discoverd_data_type);


            match metadata.get(field).unwrap().evolution.get(new_feild_name) {
                Some(_evolution) => {
                    if discoverd_data_type == "array" {
                        // println!("Have an evolution for field: '{}' to type: '{}' but it is an array, so ignoring", field, discoverd_data_type);

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
                },
                None => {
                    if discoverd_data_type == "array" {

                        // println!("Creating new evolution for array field: '{}' to type: '{}'", field, discoverd_data_type);

                        *updated_schema = "yes".to_string();

                        Evolution::evolve_array_field(
                            field,
                            value,
                            parent_field,
                            parent_data_type,
                            metadata,
                        );
                        return Ok(ResolvedFieldValue::new(new_feild_name.clone(), value.clone()));
                    }

                    println!("Creating new evolution for field: '{}' to type: '{}'", field, discoverd_data_type);

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
                         &mut update_schmea_mock
                    );

                    // Update existing field with the evolution to the new field
                    let evo = Evolution {
                        type_string: discoverd_data_type.clone(),
                        new_field: new_feild_name.clone(),
                        sovled: true,
                    };

                    metadata.get_mut(field).unwrap().evolution.insert(new_feild_name.to_string(), evo.clone());

                }
            };

            match Evolution::apply_evolution_factory(field, value, metadata, flatten) {
                Ok(v) => Ok(v),
                Err(e) => {
                    // println!("#### Error applying evolution factory: {}", e);
                    Err(e)
                },
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
        flatten: bool
    ) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {

        // println!("Applying evolution factory for field: '{}'", field);

        // get the cached last successful evolution for this field and try it first
        let mut found_value: Option<Result<ResolvedFieldValue, Box<dyn std::error::Error>>> = None;
        LAST_SUCCESSFUL_EVOLUTION.with(|last_evolution_refcell| {
            let last_evolution_guard = last_evolution_refcell.borrow();
            if let Some(evolution_key) = last_evolution_guard.get(field) {
                // Copy out needed data to avoid holding immutable borrows across mutation
                let (cached_key, new_field_name, type_string_opt): (String, Option<String>, Option<String>) = {
                    if let Some(field_metadata) = metadata.get(field) {
                        if let Some(evolution) = field_metadata.evolution.get(evolution_key) {
                            (evolution_key.clone(), Some(evolution.new_field.clone()), Some(evolution.type_string.clone()))
                        } else {
                            (evolution_key.clone(), None, None)
                        }
                    } else {
                        (evolution_key.clone(), None, None)
                    }
                };

                if let (Some(new_field), Some(type_string)) = (new_field_name, type_string_opt) {
                    // Try fast mapping first; only discover if needed
                    match fast_set_value(&type_string, &new_field, value, metadata, Some(false), flatten) {
                        Ok(v) => {
                            // Ensure minimal metadata exists for the evolved sibling when fast path succeeds
                            if !metadata.contains_key(&new_field) {
                                if let Ok(mut m) = Metadata::new() {
                                    m.determined_type = type_string.clone();
                                    m.out_field_name = new_field.clone();
                                    metadata.insert(new_field.clone(), m);
                                }
                            }
                            found_value = Some(Ok(ResolvedFieldValue::new(new_field.clone(), v.value)));
                        },
                        Err(_) => {
                            let mut updated = "no".to_string();
                            discover_ingest(
                                &new_field,
                                value,
                                None,
                                None,
                                metadata,
                                &mut updated
                            );
                            if let Ok(v2) = fast_set_value(&type_string, &new_field, value, metadata, Some(false), flatten) {
                                if !metadata.contains_key(&new_field) {
                                    if let Ok(mut m) = Metadata::new() {
                                        m.determined_type = type_string.clone();
                                        m.out_field_name = new_field.clone();
                                        metadata.insert(new_field.clone(), m);
                                    }
                                }
                                found_value = Some(Ok(ResolvedFieldValue::new(new_field.clone(), v2.value)));
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
                fn score_evolution(value: &Value, evolution: &Evolution, metadata: &HashMap<String, Metadata>) -> i32 {
                    let mut s: i32 = 0;
                    match evolution.type_string.as_str() {
                        "timestamp_milli" => {
                            s += 40;
                            if let Some(i) = value.as_i64() {
                                if i >= 1_000_000_000_000 { s += 30; }
                            }
                        },
                        "timestamp" => {
                            s += 20;
                            if let Some(i) = value.as_i64() {
                                if i < 1_000_000_000_000 { s += 10; }
                            }
                        },
                        "double" => {
                            s += 25;
                            if value.is_f64() { s += 10; }
                        },
                        "long" => {
                            s += 18;
                            if value.is_i64() { s += 5; }
                        },
                        "integer" => {
                            s += 16;
                            if value.is_i64() { s += 4; }
                        },
                        "map" => {
                            s += 12;
                            if value.is_object() { s += 5; }
                            if let Some(m) = metadata.get(&evolution.new_field) {
                                if !m.determined_type_values.is_empty() {
                                    // Prefer specified values types; numeric values -> prefer double
                                    if m.determined_type_values == "double" { s += 12; }
                                    else if m.determined_type_values == "long" { s += 8; }
                                    else { s += 4; }
                                }
                            }
                        },
                        "array" => {
                            s += 12;
                            if value.is_array() { s += 5; }
                            if let Some(m) = metadata.get(&evolution.new_field) {
                                if !m.determined_type_values.is_empty() {
                                    if m.determined_type_values == "double" { s += 12; }
                                    else if m.determined_type_values == "long" { s += 8; }
                                    else { s += 4; }
                                }
                            }
                        },
                        "record" => {
                            s += 14;
                            if value.is_object() { s += 4; }
                        },
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
                    match fast_set_value(&evolution.type_string, &evolution.new_field, value, metadata, Some(false), flatten) {
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
                                last_evolution_guard.insert(field.to_string(), evolution_key.clone());
                            });
                            return Ok(ResolvedFieldValue::new(evolution.new_field.clone(), v.value));
                        },
                        Err(first_err) => {
                            let mut updated = "no".to_string();
                            discover_ingest(
                                &evolution.new_field,
                                value,
                                None,
                                None,
                                metadata,
                                &mut updated
                            );
                            match fast_set_value(&evolution.type_string, &evolution.new_field, value, metadata, Some(false), flatten) {
                                Ok(v2) => {
                                    if !metadata.contains_key(&evolution.new_field) {
                                        if let Ok(mut m) = Metadata::new() {
                                            m.determined_type = evolution.type_string.clone();
                                            m.out_field_name = evolution.new_field.clone();
                                            metadata.insert(evolution.new_field.clone(), m);
                                        }
                                    }
                                    LAST_SUCCESSFUL_EVOLUTION.with(|last_evolution_refcell| {
                                        let mut last_evolution_guard = last_evolution_refcell.borrow_mut();
                                        last_evolution_guard.insert(field.to_string(), evolution_key.clone());
                                    });
                                    return Ok(ResolvedFieldValue::new(evolution.new_field.clone(), v2.value));
                                },
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
                Err(Box::new(ArrowError::ParseError(format!("No schema evolutions succeeded for field: '{}', Errors: {}", field, 
                                                            errors.iter().map(|e| e.to_string()).collect::<Vec<String>>().join(", ")
                ))))
            },
            None => {
                // println!("No metadata for evolution field: '{}'", field);
                return Err(Box::new(ArrowError::ParseError(format!("No metadata for evolution field: '{}'", field))));
            }
        }
    }

}

// Proposal DTOs used by the sequencer/data-plane
#[derive(Clone, Debug)]
pub struct EvolutionSpec {
    pub parent: Option<String>,
    pub field: String,
    pub required_type: String,
    pub values_type: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EvolutionProposal {
    pub namespace: String,
    pub fields: Vec<EvolutionSpec>,
}

pub fn infer_specs_for_record(record: &serde_json::Value, metadata: &HashMap<String, Metadata>) -> Vec<EvolutionSpec> {
    let mut specs = Vec::new();
    if let Some(obj) = record.as_object() {
        for (k, v) in obj.iter() {
            match metadata.get(k) {
                None => {
                    // brand new field
                    let (req, vals) = infer_required_type(v);
                    specs.push(EvolutionSpec { parent: None, field: k.clone(), required_type: req, values_type: vals });
                },
                Some(md) => {
                    // existing field but possibly different type -> propose sibling evolution
                    let analyser = AnalyseSchema { i: 0 };
                    // Work on a temp copy to reuse resolver
                    let mut temp = metadata.clone();
                    let detected = analyser.resolve_field_type(temp.borrow_mut(), &k.to_string(), v.clone().borrow_mut());
                    if !detected.is_empty() && detected != md.determined_type {
                        if detected == "array" {
                            // refine using element type
                            let (req, vals) = infer_required_type(v);
                            specs.push(EvolutionSpec { parent: None, field: k.clone(), required_type: req, values_type: vals });
                        } else {
                            specs.push(EvolutionSpec { parent: None, field: k.clone(), required_type: detected, values_type: None });
                        }
                    }
                }
            }
        }
    }
    specs
}

fn infer_required_type(value: &serde_json::Value) -> (String, Option<String>) {
    use serde_json::Value as V;
    match value {
        V::Null => ("string".to_string(), None),
        V::Bool(_) => ("boolean".to_string(), None),
        V::Number(n) => {
            if n.is_i64() { ("long".to_string(), None) } else { ("double".to_string(), None) }
        },
        V::String(_) => ("string".to_string(), None),
        V::Array(arr) => {
            if let Some(V::Object(_)) = arr.get(0) { ("array".to_string(), Some("record".to_string())) } else { ("array".to_string(), Some("string".to_string())) }
        },
        V::Object(_) => ("record".to_string(), None),
    }
}

pub fn apply_specs_to_namespace(namespace: &str, specs: &[EvolutionSpec], pm: &mut PipelineMetadata) {
    if let Some(ns_meta) = pm.metadata.get_mut(namespace) {
        for s in specs.iter() {
            let target: &mut HashMap<String, Metadata> = match &s.parent {
                Some(parent) => match ns_meta.fields.get_mut(parent) { Some(m) => &mut m.fields, None => continue },
                None => &mut ns_meta.fields,
            };
            let evolved_name = if s.required_type == "array" && s.values_type.is_some() {
                format!("{}_array_{}", s.field, s.values_type.clone().unwrap())
            } else {
                format!("{}_{}", s.field, s.required_type)
            };
            if !target.contains_key(&evolved_name) {
                let mut md = Metadata::new().unwrap();
                md.determined_type = if s.required_type == "array" { s.values_type.clone().unwrap_or("string".to_string()) } else { s.required_type.clone() };
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
        wf.determined_type = "string".to_string();
        wf.out_field_name = "water_flowmeter".to_string();
        let evo = Evolution { type_string: "record".to_string(), new_field: "water_flowmeter_record".to_string(), sovled: true };
        wf.evolution.insert("water_flowmeter_record".to_string(), evo);
        root.insert("water_flowmeter".to_string(), wf);

        // water_flowmeter_record exists with some children but missing 'is_fitted'
        let mut wfr = Metadata::new().unwrap();
        wfr.determined_type = "record".to_string();
        // pre-populate a couple of children
        let mut data_valid = Metadata::new().unwrap();
        data_valid.determined_type = "boolean".to_string();
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
        let record_md = root.get("water_flowmeter_record").expect("record md present");
        assert!(record_md.fields.get("is_fitted").is_some(), "is_fitted should be discovered");
        assert_eq!(record_md.fields.get("is_fitted").unwrap().determined_type, "boolean");
    }

    #[test]
    fn test_child_type_change_evolves_sibling_under_record() {
        let flatten = false;
        // Parent record with child temperature_degc as string; evolve to double
        let mut parent_fields: HashMap<String, Metadata> = HashMap::new();
        let mut temp = Metadata::new().unwrap();
        temp.determined_type = "string".to_string();
        // Add an evolution entry to double sibling
        let evo = Evolution { type_string: "double".to_string(), new_field: "temperature_degc_double".to_string(), sovled: true };
        temp.evolution.insert("temperature_degc_double".to_string(), evo);
        parent_fields.insert("temperature_degc".to_string(), temp);

        // Incoming numeric value
        let v = json!(12.34);

        let out = Evolution::apply_evolution_factory("temperature_degc", &v, &mut parent_fields, flatten)
            .expect("child evolution should succeed");

        // Should return the evolved sibling and numeric value
        assert_eq!(out.field, "temperature_degc_double");
        assert!(out.value.is_number());

        // Ensure evolved sibling exists in metadata
        assert!(parent_fields.get("temperature_degc_double").is_some());
        assert_eq!(parent_fields.get("temperature_degc_double").unwrap().determined_type, "double");
    }
}

#[cfg(test)]
mod tests_evolution_chained {
    use super::*;
    use serde_json::json;
    use crate::discover::LAST_SUCCESSFUL_EVOLUTION;

    fn md() -> Metadata { Metadata::new().unwrap() }

    #[test]
    fn test_primitive_chain_string_to_long_to_double_with_cached_miss() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // Field 'v' initially string with two sibling evolutions: long and double
        let mut v = md();
        v.determined_type = "string".to_string();
        v.out_field_name = "v".to_string();
        v.evolution.insert(
            "v_long".to_string(),
            Evolution { type_string: "long".to_string(), new_field: "v_long".to_string(), sovled: true }
        );
        v.evolution.insert(
            "v_double".to_string(),
            Evolution { type_string: "double".to_string(), new_field: "v_double".to_string(), sovled: true }
        );
        root.insert("v".to_string(), v);

        // Seed cache to the wrong evolution to simulate cached miss
        LAST_SUCCESSFUL_EVOLUTION.with(|cell| cell.borrow_mut().insert("v".to_string(), "v_long".to_string()));

        // Provide a float -> should resolve to v_double
        let val = json!(12.34);
        let out = Evolution::apply_evolution_factory("v", &val, &mut root, flatten).expect("evolution succeeds");
        assert_eq!(out.field, "v_double");
        assert!(out.value.is_number());
    }

    #[test]
    fn test_timestamp_chain_string_to_ts_to_ts_milli() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // 'ts' has two evolutions: timestamp, then timestamp_milli
        let mut ts = md();
        ts.determined_type = "string".to_string();
        ts.out_field_name = "ts".to_string();
        ts.evolution.insert(
            "ts_timestamp".to_string(),
            Evolution { type_string: "timestamp".to_string(), new_field: "ts_timestamp".to_string(), sovled: true }
        );
        ts.evolution.insert(
            "ts_timestamp_milli".to_string(),
            Evolution { type_string: "timestamp_milli".to_string(), new_field: "ts_timestamp_milli".to_string(), sovled: true }
        );
        root.insert("ts".to_string(), ts);

        let val = json!(1700000000123i64); // ms
        let out = Evolution::apply_evolution_factory("ts", &val, &mut root, flatten).expect("evolution succeeds");
        assert_eq!(out.field, "ts_timestamp_milli");
        assert!(out.value.is_number());
    }

    #[test]
    fn test_record_then_child_evolves_again() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // sensor evolves to sensor_record
        let mut sensor = md();
        sensor.determined_type = "string".to_string();
        sensor.out_field_name = "sensor".to_string();
        sensor.evolution.insert(
            "sensor_record".to_string(),
            Evolution { type_string: "record".to_string(), new_field: "sensor_record".to_string(), sovled: true }
        );
        root.insert("sensor".to_string(), sensor);

        // Prime record sibling with one child 'a' as string and evolution to 'a_double'
        let mut rec = md();
        rec.determined_type = "record".to_string();
        let mut a = md();
        a.determined_type = "string".to_string();
        a.out_field_name = "a".to_string();
        a.evolution.insert(
            "a_double".to_string(),
            Evolution { type_string: "double".to_string(), new_field: "a_double".to_string(), sovled: true }
        );
        rec.fields.insert("a".to_string(), a);
        root.insert("sensor_record".to_string(), rec);

        // First apply parent evolution
        let obj = json!({ "a": 1.23 });
        let r = Evolution::apply_evolution_factory("sensor", &obj, &mut root, flatten).expect("parent evolve ok");
        assert_eq!(r.field, "sensor_record");

        // Now evolve child under the record
        if let Some(rec_md) = root.get_mut("sensor_record") {
            let out = Evolution::apply_evolution_factory("a", &json!(1.23), rec_md.fields.as_mut(), flatten).expect("child evolve ok");
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
        arr.determined_type = "string".to_string();
        arr.out_field_name = "arr".to_string();
        arr.evolution.insert(
            "arr_array_long".to_string(),
            Evolution { type_string: "array".to_string(), new_field: "arr_array_long".to_string(), sovled: true }
        );
        arr.evolution.insert(
            "arr_array_double".to_string(),
            Evolution { type_string: "array".to_string(), new_field: "arr_array_double".to_string(), sovled: true }
        );
        root.insert("arr".to_string(), arr);

        // Prime metadata for the target array values type so fast mapping can work without full discovery
        let mut arr_double = md();
        arr_double.determined_type = "array".to_string();
        arr_double.determined_type_values = "double".to_string();
        arr_double.out_field_name = "arr_array_double".to_string();
        root.insert("arr_array_double".to_string(), arr_double);

        let val = json!([1.1, 2.2, 3.3]);
        let out = Evolution::apply_evolution_factory("arr", &val, &mut root, flatten).expect("array evolve ok");
        assert_eq!(out.field, "arr_array_double");
        assert!(out.value.is_array());
    }

    #[test]
    fn test_map_values_type_chain() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // attrs evolves to map<string,double>
        let mut attrs = md();
        attrs.determined_type = "string".to_string();
        attrs.out_field_name = "attrs".to_string();
        attrs.evolution.insert(
            "attrs_map".to_string(),
            Evolution { type_string: "map".to_string(), new_field: "attrs_map".to_string(), sovled: true }
        );
        attrs.evolution.insert(
            "attrs_map_double".to_string(),
            Evolution { type_string: "map".to_string(), new_field: "attrs_map_double".to_string(), sovled: true }
        );
        root.insert("attrs".to_string(), attrs);

        // Prime target map values type
        let mut map_double = md();
        map_double.determined_type = "map".to_string();
        map_double.determined_type_values = "double".to_string();
        map_double.out_field_name = "attrs_map_double".to_string();
        root.insert("attrs_map_double".to_string(), map_double);

        let val = json!({"k1": 1.2, "k2": 3.4});
        let out = Evolution::apply_evolution_factory("attrs", &val, &mut root, flatten).expect("map evolve ok");
        assert_eq!(out.field, "attrs_map_double");
        assert!(out.value.is_object());
    }
}

#[cfg(test)]
mod tests_evolution_more_types {
    use super::*;
    use serde_json::json;

    fn md() -> Metadata { Metadata::new().unwrap() }

    #[test]
    fn test_string_to_boolean_sibling() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut f = md();
        f.determined_type = "string".to_string();
        f.out_field_name = "flag".to_string();
        f.evolution.insert(
            "flag_bool".to_string(),
            Evolution { type_string: "boolean".to_string(), new_field: "flag_bool".to_string(), sovled: true }
        );
        root.insert("flag".to_string(), f);

        let val = json!("true");
        let out = Evolution::apply_evolution_factory("flag", &val, &mut root, flatten).expect("bool evolve ok");
        assert_eq!(out.field, "flag_bool");
        assert!(out.value.is_boolean());
    }

    #[test]
    fn test_string_to_date_sibling() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut d = md();
        d.determined_type = "string".to_string();
        d.out_field_name = "d".to_string();
        d.evolution.insert(
            "d_date".to_string(),
            Evolution { type_string: "date".to_string(), new_field: "d_date".to_string(), sovled: true }
        );
        root.insert("d".to_string(), d);

        let val = json!("2024-01-02");
        let out = Evolution::apply_evolution_factory("d", &val, &mut root, flatten).expect("date evolve ok");
        assert_eq!(out.field, "d_date");
    }

    #[test]
    fn test_timestamp_prefers_seconds_when_small() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();
        let mut ts = md();
        ts.determined_type = "long".to_string();
        ts.out_field_name = "ts".to_string();
        ts.evolution.insert(
            "ts_timestamp".to_string(),
            Evolution { type_string: "timestamp".to_string(), new_field: "ts_timestamp".to_string(), sovled: true }
        );
        ts.evolution.insert(
            "ts_timestamp_milli".to_string(),
            Evolution { type_string: "timestamp_milli".to_string(), new_field: "ts_timestamp_milli".to_string(), sovled: true }
        );
        root.insert("ts".to_string(), ts);

        let val = json!(1_700_000_000i64); // seconds-ish
        let out = Evolution::apply_evolution_factory("ts", &val, &mut root, flatten).expect("ts evolve ok");
        assert_eq!(out.field, "ts_timestamp");
    }

    #[test]
    fn test_array_string_to_array_long() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        let mut arr = md();
        arr.determined_type = "string".to_string();
        arr.out_field_name = "arr".to_string();
        arr.evolution.insert(
            "arr_array_long".to_string(),
            Evolution { type_string: "array".to_string(), new_field: "arr_array_long".to_string(), sovled: true }
        );
        root.insert("arr".to_string(), arr);

        // Prime target array with values type = long so fast mapping can succeed
        let mut target = md();
        target.determined_type = "array".to_string();
        target.determined_type_values = "long".to_string();
        target.out_field_name = "arr_array_long".to_string();
        root.insert("arr_array_long".to_string(), target);

        let val = json!(["1", "2", "3"]);
        let out = Evolution::apply_evolution_factory("arr", &val, &mut root, flatten).expect("array long evolve ok");
        assert_eq!(out.field, "arr_array_long");
        assert!(out.value.is_array());
    }

    #[test]
    fn test_record_child_string_to_long() {
        let flatten = false;
        let mut root: HashMap<String, Metadata> = HashMap::new();

        // parent record exists with child n: string -> n_long
        let mut rec = md();
        rec.determined_type = "record".to_string();
        let mut n = md();
        n.determined_type = "string".to_string();
        n.out_field_name = "n".to_string();
        n.evolution.insert(
            "n_long".to_string(),
            Evolution { type_string: "long".to_string(), new_field: "n_long".to_string(), sovled: true }
        );
        rec.fields.insert("n".to_string(), n);
        root.insert("rec".to_string(), rec);

        // value for child
        if let Some(rec_md) = root.get_mut("rec") {
            let out = Evolution::apply_evolution_factory("n", &json!("1234"), rec_md.fields.as_mut(), flatten).expect("child cast ok");
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
    use std::collections::HashMap;
    use serde_json::{Map, Value};

    fn setup_metadata() -> HashMap<String, Metadata> {
        let mut metadata = HashMap::new();
        let mut md = Metadata::new().unwrap();
        md.determined_type = "string".to_string();
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
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema, flatten);

        let expected_data_type = "long".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_new_field).unwrap().new_field, expected_new_field);
        // Assert the new evolved fields Metadata
        assert_eq!(metadata.get(&expected_new_field).unwrap().determined_type, expected_data_type);
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_integer() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Number(serde_json::Number::from(10));
        let flatten = false;
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema, flatten);

        // The expected type should be integer, not timestamp with our more conservative timestamp detection
        let expected_data_type = "integer".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_new_field).unwrap().new_field, expected_new_field);
        // Assert the new evolved fields Metadata
        assert_eq!(metadata.get(&expected_new_field).unwrap().determined_type, expected_data_type);
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_boolean() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Bool(true);
        let flatten = false;

        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema, flatten);

        let expected_data_type = "boolean".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_new_field).unwrap().new_field, expected_new_field);
        // Assert the new evolved fields Metadata
        assert_eq!(metadata.get(&expected_new_field).unwrap().determined_type, expected_data_type);
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_null() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Null;
        let flatten = false;
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema, flatten);

        let expected_data_type = "string".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_new_field).unwrap().new_field, expected_new_field);
        // The field is actually created, so check that it exists
        assert!(metadata.get(&expected_new_field).is_some());
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_array() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Array(vec![Value::String("test".to_string())]);
        let flatten = false;
        
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema, flatten);

        // First we create a temporary field with "_unknown" suffix
        let _temp_field_name = format!("{}_{}", &field, "unknown");
        // Then the final field is derived as field + "_array_" + element data type (string)
        let expected_new_field = format!("{}_array_{}", &field, "string");

        assert!(result.is_ok());
        // Assert the Evolution - the key in the map is "array", not the new field name
        assert_eq!(metadata.get(&field).unwrap().evolution.get("array").unwrap().new_field, expected_new_field);
        // Assert the new evolved fields Metadata exists with the correct type
        assert!(metadata.get(&expected_new_field).is_some());
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_map() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let mut value = Value::Object(serde_json::Map::new());
        let flatten = false;
        // Create a simple map with consistent value types
        value.as_object_mut().unwrap().insert("test".to_string(), Value::String("test".to_string()));
        value.as_object_mut().unwrap().insert("test2".to_string(), Value::String("test2".to_string()));
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema, flatten);

        assert!(result.is_ok());

        // Get the actual evolution that was created
        let field_metadata = metadata.get(&field).unwrap();
        assert_eq!(field_metadata.evolution.len(), 1, "Should have exactly one evolution");
        
        // Get the first (and only) evolution
        let (_, evolution) = field_metadata.evolution.iter().next().unwrap();
        
        // Assert the evolution points to a record type
        assert_eq!(evolution.type_string, "record");
        
        // Get the evolved field's metadata
        let evolved_field_metadata = metadata.get(&evolution.new_field).unwrap();
        assert_eq!(evolved_field_metadata.determined_type, "record");
        
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_record() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let mut complex_struct = Map::new();
        complex_struct.insert("test".to_string(), Value::String("test".to_string()));
        complex_struct.insert("test2".to_string(), Value::Number(123.into()));
        complex_struct.insert("test2".to_string(), Value::Array(vec![Value::String("test".to_string())]));
        let value = Value::Object(complex_struct);
        let flatten = false;
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema, flatten);

        let expected_data_type = "record".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_new_field).unwrap().new_field, expected_new_field);
        // Assert the new evolved fields Metadata
        assert_eq!(metadata.get(&expected_new_field).unwrap().determined_type, expected_data_type);
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
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
