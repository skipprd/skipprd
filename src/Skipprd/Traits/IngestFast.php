<?php

namespace Skipprd\Traits;

use Skipprd\Helpers;

trait IngestFast
{

    public function fastPathIngest(array $unwrappedMessage, string $namespace): array {

        $message = $this->defaultMsgs[$namespace];

        foreach ($unwrappedMessage as $field => $value) {

            $field = Helpers::cleanFieldName($field);

            if (isset(Config::$specialFields[$field])) {
                $message[$field] = $value;
            } else {
                $resolvedValue = $this->fastSetValue(
                    Config::$discoveredFieldOccurrence[$namespace]['fields'][$field]['determined_type'],
                    $field,
                    $value,
                    Config::$discoveredFieldOccurrence[$namespace]['fields']
                );

                // ignore if null, use default message which has correct null for data type
                if ($resolvedValue !== null) {
                    $message[$field] = $resolvedValue;
                }
            }
        }

        $this->totalEntries++;
        $this->incrementNamespacesCount($namespace);
        $this->currentEntries++;
        $this->fastPath++;
        unset($unwrappedMessage);
//            $this->outputEmit($message);

        return $message;
    }

    /**
     * @param $dataType string - the expected data type of the field value
     * @param $field string - field name
     * @param $value string -  the actual field value
     * @return mixed|null - value data type on success or null on error
     */
    public function fastSetValue(string $dataType, string $field, $value, array $metadata = null)
    {

        try {
            if ($value !== null) {

                if ($dataType === 'record') {

                    foreach ($value as $sub_field => $sub_value) {

                        // only ingest fields enabled to sync to output
                        if ($metadata[$field]['fields'][$sub_field]['enabled'] == true) {

                            $clean_sub_field = Helpers::cleanFieldName($sub_field);

                            $newValue[$clean_sub_field] = $this->fastSetValue(
                                $metadata[$field]['fields'][$sub_field]['determined_type'],
                                $sub_field,
                                $sub_value,
                                $metadata[$field]['fields']
                            );
                        }
                    }

                    $value = $newValue;

                } else {
                    if ($dataType === 'map') {
                        foreach ($value as $key => $val) {
                            if ($val !== null) {
                                $value[$key] = $this->fastSetValue(
                                    $metadata[$field]['determined_type_values'],
                                    $key,
                                    $val,
                                );
                            }
                        }
                    } else {
                        if ($dataType === 'array') {
                            foreach ($value as $key => $val) {
                                if ($value !== null) {
                                    $value[$key] = $this->fastSetValue(
                                        $metadata[$field]['determined_type_values'],
                                        $key,
                                        $val
                                    );
                                }
                            }
                        } else {
                            if ($dataType === 'string' || $dataType === 'date') {
                                $value .= '';
                            } elseif ($dataType === 'timestamp' || $dataType === 'timestamp_milli') {
                                $value = (int) $value + 0;// force string to int
//            } elseif ($dataType === 'date') {
//                $value = (int) $value + 0; // force string to int
                            } elseif ($dataType === 'int' || $dataType === 'integer') {
                                $value = (int) $value + 0; // force string to int
                            } elseif ($dataType === 'long') {
                                $value = (int) $value + 0; // force strings to long
                            } elseif ($dataType === 'double') {
                                $value = (float) sprintf("%.2f", $value);
                            } elseif ($dataType === 'boolean') {
                                $value = (bool) $value;
                            }
                        }
                    }
                }
            }

            unset($dataType, $field, $metadata);

            return $value;

        } catch (\Exception $e) {
            SkipprLogger::info("Failed setting value $value of type $dataType for field $field");
            throw $e;
        }
        catch (\Error $e) {
            SkipprLogger::info("Failed setting value $value of type $dataType for field $field");
            throw $e;
        }
    }
}