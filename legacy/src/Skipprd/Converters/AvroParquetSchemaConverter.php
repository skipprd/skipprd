<?php


namespace legacy\src\Skipprd\Converters;

use function Skipprd\Converters\count;

/**
 * A bad attempt a porting https://github.com/apache/parquet-mr/blob/ee30b13bb5c3f6848c76641d3b93c9858e6746cb/parquet-avro/src/main/java/org/apache/parquet/avro/AvroSchemaConverter.java#L156
 */


/**
 * Class AvroParquetSchemaConverter
 * @package Skipprd\Converters
 */
class AvroParquetSchemaConverter implements SchemaConverterInterface
{

    /** This field is required (can not be null) and each record has exactly 1 value. */
    private const REQUIRED = 0;

    /** The field is optional (can be null) and each record has 0 or 1 values. */
    private const OPTIONAL = 1;

    /** The field is repeated and can contain 0 or more values */
    private const REPEATED = 2;


//    private const BOOLEAN = 'bool';
    private const BOOLEAN = 'boolean';
//    private const INT32 = 'int32';
//    private const INT64 = 'int64';
    private const INT32 = 'integer';
    private const INT64 = 'long';
    private const INT96 = 'int96';  // deprecated, only used by legacy implementations.
    private const FLOAT = 'float';
    private const DOUBLE = 'double';
    private const BYTE_ARRAY = 'string';
    private const FIXED_LEN_BYTE_ARRAY = 'string';

    public function convert($avroSchema): array
    {
        return $this->convertFields($avroSchema);
    }

    private function convertFields(\AvroSchema $avroSchema)
    {

        $types = [];

        foreach ($avroSchema->fields() as $field) {
            if ($field->type == \AvroSchema::NULL_TYPE) {
                continue; // Avro nulls are not encoded, unless they are null unions
            }

            $types[$field->attribute('name')] = $this->convertField(
                $field->attribute('name'),
                $field->type,
                self::OPTIONAL
            );
        }

        return $types;
//        types.add(convertField(field));
//    }
//    return types;
    }

    private function convertPrimitiveType(string $type): string
    {

        if ($type == \AvroSchema::BOOLEAN_TYPE) {
            return self::BOOLEAN;
        } elseif ($type == \AvroSchema::INT_TYPE) {
            return self::INT32;
        } elseif ($type == \AvroSchema::LONG_TYPE) {
            return self::INT64;
        } elseif ($type == \AvroSchema::FLOAT_TYPE) {
            return self::FLOAT;
        } elseif ($type == \AvroSchema::DOUBLE_TYPE) {
            return self::DOUBLE;
        } elseif ($type == \AvroSchema::BYTES_TYPE) {
            return self::BYTE_ARRAY; // @todo BINARY
        } elseif ($type == \AvroSchema::STRING_TYPE) {
//        if ($logicalType != null && $logicalType->name == $LogicalTypes.uuid().getName()) && writeParquetUUID) {
//            builder = Types.primitive(FIXED_LEN_BYTE_ARRAY, repetition)
//                .length(LogicalTypeAnnotation.UUIDLogicalTypeAnnotation.BYTES);
            if (false) {
                // @todo implement uuid
            } else {
                return self::BYTE_ARRAY; // @todo BINARY
            }
        }

        return false;
    }


    private function convertField(string $fieldName, \AvroSchema $schema, $repetition)
    {

        $parquetField = ['name' => $fieldName];

        $type = $schema->type();
        $logicalType = $schema->type();

        if ($parquetType = $this->convertPrimitiveType($type)) {
            $parquetField['type'] = $parquetType;
            $parquetField['repeat'] = $repetition;

        } elseif ($type == \AvroSchema::RECORD_SCHEMA) {
            $parquetField['type'] = 'group';
            $parquetField['repeat'] = $repetition;
//        $parquetField['schema'] = $this->convertField($schema->qualified_name(), $this->convertFields($schema), self::REQUIRED);
//        $parquetField['schema'] = $schema->fields();
            $parquetField['schema'] = $this->convertFields($schema);
        } elseif ($type == \AvroSchema::ENUM_SCHEMA) {
            $parquetField['type'] = 'group';
            $parquetField['repeat'] = $repetition;
            $parquetField['schema'] = $this->convertField(
                $schema->fullname(),
                $schema->symbols(),
                self::REQUIRED
            );
//        builder = Types.primitive(BINARY, repetition).as(enumType());
        } elseif ($type == \AvroSchema::ARRAY_SCHEMA) {
            $parquetField['type'] = 'group';
            $parquetField['repeat'] = $repetition;
//        $parquetField['name'] = $schema->name;
            $parquetField['annotation'] = 'LIST';

            $parquetField['schema']['type'] = 'group';
            $parquetField['schema']['repeat'] = self::REPEATED;
            $parquetField['schema']['name'] = 'list';

            // support list elements of primitive types and array of arrays
            $parquetField['schema']['schema'] = $this->convertField(
                'element',
                $schema->items(),
                self::REQUIRED
            );
        } elseif ($type == \AvroSchema::MAP_SCHEMA) {
            $parquetField['type'] = 'group';
            $parquetField['repeat'] = $repetition;
//        $parquetField['name'] = $schema->name;
            $parquetField['annotation'] = 'MAP';

            foreach ($schema->values() as $itemSchema) {
                $parquetField['schema']['type'] = 'group';
                $parquetField['schema']['repeat'] = $repetition;
                $parquetField['schema']['annotation'] = 'MAP_KEY_VALUE'; // map keys are always strings

                // key
//            $parquetField['schema']['schema']['repeat'] = $repetition;
                // avro map key type is always string
                $parquetField['schema']['schema']['key_type'] = 'string';
//            $parquetField['schema']['schema']['name'] = 'key';

                // value
                $parquetField['schema']['schema']['repeat'] = $repetition;
                $parquetField['schema']['schema']['value_type'] = $this->convertPrimitiveType($itemSchema);
                $parquetField['schema']['schema']['name'] = 'value';
            }
        } elseif ($type == \AvroSchema::FIXED_SCHEMA) {
//          $parquetField['type'] = self::FIXED_LEN_BYTE_ARRAY;
            $parquetField['type'] = self::BYTE_ARRAY;
            $parquetField['repeat'] = $repetition;
        } elseif ($type == \AvroSchema::UNION_SCHEMA) {
            return $this->convertUnion(
                $fieldName,
                $schema,
                $repetition
            );
//        return convertUnion(fieldName, schema, repetition);
        } else {
            throw new \AvroException("Cannot convert Avro type " . $type);
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

        return $parquetField;
    }

    public function convertUnion(string $fieldName, \AvroSchema $avroSchema, $repetition)
    {

        $nonNullSchemas = [];

        $foundNullSchema = false;

        // Found any schemas in the union? Required for the edge case, where the union contains only a single type.
        foreach ($avroSchema->schemas() as $subSchema) {
            if ($subSchema->type == \AvroSchema::NULL_TYPE) {
                $foundNullSchema = true;

                if (self::REQUIRED == $repetition) {
                    $repetition = self::OPTIONAL;
                }
            } else {
                $nonNullSchemas[] = $subSchema;
            }
        }

        switch (count($nonNullSchemas)) {
            case 0:
                // @todo - skippr internal fields can always be null, as may others
                //         consider if casting to string is a good idea?
//                throw new \UnexpectedValueException("Cannot convert Avro union of only nulls");
                $parquetField['type'] = self::BYTE_ARRAY;
                $parquetField['repeat'] = $repetition;
                return $parquetField;
            case 1:
                if ($foundNullSchema) {
                    return $this->convertField(
                        $fieldName,
                        $nonNullSchemas[0],
                        $repetition
                    );
                } else {
                    return $this->convertUnionToGroupType(
                        $fieldName,
                        $repetition,
                        $nonNullSchemas
                    );
                }

            default:
                return $this->convertUnionToGroupType(
                    $fieldName,
                    $repetition,
                    $nonNullSchemas
                );
        }
    }

    public function convertUnionToGroupType(string $fieldName, $repetition, array $nonNullSchemas)
    {

        $unionTypes = [];
        $i = 0;

        foreach ($nonNullSchemas as $subSchema) {
            $unionTypes[] = $this->convertField(
                "member" . $i++,
                $subSchema,
                self::OPTIONAL
            );
        }

        $parquetField['type'] = 'group';
        $parquetField['repeat'] = $repetition;
        $parquetField['name'] = $fieldName;
//      $parquetField['annotation'] = 'MAP';
//      $parquetField['fields'] = $unionTypes;
        $parquetField['schema'] = $this->convertFields($unionTypes);
    }
}
