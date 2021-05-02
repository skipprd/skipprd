<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 07/08/2019
 * Time: 20:49
 */

namespace Skipprd\Traits;

use Carbon\Carbon;
use Skipprd\Str;
use Skipprd\Helpers;

trait AnalyseSchema
{

    /**
     * @var int The count of processed messages
     */
    public $i = 0;

//    protected $discoveredFieldOccurrence = [];

    public $dateFieldvalidationMminSample = 100;

    static $dataTypeMasks = [
        'boolean' => false,
        'integer' => 0,
        'long' => 0,
        'double' => '0.0', # ioncube segfault with float, so have to use string
        'string' => '',
        'keyword' => '',
        'NULL' => null,
////        'record' => [],
////        'array' => [],
////        'map' => [],
        'date' => '01-01-1970',
        'timestamp' => 0,
        'timestamp_milli' => 0,
        'seconds' => 0,
    ];

    static $dataTypeDrops = [
        'boolean' => null,
        'integer' => null,
        'long' => null,
        'double' => null,
        'string' => null,
//        'keyword' => null,
        'NULL' => null,
//        'record' => [],
//        'array' => [],
//        'map' => [],
        'date' => null,
        'timestamp' => null,
        'timestamp_milli' => null,
        'seconds' => null,
    ];

    static $dataTypeCasts = [
        'array' => [],
        'map' => [],
        'record' => [],
        'timestamp' => [
            'integer',
//            'long',
            'double',
//            'string',
//            'boolean'
        ],
        'timestamp_milli' => [
//            'integer',
            'long',
            'double',
//            'string',
//            'boolean'
        ],
        'date' => [
            'string'
        ],
        'integer' => [
//            'long',
            'double',
//            'string',
//            'boolean'
        ],
        'long' => [
//            'integer',
            'double',
//            'string',
//            'boolean'
        ],
        'double' => [
            'integer',
            'long',
//            'string',
//            'boolean'
        ],
        'boolean' => [
//            'integer',
//            'long',
//            'double',
//            'string',
        ],
        'string' => [
            'integer',
            'long',
            'double',
            'boolean',
        ],
//        'seconds' => [
//        ]
    ];

    public function analysePayload(array $message, array &$metadata)
    {
        
        $this->i++;

        foreach ($message as $field => $value) {

            $this->analyseField($field, $value,$metadata);

        }

    }

    public function analyseField($field, $value, &$fieldOccurrence)
    {
        $field = Helpers::cleanFieldName($field);

        if (isset(Config::$specialFields[$field])) {
            // we've already declared this in our default mapping.
//            continue;
            return;
        }

        $this->initDiscoveredType($fieldOccurrence, $field);

        // Build mapping/Schema
        if ($fieldOccurrence[$field]['count'] < $this->minSample) {

            $fieldOccurrence[$field]['count']++;

            $this->resolveFieldType($fieldOccurrence, $field, $value);

//                                    $dataType = $this->getLogicalType($field, $value);
//                                    $this->setDiscoveredOccurrence($field, $dataType, $value);

//                                    if ($fieldOccurrence[$field]['count'] >= $this->minSample) {
//
//                                        // @todo - improve performance by passing specific field
//                                        $this->determineFieldTypes($field);
//
//
//                                    }

        }

        if (is_array($value) && !empty($value)) {
            foreach ($value as $sub_field => $sub_value) {

                $this->analyseField($sub_field, $sub_value, $fieldOccurrence[$field]['fields']);

            }
        }

        // Mapping/Schema known, enforce it
//                                if (empty($fieldOccurrence[$field])) {
//                                    continue;
//                                }


    }

    public function resolveFieldType(&$array, $field, $value, $parentType = null)
    {
        $dataType = $this->getLogicalType($field, $value);

        $this->initDiscoveredType($array, $field);

        if ($dataType == 'array') {

            $typeCount = [];

            $isSequential = Helpers::isSequentialArrayKeys($value);

            foreach ($value as $sub_field => $sub_value) {

                $logicalType = $this->getLogicalType($sub_field, $sub_value);

                $typeCount[$logicalType] = 'hit';

                // @todo - check if types are castable to same primitive, then could be array
                // e.g. [1,2,3] may discover as schema [bool, int, int] and therefore
                // parent field resolve type as `record`.
                // When in fact we'd want to discover schema as [int, int int] and
                // parent field resolve as `array`.
            }

            // Multiple type within array values?
            // Must be a record then.
            if (count($typeCount) > 1) {

                $dataType = 'record';

                // Array of Arrays? Use a Record for the parent.
            } elseif (array_key_exists('array', $typeCount)) {
//                } elseif (array_key_exists('array', $array[$sub_field]['type'])) {
                $dataType = 'record';
//                    $dataType = 'map';

            } elseif ($isSequential) {
                 // array of sequential int keys is an avro array
                $dataType = 'array';

            } elseif (!$isSequential) {
                // associative array is an avro map
                $dataType = 'map';
            }
        }

        $this->setDiscoveredOccurrence($array, $field, $dataType, $value);

        return $dataType;
    }

    public function getLogicalType(string $field, $value, bool $allowDate = true)
    {

        $dataType = gettype($value);

        if ($dataType == 'string' || $dataType == 'integer' || $dataType == 'double') {

            // String really an int?
            $dataType = AnalyseSchema::checkStringOrInt($value);

            if ($allowDate) {

                $validTimestamp = false;

                if ($dataType == 'integer') {
                    $validTimestamp = AnalyseSchema::isValidTimeStamp($value);
//                $dataType = 'timestamp';
//
                } elseif ($dataType == 'long') {
                    $validTimestamp = AnalyseSchema::isValidTimeStamp($value / 1000);
//                $dataType = 'timestamp_milli';
//
                }

                if ($validTimestamp) {

                    $this->setDateFieldCandidate($field);

                    $this->incrementDateFieldCandidateCount($field);

                }

            }

//            if (is_float($value + 0) && (float) $value == $value) {
            if (AnalyseSchema::isFloat($value)) {
                if (filter_var($value, FILTER_VALIDATE_FLOAT)) {
                    $dataType = 'double';
                }
            }

//            // seconds?
//            // @todo - configurable delimiter
//            $fieldHaystack = $this->explodeField($field);
//            $haystack = [
//                'idle',
//                'session',
//                'duration',
//                'time',
//                'second',
//                'seconds'
//            ];
//            $matches = array_intersect($fieldHaystack, $haystack);
//
//            if (!empty($matches)) {
//                $dataType = 'seconds';
//            }
//
//            // category
//            $fieldHaystack = $this->explodeField($field);
//            $haystack = ['status', 'code', 'tag'];
//            $matches = array_intersect($fieldHaystack, $haystack);
//
//            if (!empty($matches)) {
//                $dataType = 'keyword';
//            }
        }

        if ($dataType == 'string' && $allowDate) {

            // Limit number of check type attempts for data as expensive operation.
            if (empty(Config::$dateFieldCandidates[$field]['check_count']) || Config::$dateFieldCandidates[$field]['check_count'] < $this->dateFieldvalidationMminSample) {


                if ($format = AnalyseSchema::isValidDate($value)) {
                    $dataType = 'date';
                    $this->setDateFieldCandidate($field, $format);
                }

                $this->incrementDateFieldCandidateCount($field);

                // Already hit date field check limit. Force set type if valid date field.
            } elseif (!empty(Config::$dateFieldCandidates[$field]['valid_count'])
                && Config::$dateFieldCandidates[$field]['valid_count'] >= $this->dateFieldvalidationMminSample) {
                $dataType = 'date';

            }

        }

        if (filter_var($value, FILTER_VALIDATE_BOOLEAN)) {
            $dataType = 'boolean';
        }

        // @todo - logical interpretation based on field name

        return $dataType;

    }

    /**
     * @param $value
     * @param $dataType
     * @return string
     */
    static function checkStringOrInt($value)
    {

        $dataType = gettype($value);

        // @todo - I think is_float is an alias of is_numeric
        if (is_numeric($value) && (int) $value == $value) {
            if (filter_var($value, FILTER_VALIDATE_INT,
                ['min_range' => PHP_INT_MIN, 'max_range' => PHP_INT_MAX])) {

                if (self::is32bitSignedInt($value)) {
                    $dataType = 'integer';

                } elseif (self::is64bitSignedInt($value)) {
                    $dataType = 'long';
                }
            }
        }

        return $dataType;
    }

    function incrementDateFieldCandidateCount(string $field)
    {

        if (empty(Config::$dateFieldCandidates[$field]['check_count'])) {
            Config::$dateFieldCandidates[$field]['check_count'] = 1;
        } else {
            Config::$dateFieldCandidates[$field]['check_count']++;
        }

    }

    function setDateFieldCandidate(string $field, string $format = '')
    {

        if (empty(Config::$dateFieldCandidates[$field]['valid_count'])) {
            Config::$dateFieldCandidates[$field]['valid_count'] = 1;
        } else {
            Config::$dateFieldCandidates[$field]['valid_count']++;
        }

        if (Config::$dateFieldCandidates[$field]['valid_count'] >= $this->dateFieldvalidationMminSample) {

            Config::$dateFieldCandidates[$field]['field'] = $field;

            // save the format, else calls to setValue() often hit Carbon::createFromFormat causing memory explosion
            if ($format != '') {
                Config::$dateFieldCandidates[$field]['format'] = $format;
            }
        }

    }

    static public function isFloat($test) {

        if (!is_scalar($test)) {return false;}

        $type = gettype($test);

        if ($type === "double") {
            return true;
        } else {
            return preg_match("/^\\d+\\.\\d+$/", $test) === 1;
        }

    }

    static function is32bitSignedInt($value)
    {

        $value = intval($value);
        
        (int) @$value += 0; // handle leading zero

        $options = ['min_range' => -2147483647, 'max_range' => 2147483647];

        return false !== filter_var($value, FILTER_VALIDATE_INT,
            compact('options'));
    }

    static function is64bitSignedInt($value)
    {

        $value = intval($value);

        (int) @$value += 0; // handle leading zero

        $options = [
            'min_range' => -9223372036854775807,
            'max_range' => 9223372036854775807
        ];

        return false !== filter_var($value, FILTER_VALIDATE_INT,
            compact('options'));
    }

    static function isValidTimeStamp($timestamp)
    {
        if (is_numeric($timestamp) && strtotime(date('d-m-Y H:i:s',
                $timestamp)) === (int) $timestamp
        ) {

            $date = strtotime(date('d-m-Y H:i:s', $timestamp));

            if ($date >= strtotime('1970-01-01') && $date <= strtotime('+20 years')) {
                return $timestamp;
            }

        } else {
            return false;
        }
    }

    static function isValidDate($value)
    {

        $validFormats = [
            DATE_ATOM,
            "Y-m-d\TH:i:s.vP",
            "Y-m-d\TH:i:s.uP",
//            DATE_RFC3339_EXTENDED,
            DATE_COOKIE,
            DATE_ISO8601,
            DATE_RFC822,
            DATE_RFC850,
            DATE_RFC1036,
            DATE_RFC1123,
            DATE_RFC2822,
            DATE_RFC3339,
            DATE_RSS,
            DATE_W3C,
            'Y-m-d\'T\'H:i:s.uZ', // Microseconds
            'Y-m-d\'T\'H:i:s.vZ', // Milliseconds
            'Y-m-d\'T\'H:i:s+Z',
            'Y-m-d\'T\'H:i:sZ',
            'Y-m-d\'T\'H:i:s',
            'Y-m-d\'T\'H:i',
            'Y-m-d\'T\'H',
            'Y-m-d H:i:s',
            'Y-m-d H:i',
            'Y-m-d H',
            'Y-m-d',
            'd-m-Y H:i:s',
            'd-m-Y H:i',
            'd-m-Y H',
            'd-m-Y',
            'd-m-yy'
        ];

        foreach ($validFormats as $format) {
            try {

                $return = Carbon::createFromFormat($format, $value);

                if ($return !== false) {
                    $return = null; // prevent memory bomb
                    return $format;
                }
            } catch (\Exception $e) {
                $return = null; // prevent memory bomb
            }

            $return = null;
        }

        return false;

    }

    public function applyEvolutionFactory(
        &$field,
        $value,
        $evolution,
        string &$dataType = '',
        string $newValue = ''
    ) {

        switch ($evolution) {

            case 'cast':
                $dataType = $newValue;
//                $value = $this->setValue($newValue, $field, $value);
                break;
            case 'new':
                $field = $newValue;
                break;
            case 'rename':
                $field = $newValue;
//                $value = $this->setValue($dataType, $field, $value);
                break;
            case 'merge':
                $field = $newValue;
                break;
//            case 'transform':
//                $transformation = $newValue;
//
//                $this->applyTransformationFactory($field, $value, $transformation, $dataType);
//                break;
            case 'default':
                break;

        }

    }

    public function handleValueError(&$field, $value, $fieldOccurrence)
    {

        $dataType = $this->getLogicalType($field, $value, false);


//        Registry::skipprd()->debug("Resolving type: $dataType for field: $field value: $value");

//        // Evolution
        if ( !empty($fieldOccurrence[$field]['evolution'][$dataType]['new_value']) ) {
            $evolution = $fieldOccurrence[$field]['evolution'][$dataType]['type'];
            $newValue = $fieldOccurrence[$field]['evolution'][$dataType]['new_value'];

//            Registry::skipprd()->debug("Resolving with: $evolution to $newValue");

            $this->applyEvolutionFactory($field, $value, $evolution, $dataType, $newValue);

        }
//
//        return $value;

    }

    public function initDiscoveredType(&$array, $field)
    {

        if (empty($array[$field]['count'])) {
            $array[$field]['count'] = 1;
            $array[$field]['type'] = [];
            $array[$field]['parent_type'] = '';
            $array[$field]['fields'] = [];
            $array[$field]['evolution'] = [];
        }
    }

    public function setDiscoveredOccurrence(&$array, $field, $dataType, $value)
    {

        # handy to display example value to user
        $array[$field]['last_value'] = ($dataType != 'array') ? '' : $value;


        if (empty($array[$field]['type'][$dataType])) {
            $array[$field]['type'][$dataType] = 1;
            $array[$field]['evolution'][$dataType]['type'] = '';
            $array[$field]['evolution'][$dataType]['new_value'] = '';
            $array[$field]['evolution'][$dataType]['sample'] = $value;
            $array[$field]['evolution'][$dataType]['solved'] = false;
        } else {
            $array[$field]['type'][$dataType]++;

        }

        // Timestamps possibly just plain old ints/longs
//        if ($dataType == 'timestamp') {
//            $this->setDiscoveredOccurrence($array, $field, 'integer', $value);
//        } elseif ($dataType == 'timestamp_milli') {
//            $this->setDiscoveredOccurrence($array, $field, 'long', $value);
//        }
        if ($dataType == 'integer') {
            $validTimestamp = $this->isValidTimeStamp($value);
            if ($validTimestamp) {
                $this->setDiscoveredOccurrence($array, $field, 'timestamp', $value);
            }
        } elseif ($dataType == 'long') {
            $validTimestamp = $this->isValidTimeStamp($value / 1000);
            if ($validTimestamp) {
                $this->setDiscoveredOccurrence($array, $field, 'timestamp_milli', $value);
            }

        }

//        $types = Config::$discoveredFieldOccurrence = array_pull(Config::$discoveredFieldOccurrence, "$field.type");
//        if (empty($types[$dataType])) {
//            $types[$dataType] = 1;
//        } else {
//            $types[$dataType]++;
//        }
//        Config::$discoveredFieldOccurrence = array_add(Config::$discoveredFieldOccurrence, "$field.type", $types);

    }

}
