<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 07/08/2019
 * Time: 20:49
 */

namespace Skipprd\Traits;

use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Helpers;

class Config
{

    use RecordFilter;

    // input an output plugin run modes
    public const RUN_MODE_SYNC = 'sync';
    public const RUN_MODE_VALIDATE_CONNECTION = 'validate_connection';
    public const RUN_MODE_VALIDATE_CONFIG = 'validate_config';
    public const RUN_MODE_SAVE = 'save';
    public const RUN_MODE_RESET_SOURCE_OFFSETS = 'reset_source_offsets';
    public const RUN_MODE_DELETE_PLUGIN = 'delete_plugin';
    // output plugins only
    public const RUN_MODE_CREATE_UPDATE_DEST_SCHEMA = 'sync_schema';
    public const RUN_MODE_DELETE_DEST_SCHEMA = 'delete_schema';

    public const RUN_MODE_VALIDATE_SCHEMA = 'validate_schema';

    public static $anonymousMetrics = true;

    public static $logLevel = 'INFO';

    public static $dataDir = '/data';

    public static $containerMem = '/data';

    public static $pipelineName = '';

    public static $pipelineId;

    public static $tenantId = '';

    public static $state = [];

    public static $taskId;

    public static $taskLogs = [];

    public static $exitCode;

    public static $syncMode = 'sync';

    /**
     * strict     - Ingest messages to output exactly as is with no attempt to cast to expected type
     *              or conform to schema. Won't discover new fields!
     *              Useful for replicating data across ENV/systems json -> json for instance or when in
     *              production and solving schema evolution in lower environments.
     */
    public const MUTABLE_MODE_STRICT = 'strict';

    /**
     * resolve    - Attempted to resolve data types to expected schema, by safe type casing
     *              and padding missing fields with nulls. Supporting conversion to formats such as Parquet
     *              and to ensure destination tables, datalakes, etc have complete rows.
     */
    public const MUTABLE_MODE_RESOLVE = 'resolve';

    /**
     * evolve     - As resolve, plus auto ingest any new fields and forward to the destination.
     */
    public const MUTABLE_MODE_EVOLVE = 'evolve';


    public static $mutableMode = self::MUTABLE_MODE_STRICT | self::MUTABLE_MODE_RESOLVE | self::MUTABLE_MODE_EVOLVE;

    public static $runMode = self::RUN_MODE_SYNC;

    public static $offsets = [];

    public static $sourceFormat = null;

    public static $outputFormat = null;

    public static $analysing = true;

    public static $minDiscoveryRecords = 10000;

    public static $maxDiscoverySeconds = 600;

    public static $idFields = [];

    public static $dateFieldCandidates = [];

    public static $discoveredFieldOccurrence = [];

    public static $schema = [];

    public static $filters = false;

    public static $flushBufferBytes = 250000;

    public static $flushBufferSeconds = 60;

    public static $flushBufferRecords = 1000;

    public static $eventTimeBucketDurationSeconds = false;

    public static $pollIntervalSeconds = null;

    /**
     * @var \AvroSchema $avroSchemas
     */
    public static $avroSchemas;

    public static $outputSchemas = [];

    public static $partitionByFields = [];

    public static $eventTypeFields = false;

    public static $eventPath = false;

    public static $flattenEvents = false;

    public static $timeFields = [];

    public static $systemUserApiToken = '';

    public static $enableDeadLetters = true;

    protected static $configUpdatedTime = 0;

    public static $specialFields = [
        'source_namespace' => '',
        'source_partition' => '',
        'skpr_event_ts' => 0,
        'skpr_namespace' => '',
        'skpr_partition' => '',
    ];

    static $specialFieldsMapping = [
        [
            'name' => 'source_namespace',
            'default' => null,
            'type' => ['null', 'string']
        ],
        [
            'name' => 'source_partition',
            'default' => null,
            'type' => ['null', 'string']
        ],
        [
            'name' => 'skpr_event_ts',
            'default' => null,
            'type' => ['null', 'int']
        ],
        [
            'name' => 'skpr_namespace',
            'default' => null,
            'type' => ['null', 'string']
        ],
        [
            'name' => 'skpr_partition',
            'default' => null,
            'type' => ['null', 'string']
        ],
    ];

    public static $batchFormats = [
        'parquet',
        'csv',
        'xml',
        'avro_file'
    ];

    public static function getenv(string $name, $default = '')
    {

        return (!empty(getenv($name))) ? getenv($name) : $default;
    }

    public static function getPipelineName()
    {

        $inputPluginName = Helpers::cleanFieldName(Config::getenv('DATA_SOURCE_PLUGIN_NAME'));
        $outputPluginName = Helpers::cleanFieldName(Config::getenv('DATA_OUTPUT_PLUGIN_NAME'));
        $defaultPipelineName = $inputPluginName . 'to' . $outputPluginName;
        $defaultPipelineName = Config::getenv(
            'PIPELINE_NAME',
            $defaultPipelineName
        );

        return $defaultPipelineName;
    }

    public static function getConfig()
    {
        self::$pipelineId = self::getenv('PIPELINE_ID');

        self::$logLevel = Config::getenv('LOG_LEVEL', 'INFO');

        self::$containerMem = Config::getenv('MEM', 1024); // convert to bytes
        self::$containerMem = self::$containerMem * 0.8; // allow some overhead
        ini_set('memory_limit', self::$containerMem . 'M');

        self::$flushBufferBytes = Config::getenv('DATA_OUTPUT_FLUSH_BYTES', self::$flushBufferBytes);
        self::$flushBufferSeconds = Config::getenv('DATA_OUTPUT_FLUSH_SECONDS', self::$flushBufferSeconds);
        self::$flushBufferRecords = Config::getenv('DATA_OUTPUT_FLUSH_RECORDS', self::$flushBufferRecords);

        self::$eventTimeBucketDurationSeconds = Config::getenv('DATA_OUTPUT_TIME_BUCKET', false);

        self::$pollIntervalSeconds = Config::getenv('DATA_SOURCE_POLL_INTERVAL_SECONDS', self::$pollIntervalSeconds);

        self::$mutableMode = Config::getenv('DATA_SOURCE_MUTABLE_MODE', self::MUTABLE_MODE_RESOLVE);

        if (self::$mutableMode == self::MUTABLE_MODE_STRICT) {
            SkipprLogger::info('Strict mutable mode enabled, will sync an exact copy of records.');
        }

        self::$runMode = Config::getenv('RUN_MODE', self::$runMode);

        self::$flattenEvents = Config::getenv('DATA_SOURCE_FLATTEN_EVENTS', self::$flattenEvents);

        self::$taskId = Config::getenv('TASK_ID');
            
        $avroArr = [];
//        self::$mapping = [];
        self::$discoveredFieldOccurrence = [];

        self::$anonymousMetrics = Config::getenv('ANONYMOUS_METRICS', true);

        self::$pipelineName = Config::getPipelineName();

        Config::$state['tenant_id'] = Helpers::randomStr(16);
        self::$tenantId = self::getenv(
            'TENANT_ID',
            Config::$state['tenant_id']
        );

        $dataDir = self::getenv('DATA_DIR');
        self::$dataDir = (empty($dataDir)) ? self::$dataDir : $dataDir;
        @mkdir(self::$dataDir);

        $uri = self::getenv('SKIPPR_API_ENDPOINT');

        if (!empty($uri)) {
            SkipprLogger::info("Skippr API endpoint configured to $uri");
            
            // Get Mapping
            try {
                SkipprLogger::info('Requesting config for pipeline ' . self::$pipelineName . ' from Skippr API');

                $path = 'ingest-job/get-mapping/' . self::$pipelineId;

                $client = new \GuzzleHttp\Client([
                    'base_uri' => $uri,
                    'headers' => [
                        'Authorization' => "Bearer " . self::getenv('SKIPPR_API_TOKEN')
                    ]
                ]);

                $body = $client->get($path)->getBody();

                $mapping = json_decode($body, true);

                self::$discoveredFieldOccurrence = $mapping;
            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }

            // Get Schema
//            try {
//                SkipprLogger::info('Looking up schema for pipeline ' . $defaultPipelineName);
//
//                $schemaName = self::$tenantId . '_' . self::$pipelineId . '-value';
//                $url = "http://$uri/";
//                $path = 'subjects/' . $schemaName . '/versions/latest';
//
//                $client = new \GuzzleHttp\Client([
//                    'base_uri' => $url,
//                    'headers' => [
//                        'Authorization' => "Bearer " . Config::getenv('SKIPPR_API_TOKEN')
//                    ]
//                ]);
//
//                $resp = json_decode($client->get($path)
//                    ->getBody()
//                    ->getContents(), true);
//
//                $avroArr = json_decode($resp['schema'], true);
//            } catch (\Exception $e) {
//                SkipprLogger::error($e->getMessage());
//            }
        } else {
            if (file_exists(self::$dataDir . '/skippr-state.json')) {
                try {
                    SkipprLogger::info('Found existing ' . self::$dataDir . '/skippr-state.json');

                    self::$state = json_decode(
                        file_get_contents(self::$dataDir . '/skippr-state.json'),
                        true
                    );

                    if (!empty(Config::$state[self::$pipelineName])) {
                        SkipprLogger::info('Loading state for pipeline ' . self::$pipelineName);

                        self::$discoveredFieldOccurrence = Config::$state[self::$pipelineName]['mapping'];

//        Config::$discoveredFieldOccurrence = (empty($configYml['field_yml'])) ? [] : $configYml['field_yml'];
                    }
                } catch (\Exception $e) {
                    SkipprLogger::error($e->getMessage());
                }
            }
        }

//        self::$offsets = (!empty(self::$state[$defaultPipelineName]['offsets']) ? self::$state[$defaultPipelineName]['offsets'] : []);

//        self::$schema['fields'] = [];
//
//        self::$schema['fields'] = (empty($avroArr)) ? [] : $avroArr;

        self::$eventPath = self::getenv('DATA_SOURCE_EVENT_PATH');

        self::$sourceFormat = self::getenv('DATA_SOURCE_FORMAT', '');
        self::$outputFormat = self::getenv('DATA_OUTPUT_FORMAT', 'json');

        $partitionFields = self::getenv('DATA_OUTPUT_PARTITION_BY_FIELDS', null);
        if (!empty($partitionFields)) {
            self::$partitionByFields = explode(',', $partitionFields);
        }

        $eventTypeFields = self::getenv('DATA_SOURCE_EVENT_TYPE_FIELDS', null);
        if (!empty($eventTypeFields)) {
            self::$eventTypeFields = explode(',', $eventTypeFields);
        }

        $timeFields = self::getenv('DATA_OUTPUT_TIME_FIELDS', null);
        if (!empty($timeFields)) {
            self::$timeFields = explode(',', $timeFields);
        }

        self::$analysing = (empty(self::$discoveredFieldOccurrence)) ? true : false;
        self::$analysing = (bool) self::getenv('ANALYSING', self::$analysing);

        self::$systemUserApiToken = self::getenv('SKIPPR_API_TOKEN');

        if (!empty(self::$discoveredFieldOccurrence)) {
            foreach (self::$discoveredFieldOccurrence as $namespace => $mapping) {
                SkipprLogger::info("Building $namespace schema");

                $converter = new SkipprAvroSchemaConverter();
                self::$schema[$namespace] = $converter->convert($mapping['fields']);

                self::$schema[$namespace] = self::schemaMerge(
                    self::$specialFieldsMapping,
                    self::$schema[$namespace]
                );

                self::$avroSchemas[$namespace] = self::buildAvroSchema(self::$schema[$namespace]);

//                $converter = new AvroParquetSchemaConverter();
//                self::$outputSchemas[$namespace] = $converter->convert(Config::$avroSchemas[$namespace]);

                $outputFormat = ucfirst(self::$outputFormat);
                $converterClass = 'Skipprd\Converters\Avro' . $outputFormat . 'SchemaConverter';

                if (class_exists($converterClass)) {
                    SkipprLogger::info("Generating $namespace $outputFormat schema");

                    $converter = new $converterClass();

                    self::$outputSchemas[$namespace] = $converter->convert(self::$avroSchemas[$namespace]);
                } else {
                    self::$outputSchemas[$namespace] = self::$avroSchemas[$namespace];
                }
            }
        }

        RecordFilter::initFilters(self::$filters);

        // Although we may be done analysing, we don't want to override candidate.
        // They should remain in the option list even if the user has rejected them.
//        if (!empty($configYml['field_yml']['date_field_candidates'])) {
//            Config::$dateFieldCandidates = $configYml['field_yml']['date_field_candidates'];
//        }
//
//        if (!empty($configYml['field_yml']['date_field_candidates'])) {
//            Config::$idFields = $configYml['field_yml']['enitity_field_candidates'];
//        }
    }


    public static function buildAvroSchema($schema)
    {

        $schemaName = self::$tenantId . '_' . self::$pipelineName;

        $schemaNamespace = "io.skippr." . self::$tenantId . "." . self::$pipelineName;

        $valueAvroSchema['namespace'] = $schemaNamespace;
        $valueAvroSchema['name'] = $schemaName;
        $valueAvroSchema['type'] = 'record';

        $valueAvroSchema['fields'] = $schema;

        $valueSchemaJson = json_encode($valueAvroSchema);
        $valueSchema = \AvroSchema::parse($valueSchemaJson);

        return $valueSchema;
    }

    public static function schemaMerge($existingSchema, $newSchema)
    {
        $existingSearchSchema = [];

        if (!empty($existingSchema)) {
            foreach ($existingSchema as $i => $entry) {
                $existingSearchSchema[$entry['name']]['index'] = $i;
                $existingSearchSchema[$entry['name']]['schema'] = $entry;
            }
        } else {
            $existingSchema = []; // ensure array as empty db field is string
        }

        if (!empty($newSchema)) {
            foreach ($newSchema as $i => $entry) {
                if (!empty($existingSearchSchema[$entry['name']])) { // update existing schema at array index
                    $existingSearchSchema[$entry['name']]['schema'] = $entry;
                } else { // add new schema field
                    $existingSearchSchema[$entry['name']]['index'] = $i;
                    $existingSearchSchema[$entry['name']]['schema'] = $entry;
                }
            }
        }

        $parsedSchema = [];

        // having enforced unique schema fields above, compile into schema data model
        foreach ($existingSearchSchema as $field => $entry) {
            $parsedSchema[] = $entry['schema'];
        }

        return $parsedSchema;
    }

    public static function setConfig()
    {

        $configYml = [];


//        $fieldsYml = Config::$discoveredFieldOccurrence;
//
//        $schemaArr = [];
//
//        if (!empty(self::$schema['fields'])) { // empty when first analysing
//            ksort(self::$schema['fields']);
//
//            $schemaArr = self::schemaMerge(self::$specialFieldsMapping, self::$schema['fields']);
//
//        }
//
//        $entitiesYml = Config::$entityNames;
//
//        $timeFieldsYml = Config::$timeFields;
//
//        $configYml = [
//            'field_yml' => $fieldsYml,
//            'schema_yml' => $schemaArr,
//            'event_path' => Config::$eventPath,
//            'serder' => Config::$serder,
//            'entities_yml' => $entitiesYml,
//            'time_fields_yml' => $timeFieldsYml,
//            'config_updated' => $this->configUpdatedTime,
//            'tenant_id' => Config::$tenantId ,
//            'source' => 'client',
//            'pipeline_id' => $this->pipelineId,
//            'pipeline_name' => Config::$pipelineName,
//            'worker_id' => $this->workerId,
//            'analysing' => Config::$analysing,
//        ];


        $uri = self::getenv('SKIPPR_API_ENDPOINT');

        if (!empty($uri)) {
            try {
                $path = 'ingest-job/update-mapping';

                $client = new \GuzzleHttp\Client([
                    'base_uri' => $uri,
                    'headers' => [
                        'Authorization' => "Bearer " . self::getenv('SKIPPR_API_TOKEN')
                    ]
                ]);

                $data = [
                    'id' => self::$pipelineId,
                    'mapping' => Config::$discoveredFieldOccurrence,
                ];
                if (!empty(self::$taskId)) {
                    $data['task_id'] = Config::$taskId;
                }

                $response = $client->post($path, [
                    'json' => $data
                ]);

                SkipprLogger::info('Updated pipeline metadata in Skippr SaaS');
            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }
        } else {
            Config::$state[Config::$pipelineName]['mapping'] = Config::$discoveredFieldOccurrence;
            Config::$state[Config::$pipelineName]['pipeline_name'] = Config::$pipelineName;
            Config::$state['tenant_id'] = Config::$tenantId;
//            Config::$state[Config::$pipelineName]['offsets'] = Config::$offsets;

            try {
                file_put_contents(
                    self::$dataDir . '/skippr-state.json',
                    json_encode(Config::$state)
                );

                SkipprLogger::info('Written state to ' . self::$dataDir . '/skippr-state.json');
            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }
        }

        return $configYml;
    }

    public static function setStatus(array $response = null)
    {

        $uri = self::getenv('SKIPPR_API_ENDPOINT');

        if (!empty($uri)) {
            try {
                $path = 'tasks/set-status';

                $client = new \GuzzleHttp\Client([
                    'base_uri' => $uri,
                    'headers' => [
                        'Authorization' => "Bearer " . self::getenv('SKIPPR_API_TOKEN')
                    ]
                ]);


                if (!empty(Config::getenv('DATA_SOURCE_PLUGIN_NAME'))) {
                    $logs['input'] = Config::$taskLogs;
                }

                if (!empty(Config::getenv('DATA_OUTPUT_PLUGIN_NAME'))) {
                    $logs['output'] = Config::$taskLogs;
                }

                $data = [
                    'response' => $response,
                    'task_id' => Config::$taskId,
                    'logs' => $logs,
                ];

                Config::$taskLogs = [];

                $exitMsg = "Notified task status API";

                if (isset(Config::$exitCode) && Config::$exitCode !== null) {
                    $data['exit_code'] = Config::$exitCode;

                    $exitCode = Config::$exitCode;
                    $exitMsg = "{$exitMsg} with exit code {$exitCode}";
                }

                SkipprLogger::info($exitMsg);

                $response = $client->post($path, [
                    'json' => $data
                ]);

            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }
        }
    }
}
