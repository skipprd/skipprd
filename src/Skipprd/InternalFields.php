<?php

namespace Skipprd;

use Carbon\Carbon;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

global $parseNamespaceCache;

class InternalFields
{

    protected static $parseNamespaceCache;
    /**
     * @param array $message - payload being ingested from the source system
     * @param string $partition - partition defined by the semantics of the source system.
     *                            For instance kinesis shard, kafka partition, etc.
     * @return string
     */
    public static function parsePartitionField(array &$message, string $partition = ''): string
    {

        // default to data source partition (table, topic, queue, file dir, etc)
        $partition =  Helpers::cleanFieldName($partition);

        // optional: partition by composite key
        if (Config::$partitionByFields) {
            $partition = '';
            
            foreach (Config::$partitionByFields as $entityFieldDot) {
                if ($entityValue = Arr::get($message, $entityFieldDot, false)) {

                    $cleanEntityFieldName = Helpers::cleanFieldName($entityFieldDot);
                    $cleanEntityFieldValue = Helpers::cleanFieldName($entityValue);
                        $partition .= '-' . $cleanEntityFieldName . '=' . $cleanEntityFieldValue;
                }
            }
        }

        $partition = strtolower(trim($partition, '-'));
        $partition = trim($partition, '-');

        $message['skpr_partition'] = $partition;

        return $partition;
    }

    public static function parseSourcePartition(string $partition)
    {
        $end = strpos($partition, '-');

        if ($end) {
            return substr($partition, 0, $end);
        } else {
            return $partition;
        }
    }

    /**
     * Not at all sure this is a good idea. Would allow users to split schemas containing
     * multiple event types into separate schemas and output indexes/tables.
     * Potentially powerful feature, but we use the namespace to track offsets and
     * could generally add real complexity.
     * @param $message
     */
    public static function parseNamespaceField(array &$message, string $namespace): string
    {

        $cleanNamespace = $namespace;

        if (!isset(self::$parseNamespaceCache[$namespace]) || self::$parseNamespaceCache[$namespace]) {

            // default to data source partition (table, topic, queue, file dir, etc)
            $cleanNamespace = Helpers::cleanFieldName($namespace);

            // optional: partition by composite key
            if (!empty(Config::$eventTypeFields)) {
                $namespaces = [];

                foreach (Config::$eventTypeFields as $entityFieldDot) {
                    if ($entityValue = Arr::get($message, $entityFieldDot, false)) {
                        $namespaces[] = Helpers::cleanFieldName($entityValue);
                    }
                }

                $cleanNamespace = implode('_', $namespaces);

                $cleanNamespace = strtolower(trim($cleanNamespace, '-'));
            }

        }

        if ($cleanNamespace !== $namespace) {
            self::$parseNamespaceCache[$namespace] = true;
        } else {
            self::$parseNamespaceCache[$namespace] = false;
        }

        $message['skpr_namespace'] = $cleanNamespace;

        return $cleanNamespace;
    }

    public static function parseSourceNamespace(string $namespace)
    {

        $end = strpos($namespace, '-');

        if ($end) {
            return substr($namespace, 0, $end);
        } else {
            return $namespace;
        }
    }

    public static function parseTimeField(array &$message): int
    {

        // default to beginning of epoch.
        $message['skpr_event_ts'] = 0;

        if (Config::$timeFields) {
            // Support nested time fields via array dot notation
            // For user confirmed event time fields, use the first one that matches
            foreach (Config::$timeFields as $field_dot) {


//                SkipprLogger::info("Looging for field $field_dot ");
//                if (isset($message[$field_dot])) {
//                    $time_value = $message[$field_dot];
//                    $message['skpr_event_ts'] = $time_value;
////                    SkipprLogger::info("Found time field $field_dot of value $time_value");
//                }

                if ($time_value = Arr::get($message, $field_dot, false)) {
//                    SkipprLogger::info("Found time field $field_dot of value $time_value");
                    $message['skpr_event_ts'] = $time_value;
                    break;
                }
            }

            // Handle millisecond timestamps
            if (strlen((string) $message['skpr_event_ts']) == 13) {
                $message['skpr_event_ts'] = floor($message['skpr_event_ts'] / 1000);
            }

            // Handle datetime strings
            if (gettype($message['skpr_event_ts']) == 'string') {
                $message['skpr_event_ts'] = Carbon::parse($message['skpr_event_ts'])->timestamp;
            }
        }

        return (int) $message['skpr_event_ts'];
    }
}
