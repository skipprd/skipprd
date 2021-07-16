<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 12/05/2020
 * Time: 12:27
 */

namespace Skipprd\Traits;


use Carbon\Carbon;
use \Exception;
use Monolog\Registry;
use Skipprd\Helpers;


trait Ingest
{

    public $flagMsgDeadLetter = false;

    public $avroSchema = null;

    public function ingestPayload(array $sourceMessage, array &$metadata, string $partition)
    {

        $this->i++;

        // @todo - configurable timefields
        if (!empty($sourceMessage)) {

            $message = $this->defaultMsgs[$partition];

            /*
             * Transformations and schema evolution
             */
            foreach ($sourceMessage as $field => $value) {

                if (!empty($metadata[$field]['enabled'])) { // only ingest fields enabled to sync to output

                    if ($this->i == 1) {
                        Registry::skipprd()->info("Ingesting field: $field");
                    }
                    $this->ingestField($field, $value, $metadata, $message);

                } elseif (isset(Config::$specialFields[$field])) {
                    $message[$field] = $value;

                }

            }


//            $message['skpr_partition'] = ($this->i %2) ? 'foo' : 'bar';




//            $carbon = Carbon::createFromTimestamp($message['skpr_event_ts']);
//            $message['skpr_event_ts'] = $carbon->timestamp;

            //                $record->payload = $message;

            //                            $this->entries[$key] = $record;


            if (!$this->flagMsgDeadLetter
                && $this->avroEncodeTest($message, $partition)
            ) {

                $this->entries++;

//                $this->entries[] = $message;

//                $this->serialiseOutput($message, $offset);
                return $message;


            } else {

                // Don't attempt to ingest records with new fields.
                // We must ensure the user selects a determined_type for the field first.
                // So just dead letter it for now.
//                $this->flagMsgDeadLetter = false;

//                $this->deadLetterMessage($message, $offset);

                return false;
            }


        } else {

            return false;
//            $this->deadLetterMessage($message);

//                                Registry::skipprd()->info("Message empty or could not emitArray, sending to dead letter queue.");
//                                Registry::skipprd()->debug($message);
//                        $this->deadLetters[] = $payload;
        }
    }

    public function ingestField($field, $value, &$metadata, &$message)
    {
        $field = Helpers::cleanFieldName($field);

//        Registry::skipprd()->debug($field);
//        Registry::skipprd()->debug($metadata[$field]);
//        exit(0);

        if (isset(Config::$specialFields[$field])) {
            $message[$field] = $value;
            
        }
        elseif (!empty($metadata[$field]['determined_type'])) {

            $dataType = $metadata[$field]['determined_type'];

            // No need to process the actual parent field, just its values
//            if ( !in_array($dataType, ['record', 'map']) ) {
            if ( !in_array($dataType, ['record']) ) {

                // @todo - getLogicalType performance is slow, avoid calling.

                //       - So only handle schema evolution on setValue failure
                //       - rather than proactively here.
//                if (count($metadata[$field]['evolution']) > 0) {
//
//                    $actualDataType = $this->getLogicalType($field, $value);
//
//                    // Evolution
//                    if (!empty($metadata[$field]['evolution'][$actualDataType]['new_value'])) {
//
//                        $evolution = $metadata[$field]['evolution'][$actualDataType]['type'];
//                        $newValue = $metadata[$field]['evolution'][$actualDataType]['new_value'] ?: '';
//                        $this->applyEvolutionFactory($field, $value, $evolution, $actualDataType, $newValue);
//                        // set actual datatype
//                        $dataType = $actualDataType;
//
//
//                    }
//                }


                // Transformation
                if (!empty($metadata[$field]['transform'])) {
                    $transformation = $metadata[$field]['transform'];

                    $this->applyTransformationFactory($field,
                        $value, $transformation, $dataType);
                }

                // $value will be cast or resolved by schema evolution
                // $field may be resolved by schema evolution rules to handle breaking changes
                //   - possible that we rename the field or merge it with an existing field
                $resolvedValue = $this->setValue($dataType, $field, $value, $metadata);

                // ignore if null, use default message which has correct null for data type
                if (!empty($resolvedValue)) {
                    $message[$field] = $resolvedValue;
                }

            }


        } else {
            // discover schema for new fields
            $dataType = $this->resolveFieldType($metadata, $field, $value);

            $this->flagMsgDeadLetter = true;

//            Registry::skipprd()->debug("dead letter");
        }

//        if (is_array($value) && !empty($value) && $dataType != 'array') {
        if (is_array($value) && !empty($value) && !in_array($dataType, ['array', 'map'])) {
            foreach ($value as $sub_field => $sub_value) {

                $sub_field = Helpers::cleanFieldName($sub_field);

                if (!empty($metadata[$field]['fields'][$sub_field]['enabled'])) { // only ingest fields enabled to sync to output

                    if ($this->i == 1) {
                        Registry::skipprd()->info("Ingesting field: $sub_field");
                    }

                    $this->ingestField($sub_field, $sub_value,$metadata[$field]['fields'],$message[$field]);
                }

            }
        }

    }

    /**
     * Message must contain ALL fields described in the schema
     * (to support some destinations like Parquet and Athena)
     * Fields are null by default
     * 
     * @param array $message
     * @return array
     */
    public function defaultMessage(array $schema = [])
    {

        try {

            // Init with internal special fields
            if (empty($schema)) {
                $message = Config::$specialFields;
            }

            foreach ($schema as $i => $field) {

                if (!empty($field['type'][1]['fields'])) {
                    $message[$field['name']] = $this->defaultMessage($field['type'][1]['fields']);
                } else {
                    if (!empty($field['type'][1]['type'])) {

                        if ($field['type'][1] == 'record') {
                            $message[$field['name']] = ['' => null];

                        } elseif ($field['type'][1]['type'] == 'array') {

                            $message[$field['name']] = [];

                        } elseif ($field['type'][1]['type'] == 'map') {

                            if ($field['type'][1]['values'] == 'string') {
                                $message[$field['name']] = ['' => ''];
                            }
                            if ($field['type'][1]['values'] == 'int') {
                                $message[$field['name']] = ['' => 0];
                            }


                        }

                    } else {
                        $message[$field['name']] = null;
                    }
                }
            }

        } catch (Exception $e) {
            Registry::skipprd()->error('Unable to build default message.');
            throw $e;
        }

        return $message;
    }

    public function applyTransformationFactory(
        &$field,
        &$value,
        $transformation,
        string $dataType,
        string $newValue = '',
        string $newField = ''
    ) {
        
        switch ($transformation) {

            case 'drop':
                if (array_key_exists($dataType, AnalyseSchema::$dataTypeDrops)) {
                    $value = AnalyseSchema::$dataTypeDrops[$dataType];
                }
                break;
            case 'mask':
                // may not exits, e.g. array, map, etc. For these we mash their sub items
                if (array_key_exists($dataType, AnalyseSchema::$dataTypeMasks)) {
                    $value = AnalyseSchema::$dataTypeMasks[$dataType];
                }
                break;
//            case 'random':
//                $value = Helpers::randomPassword(16);
//                break;
//            case 'rename':
//                $field = $newField;
//                break;
            case 'default':
                break;

        }

    }

    public function avroEncodeTest(array $record, string $partition)
    {

        try {
            $valid = \AvroSchema::is_valid_datum(Config::$avroSchemas[$partition], $record);

        } catch (\AvroSchemaParseException $e) {
            $valid = false;
        }

        return $valid;

    }

    /**
     * @param $dataType string - the expected data type of the field value
     * @param $field string - field name
     * @param $value string -  the actual field value
     * @return mixed|null - value data type on success or null on error
     */
    public function setValue($dataType, &$field, $value, $fieldOccurrence = [])
    {

        try {
            switch ($dataType) {

                case 'array':

                    if (is_array($value) && Helpers::isSequentialArrayKeys($value)) {

                        foreach ($value as $key => $val) {
                            $value[$key] = $this->setValue($fieldOccurrence[$field]['determined_type_values'], $key, $val);
                        }
                        return $value;

                    } else {
                        throw new \Exception();
                    }

                    break;

                case 'record':
                case 'map':
                    Helpers::cleanArrayFieldNames($value);

                    if (is_array($value)) {

                        foreach ($value as $key => $val) {
                            $value[$key] = $this->setValue($fieldOccurrence[$field]['determined_type_values'], $key, $val);
                        }

                        return $value;

                    } else {
                        throw new \Exception();
                    }

                    break;

                case 'string':

                    $value .= '';

                    if (gettype($value) == 'string') {
                        return $value;

                    } else {
                        throw new \Exception();
                    }

                    break;
                case 'timestamp':
                case 'timestamp_milli':

                    return Carbon::createFromTimestamp($value)->timestamp;

                    break;
                case 'date':

                    return Carbon::parse($value)->timestamp;

                    break;
                case 'int':
                case 'integer':

                    if (AnalyseSchema::is32bitSignedInt($value)) {
                        return (int) $value + 0; // force string to int

                    } else {
                        throw new \Exception();
                    }

                    break;
                case 'long':
                    $resp = null;

                    if (AnalyseSchema::is64bitSignedInt($value)) {
                        return (int) $value + 0; // force strings to long

                    } else {
                        throw new \Exception();
                    }

                    break;
                case 'double':

                    if (AnalyseSchema::isFloat($value)) {
                        return (float) floatval($value); // force strings to number

                    } else {

                        $valFloat = (float) sprintf("%.2f", $value);

                        if (AnalyseSchema::isFloat($valFloat)) {

                            return (float) $valFloat; // force strings to number

                        } else {
                            throw new \Exception();
                        }
                    }

                    break;
                case 'boolean':

                    if (filter_var($value,FILTER_VALIDATE_BOOLEAN)) {
                        return (bool) $value;

                    } else {
                        throw new \Exception();
                    }

                    break;
                default:
                    throw new \Exception();

            }

        } catch (\Exception $e) {
            $this->handleValueError($field, $value, $fieldOccurrence);
            return $value;

        }

    }
}