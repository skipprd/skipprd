<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 07/08/2019
 * Time: 20:49
 */

namespace Skipprd\Traits;

use Skipprd\Helpers;

class Config
{

    public static $pipelineName = '';

    public static $tenantId = '';

    public static $mode = 'sync';

    public static $serder = null;

    public static $analysing = true;

    protected static $idFields = [];

    public static $dateFieldCandidates = [];

    public static $discoveredFieldOccurrence = [];

    public static $schema = [];

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
    
    public static function getConfig()
    {
//        self::$pipelineId = getenv('PIPELINE_ID');
        self::$pipelineName = getenv('PIPELINE_NAME');
        self::$tenantId = getenv('TENANT');

        self::$discoveredFieldOccurrence = [];
//        Config::$discoveredFieldOccurrence = (empty($configYml['field_yml'])) ? [] : $configYml['field_yml'];

        self::$schema['fields'] = [];
        self::$schema = (empty($configYml['schema_yml'])) ? self::$schema : $configYml['schema_yml'];
//        $this->avroSchema = (empty($configYml['schema_yml'])) ? [] : \AvroSchema::real_parse($configYml['schema_yml']);


        self::$eventPath = getenv('EVENT_PATH');
        self::$serder = getenv('SERDER');
        self::$entityNames = [];
        self::$timeFields = [];

        self::$analysing = (bool) getenv('ANALYSING');
        self::$systemUserApiToken = getenv('SYSTEM_USER_API_TOKEN');

        // Although we may be done analysing, we don't want to override candidate.
        // They should remain in the option list even if the user has rejected them.
//        if (!empty($configYml['field_yml']['date_field_candidates'])) {
//            $this->dateFieldCandidates = $configYml['field_yml']['date_field_candidates'];
//        }
//
//        if (!empty($configYml['field_yml']['date_field_candidates'])) {
//            $this->idFields = $configYml['field_yml']['enitity_field_candidates'];
//        }

    }

    public static function setConfig()
    {

//        $fieldsYml = Config::$discoveredFieldOccurrence;
//
//        $schemaArr = [];
//
//        if (!empty($this->schema['fields'])) { // empty when first analysing
//            ksort($this->schema['fields']);
//
//            $schemaArr = $$this->schema['fields'];
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

//        return $configYml;
    }
}