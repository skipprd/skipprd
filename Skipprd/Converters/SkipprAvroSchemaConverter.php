<?php

namespace Skipprd\Converters;

class SkipprAvroSchemaConverter implements SchemaConverterInterface
{

    protected static $recordFieldNamesCount = [];

    protected static $avroTypeMappings = [
        'boolean' => ['default' => null, 'type' => ['null', 'boolean']],
        'integer' => ['default' => null, 'type' => ['null', 'int']],
        'long' => ['default' => null, 'type' => ['null', 'long']],
        'double' => ['default' => null, 'type' => ['null', 'double']],
        'string' => ['default' => null, 'type' => ['null', 'string']],
        'keyword' => ['default' => null, 'type' => ['null', 'string']],
        'NULL' => ['default' => null, 'type' => ['null']],
        'record' => [
            'default' => null,
            'type' => [
                'null',
                [
                    'type' => 'record',
                    'fields' => [],
                ],
            ],
        ],
        'array' => ['default' => null, 'type' => ['null', ['type' => 'array', 'items' => null]]],
        'map' => ['default' => null, 'type' => ['null', ['type' => 'map', 'values' => null]]],

//        'array' => ['default' => null, 'type' => ['null', 'array'], 'items' => [null, string|int,etc]],
//        'object' => [''],
//        'resource' => [''],
//        'NULL' => [''],
//        'unknown type' => [''],
//        'date' => ['type' => ['type' => 'string', 'logicalType' => 'timestamp-micros']],
//        'timestamp' => ['type' => ['type' => 'string', 'logicalType' => 'timestamp-micros']],
        'date' => ['default' => null, 'type' => ['null', 'long']],
        'timestamp' => ['default' => null, 'type' => ['null', 'int']],
        'timestamp_milli' => ['default' => null, 'type' => ['null', 'long']],
        'seconds' => ['default' => null, 'type' => ['null', 'int']],
//        'seconds' => [
//            'type' => 'nested',
//            'properties' => [
//                'seconds' => ['type' => 'long'],
//            ]
//        ],
//        'parent' => ['type' => 'object', 'properties' => []],
    ];

    public function convert($schema): array
    {

        $avroSchema = [];

        foreach ($schema as $field => $metaData) {

            if (!empty($metaData['determined_type'])) {

                // may create duplicates but that's handled on the Schema model

                self::buildAvroFields($avroSchema, $field,
                    $metaData['determined_type'], $schema, [],
                    $sub_field_count);
            }
        }

        return $avroSchema;
    }

    /**
     * Clean field name string to alpha numeric and underscores
     *
     * @param $field
     * @return string field
     */
    static function cleanFieldName($field)
    {
        if (is_numeric($field)) {
            $field = 'A' . $field;
        }

        $field = strtolower($field);

        $pattern = "/[^" . preg_quote('0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ',
                "/") . "]/";

        return preg_replace($pattern, "", $field);
    }

    static function buildAvroFields(array &$avroSchema, string $field, string $determined_type, array $skipprSchema, array $parent = [], &$fieldNamesCount = [])
    {

        // Build Sechema
        $type = self::$avroTypeMappings[$determined_type];

        $fieldCleanName = self::cleanFieldName($field);

        if (in_array($determined_type, ['map', 'array', 'record']) > 0) {

            if ($determined_type == 'array'
                && !empty($skipprSchema[$field]['determined_type_values'])) { // ignore empty arrays

                $valuesType = self::$avroTypeMappings[$skipprSchema[$field]['determined_type_values']];

                $type['type'][1]['items'] = $valuesType['type'][1];

                $avroSchema[] = array_merge(['name' => $fieldCleanName], $type);

            }

            if ($determined_type == 'map') {

                $sub_type = self::getComplexTypeScheme($skipprSchema, $field);

                if (!empty($sub_type)) { // not empty array in source data

                    if (!empty($skipprSchema[$field]['fields'])
                        && !empty($sub_type['type'][1]['type'])
                        && in_array($sub_type['type'][1]['type'],
                            ['map', 'array', 'record'])) {

//                    $type['type'][1]['name'] = $fieldCleanName;

                        $sub_field_count = [];

                        foreach ($skipprSchema[$field]['fields'] as $sub_field => $sub_field_meta) {

                            self::buildAvroFields($type['type'][1]['values'],
                                $sub_field, $sub_field_meta['determined_type'],
                                $skipprSchema[$field]['fields'], true, $sub_field_count);

                        }

                        $avroSchema[] = array_merge(['name' => $fieldCleanName],
                            $type);
//                    $newSchema = $type;

                    } else {

                        //////
//                    $sub_type = self::getComplexTypeScheme($skipprSchema, $field);
//                    $type['type'][1]['default'] = null;
                        $type['type'][1]['values'] = $sub_type['type'][1];
                        //////

                        if (!empty($parent)) { // has a parent field
                            $avroSchema = array_merge(['name' => $fieldCleanName],
                                $type);
                        } else {
                            $avroSchema[] = array_merge(['name' => $fieldCleanName],
                                $type);
                        }
                    }
                }
//                else {
//                    $newSchema[] = array_merge(['name' => $fieldCleanName], $type);
//                }

            }

            if ($determined_type == 'record') {

                // handle sub records with repeated field names
                // simply suffix an increment for field name in the schema
                if (isset(self::$recordFieldNamesCount[$fieldCleanName])) {

                    self::$recordFieldNamesCount[$fieldCleanName]++;

                    $type['type'][1]['name'] = $fieldCleanName . self::$recordFieldNamesCount[$fieldCleanName];

                } else {

                    self::$recordFieldNamesCount[$fieldCleanName] = 1;

                    $type['type'][1]['name'] = $fieldCleanName;

                }

                $sub_field_count = [];

                foreach ($skipprSchema[$field]['fields'] as $sub_field => $sub_field_meta) {

                    self::buildAvroFields($type['type'][1]['fields'], $sub_field, $sub_field_meta['determined_type'], $skipprSchema[$field]['fields'], [], $sub_field_count);

                }

                $avroSchema[] = array_merge(['name' => $fieldCleanName], $type);
//                $newSchema[] = array_merge(['name' => $field], $type['type'][1]);

            }

        } else {

            // Add field to schema

            // its possible for a field to be added via "new field" evolution.
            // these new fields will find there way to field_yml either because:
            // - vuejs adds to field_yml
            // - default message in ingest is created from the latest schema and
            //      consequently the mapping is updated at the end of the ingest job
            // It's therefore important to de-duplicate fields else they could be added
            //   via schema evolution AND field_yml


            // @todo - unless it's been renamed??
//            if ($skipprSchema[$field]['evolution'][$determined_type]['type'] != 'rename') {

            // de-duplicate
            if (isset($fieldNamesCount[$fieldCleanName])) {

                $fieldNamesCount[$fieldCleanName]++;
            } else {
                $fieldNamesCount[$fieldCleanName] = 1;
            }

            if ($fieldNamesCount[$fieldCleanName] == 1) {
                $avroSchema[] = array_merge(['name' => $fieldCleanName], $type);
            }

            // Evolution
            foreach ($skipprSchema[$field]['evolution'] as $dataType => $evolution) {

                if (in_array($evolution['type'], ['new', 'rename']) )  {

                    $new_field = self::cleanFieldName($evolution['new_value']);

                    // de-duplicate
                    if (isset($fieldNamesCount[$new_field])) {

                        $fieldNamesCount[$new_field]++;
                    } else {
                        $fieldNamesCount[$new_field] = 1;
                    }

                    if ($fieldNamesCount[$new_field] == 1) {

                        $typeSchema = self::$avroTypeMappings[$dataType];

                        $avroSchema[] = array_merge(['name' => $new_field], $typeSchema);

                    }
                }
            }
        }
    }

    static function getComplexTypeScheme($skipprSchema, $field) {

        $typeCandidates = [];
        $sub_type = [];

        foreach ($skipprSchema[$field]['fields'] as $sub_field => $sub_field_meta) {

            if (empty($typeCandidates[$sub_field_meta['determined_type']])) {
                $typeCandidates[$sub_field_meta['determined_type']] = 1;
            } else {
                $typeCandidates[$sub_field_meta['determined_type']]++;
            }
        }

        // may have to sub_type if empty array
        // e.g. { "tags": [] }
        if (!empty($typeCandidates)) {

            // found sub field types, return most common
            $typeCandidates = array_flip($typeCandidates);
            krsort($typeCandidates);
            $highestType = reset($typeCandidates);

            $sub_type = self::$avroTypeMappings[$highestType];
        }

        return $sub_type;

    }

}
