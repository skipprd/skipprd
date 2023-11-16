use serde_json::value::Value;
use crate::discover::{AnalyseSchema, LAST_SUCCESSFUL_EVOLUTION, Metadata};
use std::collections::HashMap;
use arrow::error::ArrowError;
use crate::ingest::fast_ingest::fast_set_value;
use std::str::FromStr;
use std::borrow::BorrowMut;
use serde_derive::{Deserialize, Serialize};
use crate::ingest::ingest::{discover_ingest, ResolvedFieldValue};


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
        mut updated_schema: &mut String,
    ) -> Result<ResolvedFieldValue, Box<dyn std::error::Error>> {

        // println!("Handling value error for field: '{}'", field);

        let mut foo: AnalyseSchema = AnalyseSchema { i: 0 };
        let mut discoverd_data_type = foo.resolve_field_type(metadata.clone().borrow_mut(), &field.to_string(), value.clone().borrow_mut());


        // println!("Evolving new data type: '{}' for field '{}' with value '{}' with current data type of '{}'", discoverd_data_type, field, value, metadata.get(field).unwrap().determined_type);

        if discoverd_data_type != "" {

            let new_feild_name = &format!("{}_{}", field, discoverd_data_type);


            match metadata.get(field).unwrap().evolution.get(new_feild_name) {
                Some(evolution) => {
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

            match Evolution::apply_evolution_factory(field, value, metadata) {
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
                        match fast_set_value(evolution_key, &evolution.new_field,  value, metadata, Some(false)) {
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

        // iterate through the evolutions and try the existing ones
        match metadata.get(field) {
            Some(field_metadata) => {
                for (evolution_key, evolution) in field_metadata.evolution.iter() {
                    // println!("Trying evolution: '{}' for field: '{}'", evolution_key, field);
                    // match match_scalar_value_fast(&evolution.new_field, evolution_key, value, metadata, false) {
                    match fast_set_value(&evolution.type_string, &evolution.new_field, value, metadata, Some(false)) {
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
                            println!("Evolution failed for field: '{}' to evolution: '{}' => '{}', Error: {}", field, &evolution.new_field, evolution_key, _err)
                        }
                    }
                }

                // throw Err() if no evolutions are Ok()
                // println!("No evolutions succeeded for field: '{}'", field);
                Err(Box::new(ArrowError::ParseError("Unable to parse value".to_string())))
            },
            None => {
                // println!("No metadata for evolution field: '{}'", field);
                return Err(Box::new(ArrowError::ParseError("### No metadata for evolution field".to_string())));
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
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema);

        let expected_data_type = "long".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_data_type).unwrap().new_field, expected_new_field);
        // Assert the new evolved fields Metadata
        assert_eq!(metadata.get(&expected_new_field).unwrap().determined_type, expected_data_type);
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_integer() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Number(123.into());
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema);

        let expected_data_type = "integer".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_data_type).unwrap().new_field, expected_new_field);
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

        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema);

        let expected_data_type = "boolean".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_data_type).unwrap().new_field, expected_new_field);
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
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema);

        let expected_data_type = "string".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_data_type).unwrap().new_field, expected_new_field);
        // Assert no evolution was performed
        assert!(metadata.get(&expected_new_field).is_none());
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_array() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let value = Value::Array(vec![Value::String("test".to_string())]);

        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema);

        let expected_data_type = "array".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_data_type).unwrap().new_field, expected_new_field);
        // Assert the new evolved fields Metadata
        assert_eq!(metadata.get(&expected_new_field).unwrap().determined_type, expected_data_type);
        // Assert old field is unchanged
        assert_eq!(metadata.get(&field).unwrap().determined_type, "string");
    }

    #[test]
    fn test_evolve_field_map() {
        let mut metadata = setup_metadata();
        let field = "test_field".to_string();
        let mut value = Value::Object(serde_json::Map::new());
        value.as_object_mut().unwrap().insert("test".to_string(), Value::String("test".to_string()));
        value.as_object_mut().unwrap().insert("test2".to_string(), Value::String("test2".to_string()));
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema);

        let expected_data_type = "map".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_data_type).unwrap().new_field, expected_new_field);
        // Assert the new evolved fields Metadata
        assert_eq!(metadata.get(&expected_new_field).unwrap().determined_type, expected_data_type);
        assert_eq!(metadata.get(&expected_new_field).unwrap().determined_type_values, "string");
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
        let mut updated_schema= "no".to_string();

        let result = Evolution::evolve_field(&field, &value, None, None, &mut metadata, &mut updated_schema);

        let expected_data_type = "record".to_string();
        let expected_new_field = format!("{}_{}", &field, expected_data_type).to_string();

        assert!(result.is_ok());
        // Assert the Evolution
        assert_eq!(metadata.get(&field).unwrap().evolution.get(&expected_data_type).unwrap().new_field, expected_new_field);
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
