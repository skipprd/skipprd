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
        metadata: &HashMap<String, Metadata>,
        flatten: bool
    ) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {

        // println!("Applying evolution factory for field: '{}'", field);

        // get the cached last successful evolution for this field and try it first
        let mut found_value: Option<Result<ResolvedFieldValue, Box<dyn std::error::Error>>> = None;
        LAST_SUCCESSFUL_EVOLUTION.with(|last_evolution_refcell| {
            let last_evolution_guard = last_evolution_refcell.borrow();
            if let Some(evolution_key) = last_evolution_guard.get(field) {
                if let Some(field_metadata) = metadata.get(field) {
                    if let Some(evolution) = field_metadata.evolution.get(evolution_key) {
                        // match match_scalar_value_fast(&evolution.new_field, &evolution_key, value, metadata, false) {
                        match fast_set_value(evolution_key, &evolution.new_field,  value, metadata, Some(false), flatten) {
                            Ok(v) => {
                                // set value if the evolution succeeds
                                // println!("Cached evolution succeeded for field: '{}' with evolution: '{}'", field, evolution_key);
                                found_value = Some(Ok(ResolvedFieldValue::new(evolution.new_field.clone(), v.value)));
                            },
                            Err(_) => { }
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
                for (evolution_key, evolution) in field_metadata.evolution.iter() {
                    // println!("Trying evolution: '{}' for field: '{}'", evolution_key, field);
                    // match match_scalar_value_fast(&evolution.new_field, evolution_key, value, metadata, false) {
                    match fast_set_value(&evolution.type_string, &evolution.new_field, value, metadata, Some(false), flatten) {
                        Ok(v) => {
                            // cache the last evolution that worked
                            LAST_SUCCESSFUL_EVOLUTION.with(|last_evolution_refcell| {
                                let mut last_evolution_guard = last_evolution_refcell.borrow_mut();
                                last_evolution_guard.insert(field.to_string(), evolution_key.clone());
                            });
                            // println!("Evolution succeeded for field: '{}' to evolution: '{}' => '{}'", field, &evolution.new_field, evolution_key);
                            return Ok(ResolvedFieldValue::new(evolution.new_field.clone(), v.value));
                        },
                        Err(_err) => {
                            errors.push(_err);
                            // println!("Evolution failed for field: '{}' to evolution: '{}' => '{}', Error: {}", field, &evolution.new_field, evolution_key, _err)
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
