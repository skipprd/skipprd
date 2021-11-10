<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 07/08/2019
 * Time: 20:49
 */

namespace Skipprd\Traits;

use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Helpers;
use Monolog\Registry;
use Skipprd\Str;

class Config
{

    public static $segmentKey = 'RnewwWgZXQjl9xofcjGJkirCH0VswBPd';

    public static $anonymousMetrics = true;

    public static $dataDir = '/data';

    public static $pipelineName = '';

    public static $tenantId = '';

    public static $state = [];

    public static $mode = 'sync';

    public static $offsets = [];

    public static $sourceFormat = null;

    public static $outputFormat = null;

    public static $analysing = true;

    public static $idFields = [];

    public static $dateFieldCandidates = [];

    public static $discoveredFieldOccurrence = [];

    public static $schema = [];

    public static $mapping = [];

    public static $filters = [];

    /**
     * @var \AvroSchema $avroSchemas
     */
    public static $avroSchemas;

    public static $outputSchemas = [];

    public static $entityNames = [];

    public static $eventPath = '';

    public static $timeFields = [];

    public static $systemUserApiToken = '';

    public static $enableDeadLetters = true;

    protected static $configUpdatedTime = 0;

    public static $specialFields = [
        'skpr_event_ts' => 0,
        'skpr_partition' => '',
    ];

    static $specialFieldsMapping = [
        ['name' => 'skpr_event_ts', 'default' => null, 'type' => ['null', 'int']],
        ['name' => 'skpr_partition', 'default' => null, 'type' => ['null', 'string']],
    ];

    public static $batchFormats = [
        'parquet',
        'csv',
        'xml',
        'avro_file'
    ];

    public static function getenv(string $name, string $default = '') : string
    {

        return (!empty(getenv($name))) ? getenv($name) : $default;
    }

    public static function getPipelineName()
    {

        $inputPluginName = Helpers::cleanFieldName(Config::getenv('DATA_SOURCE_PLUGIN_NAME'));
        $outputPluginName = Helpers::cleanFieldName(Config::getenv('DATA_OUTPUT_PLUGIN_NAME'));
        $defaultPipelineName = $inputPluginName . 'to' . $outputPluginName;
        $defaultPipelineName = Config::getenv('PIPELINE_NAME', $defaultPipelineName);

        return $defaultPipelineName;
    }

    public static function getConfig()
    {
//        self::$pipelineId = self::getenv('PIPELINE_ID');

        $avroArr = [];
//        self::$mapping = [];
        self::$discoveredFieldOccurrence = [];

        self::$anonymousMetrics =  Config::getenv('ANONYMOUS_METRICS', true);

        $defaultPipelineName = Config::getPipelineName();

        Config::$state['tenant_id'] = Helpers::randomStr(16);
        self::$tenantId = self::getenv('TENANT_ID', Config::$state['tenant_id']);

        $dataDir = self::getenv('DATA_DIR');
        self::$dataDir = (empty($dataDir)) ? self::$dataDir : $dataDir;
        @mkdir(self::$dataDir);

        $uri = self::getenv('SCHEMA_REGISTRY');

        if (!empty($uri)) {
            // Get Mapping
            try {
                SkipprLogger::info('Looking up config for pipeline ' . $defaultPipelineName);

                $url = "http://$uri/";
                $path = 'ingest-job/get-mapping/'. $defaultPipelineName;

                $client = new \GuzzleHttp\Client([
                    'base_uri' => $url,
                    'headers' => [
                        'Authorization' => "Bearer " . self::getenv('SCHEMA_API_TOKEN')
                    ]
                ]);

                $body = $client->get($path)->getBody();

                $mapping = json_decode($body, true);

                self::$discoveredFieldOccurrence = $mapping;
            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }

            // Get Schema
            try {
                SkipprLogger::info('Looking up schema for pipeline ' . $defaultPipelineName);

                $schemaName = self::$tenantId . '_' . $defaultPipelineName . '-value';
                $url = "http://$uri/";
                $path = 'subjects/'. $schemaName . '/versions/latest';

                $client = new \GuzzleHttp\Client([
                    'base_uri' => $url,
                    'headers' => [
                        'Authorization' => "Bearer " . Config::getenv('SCHEMA_API_TOKEN')
                    ]
                ]);

                $resp = json_decode($client->get($path)->getBody()->getContents(), true);

                $avroArr = json_decode($resp['schema'], true);
            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }
        } else {
            if (file_exists(self::$dataDir . '/skippr-state.json')) {
                try {
                    SkipprLogger::info('Found existing ' . self::$dataDir . '/skippr-state.json');

                    self::$state = json_decode(
                        file_get_contents(self::$dataDir . '/skippr-state.json'),
                        true
                    );

                    if (!empty(Config::$state[$defaultPipelineName])) {
                        SkipprLogger::info('Loading state for pipeline ' . $defaultPipelineName);

                        self::$discoveredFieldOccurrence = Config::$state[$defaultPipelineName]['mapping'];

//        Config::$discoveredFieldOccurrence = (empty($configYml['field_yml'])) ? [] : $configYml['field_yml'];
                    }
                } catch (\Exception $e) {
                    SkipprLogger::error($e->getMessage());
                }
            }
        }


        self::$pipelineName = Config::getenv('PIPELINE_NAME', $defaultPipelineName);

//        self::$offsets = (!empty(self::$state[$defaultPipelineName]['offsets']) ? self::$state[$defaultPipelineName]['offsets'] : []);

//        self::$schema['fields'] = [];
//
//        self::$schema['fields'] = (empty($avroArr)) ? [] : $avroArr;

        self::$eventPath = self::getenv('DATA_SOURCE_EVENT_PATH');

        self::$sourceFormat = self::getenv('DATA_SOURCE_FORMAT', '');
        self::$outputFormat = self::getenv('DATA_OUTPUT_FORMAT', 'json');

        self::$entityNames = [];
        self::$timeFields = [];

        self::$analysing = (empty(self::$discoveredFieldOccurrence)) ? true : false;
        self::$analysing = (bool) self::getenv('ANALYSING', self::$analysing);
        
        self::$systemUserApiToken = self::getenv('SCHEMA_API_TOKEN');

        if (!empty(self::$discoveredFieldOccurrence)) {
            foreach (self::$discoveredFieldOccurrence as $partition => $mapping) {
                SkipprLogger::info("Building $partition schema");

                $converter = new SkipprAvroSchemaConverter();
                self::$schema[$partition] = $converter->convert($mapping['fields']);

                self::$schema[$partition] = self::schemaMerge(self::$specialFieldsMapping, self::$schema[$partition]);

                self::$avroSchemas[$partition] = self::buildAvroSchema(self::$schema[$partition]);

//                $converter = new AvroParquetSchemaConverter();
//                self::$outputSchemas[$partition] = $converter->convert(Config::$avroSchemas[$partition]);

                $outputFormat = ucfirst(self::$outputFormat);
                $converterClass = 'Skipprd\Converters\Avro' . $outputFormat . 'SchemaConverter';

                if (class_exists($converterClass)) {
                    SkipprLogger::info("Converting $partition schema to $outputFormat");

                    $converter = new $converterClass();

                    self::$outputSchemas[Helpers::cleanFieldName($partition)] = $converter->convert(self::$avroSchemas[$partition]);
                } else {
                    self::$outputSchemas[Helpers::cleanFieldName($partition)] = self::$avroSchemas[$partition];
                }
            }
        }

        self::initFilters();

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

    public static function initFilters()
    {

        $envs = getenv();

        foreach ($envs as $name => $val) {
            if (Str::startsWith($name, 'FILTER_')) {
                SkipprLogger::info($name);
                SkipprLogger::info($val);

                $parts = explode('_', $name);
                $filterName = strtolower($parts[1]);
                unset($parts[0]);
                unset($parts[1]);
                $confName = strtolower(implode('_', $parts));

                SkipprLogger::info($filterName);

                Config::$filters[$filterName][$confName] = $val;
            }
        }
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


        $uri = self::getenv('SCHEMA_REGISTRY');

        if (!empty($uri)) {
            try {
                $url = "http://$uri/";
                $path = 'ingest-job/update-mapping';

                $client = new \GuzzleHttp\Client([
                    'base_uri' => $url,
                    'headers' => [
                        'Authorization' => "Bearer " . self::getenv('SCHEMA_API_TOKEN')
                    ]
                ]);

                $json = json_encode([
                    'id' => self::getenv('PIPELINE_ID'),
                    'mapping' => Config::$discoveredFieldOccurrence,
                ]);

                $response = $client->post($path, [
                    'json' => $json
                ]);

                SkipprLogger::info('Updated config via API');
            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }
        } else {
            Config::$state[Config::$pipelineName]['mapping'] = Config::$discoveredFieldOccurrence;
            Config::$state[Config::$pipelineName]['pipeline_name'] = Config::$pipelineName;
            Config::$state['tenant_id'] = Config::$tenantId;
//            Config::$state[Config::$pipelineName]['offsets'] = Config::$offsets;

            try {
                file_put_contents(self::$dataDir . '/skippr-state.json', json_encode(Config::$state));

                SkipprLogger::info('Written state to ' . self::$dataDir . '/skippr-state.json');
            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }
        }

        return $configYml;
    }
}
