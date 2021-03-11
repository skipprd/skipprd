<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 07/08/2019
 * Time: 20:49
 */

namespace Skipprd\Traits;

use Skipprd\Converters\SkipprAvroSchemaConverter;

class Config
{

    public static $dataDir = '/data';

    public static $pipelineName = 'pipeline';

    public static $tenantId = 'skippr';

    public static $mode = 'sync';

    public static $sourceFormat = null;

    public static $outputFormat = null;

    public static $analysing = true;

    public static $idFields = [];

    public static $dateFieldCandidates = [];

    public static $discoveredFieldOccurrence = [];

    public static $schema = [];

    public static $mapping = [];

    /**
     * @var \AvroSchema $avroSchema
     */
    public static $avroSchema;

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

    public static function getenv(string $name, string $default = '') : string {

        return (!empty(getenv($name))) ? getenv($name) : $default;

    }

    public static function getConfig()
    {
//        self::$pipelineId = getenv('PIPELINE_ID');
        self::$pipelineName = self::getenv('PIPELINE_NAME', self::$pipelineName);
        self::$tenantId = self::getenv('TENANT_ID', self::$tenantId);


        $avroArr = [];
//        self::$mapping = [];
        self::$discoveredFieldOccurrence = [];

        $dataDir = getenv('DATA_DIR');
        self::$dataDir = (empty($dataDir)) ? self::$dataDir : $dataDir;
        @mkdir(self::$dataDir);

        if (file_exists(self::$dataDir . '/skippr-state.json')) {

            self::$discoveredFieldOccurrence = json_decode(file_get_contents(self::$dataDir . '/skippr-state.json'), true);

//        Config::$discoveredFieldOccurrence = (empty($configYml['field_yml'])) ? [] : $configYml['field_yml'];

            $converter = new SkipprAvroSchemaConverter();
            $avroArr = $converter->convert(self::$discoveredFieldOccurrence);

            $avroArr = self::schemaMerge(self::$specialFieldsMapping, $avroArr);
        }
        


        self::$schema['fields'] = [];

        self::$schema['fields'] = (empty($avroArr)) ? [] : $avroArr;

        self::$avroSchema = self::buildAvroSchema();

        self::$eventPath = getenv('EVENT_PATH');

        self::$sourceFormat = getenv('DATA_SOURCE_FORMAT');
        self::$outputFormat = getenv('DATA_OUTPUT_FORMAT');

        self::$entityNames = [];
        self::$timeFields = [];

//        self::$analysing = (bool) getenv('ANALYSING');
        self::$analysing = (empty($avroArr)) ? true : false;

        self::$systemUserApiToken = getenv('SYSTEM_USER_API_TOKEN');

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

    public static function buildAvroSchema()
    {

        $schemaName = self::$tenantId . '_' . self::$pipelineName;

        $schemaNamespace = "io.skippr." . self::$tenantId . "." . self::$pipelineName;
        
        $valueAvroSchema['namespace'] = $schemaNamespace;
        $valueAvroSchema['name'] = $schemaName;
        $valueAvroSchema['type'] = 'record';

        $valueAvroSchema['fields'] = self::$schema['fields'];

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


        file_put_contents(self::$dataDir . '/skippr-state.json', json_encode(Config::$discoveredFieldOccurrence));

        $uri = getenv('SCHEMA_REGISTRY');

        if (!empty($uri)) {

            $url = "http://$uri/";
            $path = 'ingest-job/update-mapping';

            $client = new \GuzzleHttp\Client([
                'base_uri' => $url,
                'headers' => [
                    'Authorization' => "Bearer " . getenv('API_TOKEN')
                ]
            ]);

            $json = json_encode([
                'id' => getenv('PIPELINE_ID'),
                'mapping' => Config::$discoveredFieldOccurrence,
            ]);

            $response = $client->post($path, [
                'json' => $json
            ]);
        }

        return $configYml;
    }

}