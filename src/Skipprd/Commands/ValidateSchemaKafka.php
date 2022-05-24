<?php

namespace Skipprd\Commands;

//use App\IngestJob;
//use App\Schema;
//use Illuminate\Contracts\Queue\ShouldQueue;
//use Illuminate\Support\Facades\Cache;
//use Illuminate\Support\Facades\Log;
//use Iq\Commands\PipelineCommand;
//use Iq\Events\ValidateSchemaRequested;
//use Iq\Services\AvroSubPub\CachedSchemaRegistryClient;
//use Iq\Services\AvroSubPub\MessageSerializer;
//use Iq\Traits\Config;
//use phpDocumentor\Reflection\Types\Self_;
//use Superbalist\LaravelPubSub\PubSubConnectionFactory;
//use Illuminate\Queue\InteractsWithQueue;
//use Iq\Traits\Ingest;
//use Iq\Traits\AnalyseSchema;

class ValidateSchemaKafka
{

//    use InteractsWithQueue;
//    use Ingest;
//    use AnalyseSchema;
//    use Config;
//
//    private static $msgMax = 1000;
//
//    private static $cacheProgressKey = '';
//    private static $cacheResultKey = '';
//
////    private $avroFieldSchema = [];
//
//    private $validationErrors = [];
//
//    /**
//     * @var array - the ingested message with schema evolution applied
//     *            - useful for asserting against in unit test to validate evolution occurred as expected
//     */
//    public $msg = [];
//
//    public function handle(ValidateSchemaRequested $configRequest)
//    {
//
//        try {
//
//            $ingestJob = $configRequest->ingestJob;
//            $fieldYml = $configRequest->field_yml;
//
//            $this->serde = $this->avroSerde('test-value');
//
//            self::$cacheProgressKey = 'schema_validation_progress_' . $ingestJob->getCleanName();
//            self::$cacheResultKey = 'schema_validation_result_' . $ingestJob->getCleanName();
//
////            if ($ingestJob->isAnalysing()) {
////                return true;
////            }
//
////            AnalyseSchema::determineFieldTypes($fieldYml);
//
//            $this->buildAvroSchema($fieldYml);
//
//            /*
//             * @todo - Map contains only string values
//             */
//            $this->validateMapEvolution($fieldYml);
//
//            /*
//             * @todo - detect breaking change
//             *  - to write schema (in destination)
//             */
//
//            /*
//             * Casts are valid
//             */
//
////            foreach ($fieldYml as $field => $metaData) {
////
////                // Validate Cast
////                // @todo - we auto cast, so there's no reason to support casting as an evolution option?
////                if (!empty($metaData['evolution']) && count($metaData['evolution'])) {
////                    foreach ($metaData['evolution'] as $type => $evolution) {
////                        if ($evolution['type'] == 'cast') {
////
//////                            $type = $metaData['determined_type'];
////                            $castType = $evolution['new_value'];
////
////                            if (!in_array($type, AnalyseSchema::$dataTypeCasts[$castType])) {
////
////                                $this->validationErrors['cast' . $field . $type . $castType] = "Field: $field - $type cannot be cast to data type $castType.";
////                            }
////                        }
////                    }
////                }
////            }
//
//            /*
//             * Can ingest and Avro encode dead letters
//             */
//            $pubsub = $this->initKafka($ingestJob);
//            $deadLetterTopicName = 'raw_' . $ingestJob->tenant_id . '_' . $ingestJob->getCleanName() .'_deadletter';
//
//            $i = 0;
//
//            Cache::add(self::$cacheProgressKey, 0, 5);
//
//            $consumer = $pubsub->getConsumer();
//
//            $consumer->subscribe([$deadLetterTopicName]);
//
//            $isSubscriptionLoopActive = true;
//
//            while ($isSubscriptionLoopActive) {
//
//                $record = $consumer->consume(10 * 1000);
//
//                try {
//
//                    $i++;
//
//                    $record = json_decode($record->payload, true);
//
//                    if ($record == 'unsubscribe'
//                        || $record == null // no dead letters
//                    ) {
//                        $isSubscriptionLoopActive = false;
//
//                        return $this->isValid();
//
//                    }
//
////                    if ($i % 100 === 0) {
////                        Log::debug("Parsed $i records");
////                    }
//
//                   $this->ingestRecord($record, $fieldYml);
//
//                    Cache::put(self::$cacheProgressKey, $i, 5);
//
//                    if ($i > self::$msgMax) {
//
//                        Log::info("Validated against $i records");
//
//                        return $this->isValid();
//
//                    }
//
//                } catch (\Exception $e) {
//                    Log::error("Fatal error while validating schema againstDead Letter queue");
////                    Log::error($e->getTraceAsString()); // very verbose for each message
//
//                    $this->validationErrors['fatal_dead_letter'] = $e->getMessage();
//
//                    return $this->isValid();
//                }
//
//            }
//
//        } catch (\Exception $e) {
//            Log::error('Caught exception: ' . $e->getMessage());
//            Log::error('On line: ' . $e->getLine());
//            Log::error('Of file: ' . $e->getFile());
//
//            $this->validationErrors['fatal_run'] = $e->getMessage();
//
//            return $this->isValid();
//        }
//
//    }
//
//    public function validateMapEvolution($fieldYml)
//    {
//        foreach ($fieldYml as $field => $metaData) {
//
//            if (!empty($metaData['parent_type']) && $metaData['parent_type'] == 'map') {
//
//                if (!empty($metaData['evolution']) && count($metaData['evolution'])) {
//                    foreach ($metaData['evolution'] as $type => $evolution) {
//                        if ($evolution['type'] == 'new') {
//
//                            $newField = $evolution['new_value'];
//                            $parentType = $metaData['parent_type'];
//                            $parentMapType = $metaData['determined_type'];
//
//                            if ($type != $metaData['type']) {
//
//                                $this->validationErrors['new' . $field . $type . $newField] = "Field: $field - $type cannot be added to a $parentType field of $parentMapType.";
//
//                            }
//                        }
//                    }
//                }
//            }
//
//            if (!empty($metaData['fields'])) {
//                $this->validateMapEvolution($metaData['fields']);
//            }
//        }
//
//    }
//
//    public function ingestRecord($record, $fieldYml)
//    {
//        $this->msg = [];
////                    $this->msg['skpr_event_ts'] = 0;
//
//        $this->msg = $this->defaultMessage();
//
//        foreach ($record as $field => $value) {
//
//            try {
//
//                $this->ingestField($field, $value, $fieldYml, $this->msg);
//
//            } catch (\Exception $e) {
//
//                $type = $fieldYml[$field]['determined_type'];
//
//                $this->validationErrors['ingest' . $field . $type] = "Field: $field - cannot ingest and cast value ($value) to data type $type.";
////                            throw $e;
//
//            }
//        }
//
//
//        // Test each field encodes
//        $this->encodeAvroFields($this->schema, $this->msg);
//
//        // Test encode of whole message (e.g. to check maps)
//        try {
//            $this->encodeRecord($this->schema, $this->msg, false);
//
//        } catch (\Exception $e) {
//
//            // Very verbose for each messsage
////            Log::error($e->getMessage());
////            Log::error($e->getTraceAsString());
//
//            $this->validationErrors['record_encode'] = "Cannot encode data, one or more of the fields schema types are invalid.";
//        }
//
//
//    }
//
//    public function isValid()
//    {
//
//        $this->delete();
//
//        if (empty($this->validationErrors)) {
//
//            Log::info("Schema is valid");
//
//            $result = [
//                'result' => 'valid',
//                'errors' => [],
//            ];
//
//            Cache::put(self::$cacheResultKey, $result, 5);
//
//            return true;
//
//        } else {
//
//            $result = [
//                'result' => 'invalid',
//                'errors' => [],
//            ];
//
//            foreach ($this->validationErrors as $error) {
//
//                $result['errors'][] = $error;
//
//                Log::error($error);
//            }
//
//            Log::info("Schema not valid");
//
//            Cache::put(self::$cacheResultKey, $result, 5);
//
//            return false;
//        }
//
//    }
//
//    public function buildAvroSchema($fieldYml)
//    {
//
//        if (!empty($fieldYml)) {
//
//            $sub_field_count = [];
//
//            foreach ($fieldYml as $field => $metaData) {
//
//                if (!empty($metaData['determined_type'])) {
//
//                    $avroType = $metaData['determined_type'];
//
//                    IngestJob::buildAvroFields($this->schema, $field, $avroType, $fieldYml, [], $sub_field_count);
//                }
//            }
//        }
//
////        $this->avroFieldSchema = Schema::schemaMerge(TaskConfig::$specialFieldsMapping, $this->avroFieldSchema);
//
//
//    }
//
//    public function initKafka(IngestJob $ingestJob)
//    {
//
//        $factory = app(PubSubConnectionFactory::class);
//
//        $config = config('pubsub.connections.kafka');
//        $config['consumer_group_id'] = $ingestJob->tenant_id . '.validate-schema.' . $ingestJob->pipelineName . '.' . md5(randomPassword(12));
//
//        $config['producer']['enable.idempotence'] = true;
//        $config['producer']['message.send.max.retries'] = 10000000;
//        $config['producer']['bootstrap.servers'] = $config['brokers'];
//
//        $this->pubsub = $factory->make('kafka', $config);
//
//        return $this->pubsub;
//
//    }
//
//
//    public function encodeAvroFields($avroFields, array $record)
//    {
//
//        $recordsWithSchema = [];
//
//        foreach ($avroFields as $key => $avroField) {
//
//            $value = $record[$avroField['name']];
//            $field = $avroField['name'];
//            $fieldRecord = [];
//            $fieldRecord[$field] = $value;
//
//            if (!empty($avroField['type'][1]['type'])
//                && in_array($avroField['type'][1]['type'], ['map', 'array', 'record'])) {
//
//                $type = $avroField['type'][1]['type'];
//
//            } else {
//                $type = $avroField['type'][1];
//            }
//
//
//            try {
//
//                if ($type == 'record') {
//
//                    $recordsWithSchema[] = $this->encodeAvroFields($avroField['type'][1]['fields'], $fieldRecord[$field]);
//
//                } else { // primitive/map type
//
//                    $recordsWithSchema[] = $this->encodeRecord([$avroField], $fieldRecord);
//                }
//
//            } catch (\Exception $e) {
//
//                $serialisedValue = json_encode($value, true);
//
//                $errorMsg = $e->getMessage();
//
//                // unique error message
//                $this->validationErrors['encode' . $field . $type] = "Field: $field - cannot encode value ($serialisedValue) to data type $type.";
//
//            }
//        }
//
//
//        return $recordsWithSchema;
//
//    }
//
//    public function encodeRecord($avroSchema, $record)
//    {
//        $schema = ['type' => 'record', 'name' => 'abc', 'fields' => $avroSchema];
//
//        $recordsWithSchema = [];
//
//        try {
//
//            $valueSchemaJson = json_encode($schema);
//            $valueSchema = \AvroSchema::parse($valueSchemaJson);
//
//            // Encode with passed schema, without hitting schema registry
//            // and therefore not having to save schema
//            $subject = '';
//            $version = 1;
//
//
//            $this->serde->subjectVersionToWritersSet($subject, $version, $valueSchema);
//            $recordsWithSchema = $this->serde->encodeRecordWithSubjectAndVersion($subject, $version, $record, false);
//
//        } catch (\Exception $e) {
//
//            // unique error message
////            $this->validationErrors[] = $e->getMessage();
////            $this->validationErrors[] = $e->getLine();
////            $this->validationErrors[] = $e->getFile();
//
//            throw $e;
//        }
//
//        return $recordsWithSchema;
//
//    }
//
//    public function avroSerde($name) {
//
//        $container = new CachedSchemaRegistryClient([]);
//
//        $serde = new MessageSerializer($container, []);
//
//        return $serde;
//
//    }
}
