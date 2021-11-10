<?php


namespace Skipprd\Converters;

class AvroHiveSchemaConverter implements SchemaConverterInterface
{

    public function convert($schema) : array
    {
        $columns = [];

        $mappings = [
            'map' => 'map',
            'array' => 'array',
            'record' => 'struct',
            'long' => 'bigint',
            'string' => 'string' // parquet is binary but athena fails with binary and works with string?
        ];

        foreach ($schema as $field) {
            // nulls don't have type at pos 1
            // array at pos 1 means complex type
            if (!empty($field['type'][1]) && is_array($field['type'][1])) {
                // record
                if ($field['type'][1]['type'] == 'record') {
                    $structCols = $this->convert($field['type'][1]['fields']);

                    $typeStr = $mappings[$field['type'][1]['type']] . '<';

                    $types = [];
                    foreach ($structCols as $name => $col) {
                        $types[] = $col['Name'] . ':' . $col['Type'];
                    }

                    $typeStr .= implode(',', $types) . '>';

                    $columns[] = [
                        'Name' => $field['name'],
                        'Type' => $typeStr,
                    ];
                }

                // map
                if ($field['type'][1]['type'] == 'map') {
                    $fieldType = $mappings[$field['type'][1]['type']] ? $mappings[$field['type'][1]['type']] : $field['type'][1]['type'];
                    $valueType = isset($mappings[$field['type'][1]['values']]) ? $mappings[$field['type'][1]['values']] : $field['type'][1]['values'];

                    $typeStr = $fieldType . '<string,' . $valueType . '>';

                    $columns[] = [
                        'Name' => $field['name'],
                        'Type' => $typeStr,
                    ];
                }

                if ($field['type'][1]['type'] == 'array') {
                    $fieldType = $mappings[$field['type'][1]['type']] ? $mappings[$field['type'][1]['type']] : $field['type'][1]['type'];
                    $valueType = isset($mappings[$field['type'][1]['items']]) ? $mappings[$field['type'][1]['items']] : $field['type'][1]['items'];

                    $typeStr = $fieldType . '<' . $valueType . '>';

                    $columns[] = [
                        'Name' => $field['name'],
                        'Type' => $typeStr,
                    ];
                }
            } else {

                /**
                 * primitive types
                 */
                if (empty($field['type'][1])) { // null doesn't have type at pos 1
                    $parquetType = $field['type'][0];
                } elseif (!empty($mappings[$field['type'][1]])) {
                    $parquetType = $mappings[$field['type'][1]];
                } else {
                    $parquetType = $field['type'][1];
                }

                $columns[] = [
                    'Name' => $field['name'],
                    'Type' => $parquetType,
                ];
            }
        }

        return $columns;
    }
}
