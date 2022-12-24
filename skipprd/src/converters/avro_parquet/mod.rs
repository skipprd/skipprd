use std::collections::HashMap;

pub struct AvroSchema {
}


impl AvroSchema {

    const INT_MIN_VALUE: i32 = -2147483648;
    const INT_MAX_VALUE: i32 = 2147483647;
    const LONG_MIN_VALUE: i64 = -9223372036854775808;
    const LONG_MAX_VALUE: i64 = 9223372036854775807;
    const NULL_TYPE: &'static str = "null";
    const BOOLEAN_TYPE: &'static str = "boolean";
    const INT_TYPE: &'static str = "int";
    const LONG_TYPE: &'static str = "long";
    const FLOAT_TYPE: &'static str = "float";
    const DOUBLE_TYPE: &'static str = "double";
    const STRING_TYPE: &'static str = "string";
    const BYTES_TYPE: &'static str = "bytes";
    const ARRAY_SCHEMA: &'static str = "array";
    const MAP_SCHEMA: &'static str = "map";
    const UNION_SCHEMA: &'static str = "union";
    const ERROR_UNION_SCHEMA: &'static str = "error_union";
    const ENUM_SCHEMA: &'static str = "enum";
    const FIXED_SCHEMA: &'static str = "fixed";
    const RECORD_SCHEMA: &'static str = "record";
    const ERROR_SCHEMA: &'static str = "error";
    const REQUEST_SCHEMA: &'static str = "request";
    const TYPE_ATTR: &'static str = "type";
    const NAME_ATTR: &'static str = "name";
    const NAMESPACE_ATTR: &'static str = "namespace";
    const FULLNAME_ATTR: &'static str = "fullname";
    const SIZE_ATTR: &'static str = "size";
    const FIELDS_ATTR: &'static str = "fields";
    const ITEMS_ATTR: &'static str = "items";
    const SYMBOLS_ATTR: &'static str = "symbols";
    const VALUES_ATTR: &'static str = "values";
    const DOC_ATTR: &'static str = "doc";

    pub fn is_named_type(&self, type_string: &str) -> bool {
        self.named_types.contains(&type_string)
    }

    pub fn is_primitive_type(&self, type_string: &str) -> bool {
        self.primitive_types.contains(&type_string)
    }

    pub fn is_valid_type(&self, type_string: &str) -> bool {
        self.is_primitive_type(type_string) || self.is_named_type(type_string)
    }


    pub fn parse(json: &str) -> Result<AvroSchema, AvroSchemaParseException> {
        let schemata = AvroNamedSchemata::new();
        let avro = json::decode(json);
        if avro.is_err() {
            return Err(AvroSchemaParseException::new("JSON decode error"));
        }
        return Ok(AvroSchema::real_parse(avro.unwrap(), None, schemata));
    }

    fn get_type(&self) -> &'static str {
        return "foo"
    }
}

fn convert_field(field_name: String, schema: AvroSchema, repetition: String) -> HashMap<String, String> {
    let mut parquet_field: HashMap<String, String> = HashMap::new();
    parquet_field.insert("name".to_string(), field_name);

    let type_string = schema.get_type();
    let logical_type = schema.get_type();

    if let Some(parquet_type) = convert_primitive_type(type_string) {
        parquet_field.insert("type".to_string(), parquet_type);
        parquet_field.insert("repeat".to_string(), repetition);
    } else if type_string == AvroSchema::RECORD_SCHEMA {
        parquet_field.insert("type".to_string(), "group".to_string());
        parquet_field.insert("repeat".to_string(), repetition);
        parquet_field.insert("schema".to_string(), convert_fields(schema));
    } else if type_string == AvroSchema::ENUM_SCHEMA {
        parquet_field.insert("type".to_string(), "group".to_string());
        parquet_field.insert("repeat".to_string(), repetition);
        parquet_field.insert("schema".to_string(), convert_field(schema.qualified_name(), schema.symbols(), REQUIRED));
    } else if type_string == AvroSchema::ARRAY_SCHEMA {
        parquet_field.insert("type".to_string(), "group".to_string());
        parquet_field.insert("repeat".to_string(), repetition);
        parquet_field.insert("annotation".to_string(), "LIST".to_string());

        parquet_field.insert("schema".to_string(), "group".to_string());
        parquet_field.insert("repeat".to_string(), REPEATED);
        parquet_field.insert("name".to_string(), "list".to_string());

        // support list elements of primitive types and array of arrays
        parquet_field.insert("schema".to_string(), convert_field("element".to_string(), schema.items(), REQUIRED));
    } else if type_string == AvroSchema::MAP_SCHEMA {
        parquet_field.insert("type".to_string(), "group".to_string());
        parquet_field.insert("repeat".to_string(), repetition);
        parquet_field.insert("annotation".to_string(), "MAP".to_string());

        for item_schema in schema.values() {
            parquet_field.insert("schema".to_string(), "group".to_string());
            parquet_field.insert("repeat".to_string(), repetition);
            parquet_field.insert("annotation".to_string(), "MAP_KEY_VALUE".to_string()); // map keys are always strings

            // key
            parquet_field.insert("schema".to_string(), repetition);
            // avro map key type is always string
            parquet_field.insert("key_type".to_string(), "string".to_string());
            parquet_field.insert("name".to_string(), "key".to_string());

            // value
            parquet_field.insert("schema".to_string(), repetition);
            parquet_field.insert("value_type".to_string(), convert_primitive_type(item_schema));
            parquet_field.insert("name".to_string(), "value".to_string());
        }
    } else if type_string == AvroSchema::FIXED_SCHEMA {
        parquet_field.insert("type".to_string(), FIXED_LEN_BYTE_ARRAY);
        parquet_field.insert("repeat".to_string(), repetition);
    } else if type_string == AvroSchema::UNION_SCHEMA {
        return convert_union(field_name, schema, repetition);
    } else {
        panic!("Cannot convert Avro type {}", type_string);
    }

    // schema translation can only be done for known logical types because this
    // creates an equivalence
    //    if (logicalType != null) {
    //        if (logicalType instanceof LogicalTypes.Decimal) {
    //            LogicalTypes.Decimal decimal = (LogicalTypes.Decimal) logicalType;
    //        builder = builder.as(decimalType(decimal.getScale(), decimal.getPrecision()));
    //      } else {
    //            LogicalTypeAnnotation annotation = convertLogicalType(logicalType);
    //        if (annotation != null) {
    //            builder.as(annotation);
    //        }
    //      }
    //    }

    return parquet_field;
}

fn convert_primitive_type(type_string: &str) -> Option<String> {
    match type_string {
        "boolean" => Some(String::from("BOOLEAN")),
        "int" => Some(String::from("INT32")),
        "long" => Some(String::from("INT64")),
        "float" => Some(String::from("FLOAT")),
        "double" => Some(String::from("DOUBLE")),
        "bytes" => Some(String::from("BYTE_ARRAY")),
        "string" => Some(String::from("BYTE_ARRAY")),
        _ => None,
    }
}

pub fn convert_union(field_name: &str, avro_schema: &AvroSchema, repetition: &str) -> ParquetField {
    let mut non_null_schemas: Vec<AvroSchema> = Vec::new();
    let mut found_null_schema = false;

    for sub_schema in avro_schema.schemas() {
        if sub_schema.string_type == AvroSchemaType::Null {
            found_null_schema = true;
            if repetition == REQUIRED {
                repetition = OPTIONAL;
            }
        } else {
            non_null_schemas.push(sub_schema);
        }
    }

    match non_null_schemas.len() {
        0 => {
            let mut parquet_field = ParquetField::new();
            parquet_field.set_type(BYTE_ARRAY);
            parquet_field.set_repetition(repetition);
            parquet_field
        }
        1 => {
            if found_null_schema {
                convert_field(field_name, &non_null_schemas[0], repetition)
            } else {
                convert_union_to_group_type(field_name, repetition, &non_null_schemas)
            }
        }
        _ => convert_union_to_group_type(field_name, repetition, &non_null_schemas),
    }
}

pub fn convert_union_to_group_type(field_name: String, repetition: String, non_null_schemas: Vec<String>) -> String {
    let mut union_types: Vec<String> = Vec::new();
    let mut i = 0;

    for sub_schema in non_null_schemas {
        union_types.push(convert_field(
            format!("member{}", i),
            sub_schema,
            String::from("OPTIONAL"),
        ));
        i += 1;
    }

    let mut parquet_field: HashMap<String, String> = HashMap::new();
    parquet_field.insert(String::from("type"), String::from("group"));
    parquet_field.insert(String::from("repeat"), repetition);
    parquet_field.insert(String::from("name"), field_name);
    parquet_field.insert(String::from("schema"), convert_fields(union_types));
    serde_json::to_string(&parquet_field).unwrap()
}