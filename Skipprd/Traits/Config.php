<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 07/08/2019
 * Time: 20:49
 */

namespace Skipprd\Traits;

use App\Schema;
use http\Client;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Helpers;

class Config
{

    public static $pipelineName = '';

    public static $tenantId = '';

    public static $mode = 'sync';

    public static $sourceFormat = null;

    public static $outputFormat = null;

    public static $analysing = true;

    public static $idFields = [];

    public static $dateFieldCandidates = [];

    public static $discoveredFieldOccurrence = [];

    public static $schema = [];
    
    public static \AvroSchema $avroSchema;

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


    public static function getConfig()
    {
//        self::$pipelineId = getenv('PIPELINE_ID');
        self::$pipelineName = getenv('PIPELINE_NAME');
        self::$tenantId = getenv('TENANT_ID');


        $avroArr = [];
        self::$mapping = [];
        self::$discoveredFieldOccurrence = [];
        
        if (file_exists('/tmp/mapping.yaml')) {

            self::$mapping = file_get_contents('/tmp/mapping.yaml');

            self::$discoveredFieldOccurrence = yaml_parse(self::$mapping);
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


        yaml_emit_file('/tmp/mapping.yaml', Config::$discoveredFieldOccurrence);

        $yml = yaml_emit(Config::$discoveredFieldOccurrence);
        $url = 'http://localhost:8081/';
        $path = 'ingest-job/update-mapping';

        $client = new \GuzzleHttp\Client([
            'base_uri' => $url,
            'headers' => [
                'Authorization' => "Bearer " . getenv('API_TOKEN')
            ]
        ]);

        $json = json_encode([
            'id' => getenv('PIPELINE_ID'),
            'mapping' => $yml,
        ]);

        $foo = json_decode($json, true);
        $foo = yaml_parse($foo['mapping']);

        $response = $client->post($path, [
            'json' => $json
        ]);

        return $configYml;
    }

    protected static $mapping = <<<EOF
bike_id:
    count: 10000
    type:
        integer: 9999
        timestamp: 9999
    parent_type: null
    fields: {  }
    evolution:
        integer:
            type: null
            new_value: null
            sample: '100363'
            solved: false
        timestamp:
            type: null
            new_value: null
            sample: '100363'
            solved: false
    last_value: null
    determined_type: integer
rider_id:
    count: 10000
    type:
        integer: 9999
        timestamp: 9999
    parent_type: null
    fields: {  }
    evolution:
        integer:
            type: null
            new_value: null
            sample: '100973'
            solved: false
        timestamp:
            type: null
            new_value: null
            sample: '100973'
            solved: false
    last_value: null
    determined_type: integer
contact:
    count: 10000
    type:
        map: 55
        record: 9944
    parent_type: null
    fields:
        name:
            count: 10000
            type:
                string: 9999
            parent_type: record
            fields: {  }
            evolution:
                string:
                    type: null
                    new_value: null
                    sample: 'Marianne Kutch'
                    solved: false
            last_value: null
            determined_type: string
        postcode:
            count: 10000
            type:
                string: 55
                integer: 4524
                timestamp: 4524
                date: 5420
            parent_type: record
            fields: {  }
            evolution:
                string:
                    type: new
                    new_value: zip
                    sample: 71689-1315
                    solved: true
                integer:
                    type: null
                    new_value: null
                    sample: '97037'
                    solved: false
                timestamp:
                    type: null
                    new_value: null
                    sample: '97037'
                    solved: false
                date:
                    type: null
                    new_value: null
                    sample: 54943-9142
                    solved: false
            last_value: null
            determined_type: integer
        email:
            count: 10000
            type:
                string: 9999
            parent_type: record
            fields: {  }
            evolution:
                string:
                    type: null
                    new_value: null
                    sample: von.irma@kunze.biz
                    solved: false
            last_value: null
            determined_type: string
        zip:
            count: 55
            type:
                string: 55
            parent_type: record
            fields: {  }
            evolution:
                string:
                    type: null
                    new_value: null
            last_value: null
            determined_type: string
    evolution:
        map:
            type: null
            new_value: null
            sample:
                name: 'Marianne Kutch'
                postcode: 71689-1315
                email: von.irma@kunze.biz
            solved: false
        record:
            type: null
            new_value: null
            sample:
                name: 'Efren Stracke'
                postcode: '97037'
                email: tsauer@gmail.com
            solved: false
    last_value: null
    determined_type: record
hire_start_time:
    count: 10000
    type:
        integer: 9999
        timestamp: 9999
    parent_type: null
    fields: {  }
    evolution:
        integer:
            type: null
            new_value: null
            sample: 1609184429
            solved: false
        timestamp:
            type: null
            new_value: null
            sample: 1609184429
            solved: false
    last_value: null
    determined_type: integer
hire_end_time:
    count: 10000
    type:
        integer: 9999
        timestamp: 9999
    parent_type: null
    fields: {  }
    evolution:
        integer:
            type: null
            new_value: null
            sample: 1609185285
            solved: false
        timestamp:
            type: null
            new_value: null
            sample: 1609185285
            solved: false
    last_value: null
    determined_type: integer
metadata:
    count: 10000
    type:
        record: 9999
    parent_type: null
    fields:
        rcvd_time:
            count: 10000
            type:
                integer: 9999
                timestamp: 9999
            parent_type: record
            fields: {  }
            evolution:
                integer:
                    type: null
                    new_value: null
                    sample: 1609184405
                    solved: false
                timestamp:
                    type: null
                    new_value: null
                    sample: 1609184405
                    solved: false
            last_value: null
            determined_type: integer
        sent_time:
            count: 10000
            type:
                integer: 9999
                timestamp: 9999
            parent_type: record
            fields: {  }
            evolution:
                integer:
                    type: null
                    new_value: null
                    sample: 1609184355
                    solved: false
                timestamp:
                    type: null
                    new_value: null
                    sample: 1609184355
                    solved: false
            last_value: null
            determined_type: integer
        prcd_micro_time:
            count: 10000
            type:
                double: 9999
            parent_type: record
            fields: {  }
            evolution:
                double:
                    type: null
                    new_value: null
                    sample: 1609184345.4985
                    solved: false
            last_value: null
            determined_type: double
        tags:
            count: 10000
            type:
                record: 9999
            parent_type: record
            fields:
                a0:
                    count: 10000
                    type:
                        map: 9999
                    parent_type: record
                    fields:
                        name:
                            count: 10000
                            type:
                                string: 9999
                            parent_type: map
                            fields: {  }
                            evolution:
                                string:
                                    type: null
                                    new_value: null
                                    sample: type
                                    solved: false
                            last_value: null
                            determined_type: string
                        value:
                            count: 10000
                            type:
                                string: 9999
                            parent_type: map
                            fields: {  }
                            evolution:
                                string:
                                    type: null
                                    new_value: null
                                    sample: trip
                                    solved: false
                            last_value: null
                            determined_type: string
                    evolution:
                        map:
                            type: null
                            new_value: null
                            sample:
                                name: type
                                value: trip
                            solved: false
                    last_value: null
                    determined_type: map
            evolution:
                record:
                    type: null
                    new_value: null
                    sample:
                        -
                            name: type
                            value: trip
                    solved: false
            last_value: null
            determined_type: record
    evolution:
        record:
            type: null
            new_value: null
            sample:
                rcvd_time: 1609184405
                sent_time: 1609184355
                prcd_micro_time: 1609184345.4985
                tags:
                    -
                        name: type
                        value: trip
            solved: false
    last_value: null
    determined_type: record
location:
    count: 10000
    type:
        record: 9999
    parent_type: null
    fields:
        start_geo:
            count: 10000
            type:
                map: 9999
            parent_type: record
            fields:
                lat:
                    count: 10000
                    type:
                        double: 9999
                    parent_type: map
                    fields: {  }
                    evolution:
                        double:
                            type: null
                            new_value: null
                            sample: -32.163514
                            solved: false
                    last_value: null
                    determined_type: double
                lon:
                    count: 10000
                    type:
                        double: 9999
                    parent_type: map
                    fields: {  }
                    evolution:
                        double:
                            type: null
                            new_value: null
                            sample: 22.354385
                            solved: false
                    last_value: null
                    determined_type: double
            evolution:
                map:
                    type: null
                    new_value: null
                    sample:
                        lat: -32.163514
                        lon: 22.354385
                    solved: false
            last_value: null
            determined_type: map
        end_geo:
            count: 10000
            type:
                map: 9999
            parent_type: record
            fields:
                lat:
                    count: 10000
                    type:
                        double: 9999
                    parent_type: map
                    fields: {  }
                    evolution:
                        double:
                            type: null
                            new_value: null
                            sample: 74.484787
                            solved: false
                    last_value: null
                    determined_type: double
                lon:
                    count: 10000
                    type:
                        double: 9999
                    parent_type: map
                    fields: {  }
                    evolution:
                        double:
                            type: null
                            new_value: null
                            sample: 154.902423
                            solved: false
                    last_value: null
                    determined_type: double
            evolution:
                map:
                    type: null
                    new_value: null
                    sample:
                        lat: 74.484787
                        lon: 154.902423
                    solved: false
            last_value: null
            determined_type: map
    evolution:
        record:
            type: null
            new_value: null
            sample:
                start_geo:
                    lat: -32.163514
                    lon: 22.354385
                end_geo:
                    lat: 74.484787
                    lon: 154.902423
            solved: false
    last_value: null
    determined_type: record
date_field_candidates:
    bike_id:
        valid_count: 9999
        check_count: 9999
        field: bike_id
    rider_id:
        valid_count: 9999
        check_count: 9999
        field: rider_id
    postcode:
        check_count: 9096
        valid_count: 9048
        field: postcode
    hire_start_time:
        valid_count: 9999
        check_count: 9999
        field: hire_start_time
    hire_end_time:
        valid_count: 9999
        check_count: 9999
        field: hire_end_time
    rcvd_time:
        valid_count: 19998
        check_count: 19998
        field: rcvd_time
    sent_time:
        valid_count: 19998
        check_count: 19998
        field: sent_time
enitity_field_candidates:
    bike_id: {  }
    rider_id: {  }
EOF;
}