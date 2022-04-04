<?php

namespace Skipprd\Commands;


use Skipprd\Buffers\BufferAdaptorsFactory;
use Skipprd\Serders\SerdersFactory;
use Skipprd\Services\AvroSubPub\CachedSchemaRegistryClient;
use Skipprd\Services\AvroSubPub\MessageSerializer;
use Skipprd\SkipprPack;
use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;
use Skipprd\Traits\SkipprLogger;

class ValidateSchemaFile
{

    use Ingest;

    private static $msgMax = 1000;

    private $validationErrors = [];

    /**
     *
     * @var array - the ingested message with schema evolution applied
     *            - useful for asserting against in unit test to validate evolution occurred as expected
     */
    public $msg = [];

    /**
     * @var \Skipprd\Services\AvroSubPub\MessageSerializer
     */
    public $serde;

    /**
     * Execute the console command.
     */
    public function init()
    {


    }


        public function handle()
    {


        $this->init();

        try {

//            $ingestJob = $configRequest->ingestJob;
            $fieldYml = Config::$discoveredFieldOccurrence;

            $this->serde = $this->avroSerde('test-value');
            
//            self::$cacheProgressKey = 'schema_validation_progress_' . $ingestJob->getCleanName();
//            self::$cacheResultKey = 'schema_validation_result_' . $ingestJob->getCleanName();

//            if ($ingestJob->isAnalysing()) {
//                return true;
//            }

//            AnalyseSchema::determineFieldTypes($fieldYml);


            /*
             * @todo - Map contains only string values
             */
            $this->validateMapEvolution($fieldYml);

            /*
             * @todo - detect breaking change
             *  - to write schema (in destination)
             */

            /*
             * Casts are valid
             */

//            foreach ($fieldYml as $field => $metaData) {
//
//                // Validate Cast
//                // @todo - we auto cast, so there's no reason to support casting as an evolution option?
//                if (!empty($metaData['evolution']) && count($metaData['evolution'])) {
//                    foreach ($metaData['evolution'] as $type => $evolution) {
//                        if ($evolution['type'] == 'cast') {
//
////                            $type = $metaData['determined_type'];
//                            $castType = $evolution['new_value'];
//
//                            if (!in_array($type, AnalyseSchema::$dataTypeCasts[$castType])) {
//
//                                $this->validationErrors['cast' . $field . $type . $castType] = "Field: $field - $type cannot be cast to data type $castType.";
//                            }
//                        }
//                    }
//                }
//            }

            $i = 0;

            Config::setStatus($this->setStatusString(0));

            $fileBuffer = BufferAdaptorsFactory::getAdaptor('deadletter', 'file');

//            $identifier = $ingestJob->tenant_id . '-' . $ingestJob->getCleanName() . '-skipprd-deadletter';
//            $fileBuffer->tempdir = '/data/' . $identifier;

            while ($line = $fileBuffer->driver->stream()) {

                try {

                    $i++;

                    $sp = new SkipprPack($line);
                    $payload = $sp->decodeRecord();
                    $offset = $sp->decodeOffset();

                    $record = json_decode($payload, true);

                    $namespace = $fileBuffer->decodeChunkNamespace($fileBuffer->driver->streamGetCurrentBufferFile());
                    $partition = $fileBuffer->decodeChunkPartition($fileBuffer->driver->streamGetCurrentBufferFile());
                    $record['skpr_partition'] = $partition;

                    $this->ingestRecord($record, $fieldYml, $namespace);

                    $chunkSize = self::$msgMax / 5;

                    if ($i % $chunkSize = 0) {
                        Config::setStatus($this->setStatusString($i));
                    }


                    if ($i > self::$msgMax) {

                        SkipprLogger::info("Validated against $i records");

                        $fileBuffer->driver->unlockAll();
                        
                        return $this->isValid();

                    }


                } catch (\Exception $e) {
                    SkipprLogger::error("Fatal error while validating schema againstDead Letter queue");
//                    SkipprLogger::error($e->getTraceAsString()); // very verbose for each message

                    $this->validationErrors[$namespace]['fatal_dead_letter'] = $e->getMessage();

                    $fileBuffer->driver->unlockAll();

                    return $this->isValid();
                }

            }

            return $this->isValid();

        } catch (\Exception $e) {
            SkipprLogger::error('Caught exception: ' . $e->getMessage());
            SkipprLogger::error('On line: ' . $e->getLine());
            SkipprLogger::error('Of file: ' . $e->getFile());

            $this->validationErrors['fatal_run'] = $e->getMessage();

            $fileBuffer->driver->unlockAll();

            return $this->isValid();
        }

    }

    /**
     * @param int $progressPercent
     * @return string
     */
    public function setStatusString(int $progressPercent, array $result = []): string
    {

        $response = array_merge(['progress_percent' => $progressPercent], $result);

        return json_encode($response);
    }


    public function validateMapEvolution($fieldYml)
    {
        foreach ($fieldYml as $field => $metaData) {

            if (!empty($metaData['parent_type']) && $metaData['parent_type'] == 'map') {

                if (!empty($metaData['evolution']) && count($metaData['evolution'])) {
                    foreach ($metaData['evolution'] as $type => $evolution) {
                        if ($evolution['type'] == 'new') {

                            $newField = $evolution['new_value'];
                            $parentType = $metaData['parent_type'];
                            $parentMapType = $metaData['determined_type'];

                            if ($type != $metaData['type']) {

                                $this->validationErrors['new' . $field . $type . $newField] = "Field: $field - $type cannot be added to a $parentType field of $parentMapType.";

                            }
                        }
                    }
                }
            }

            if (!empty($metaData['fields'])) {
                $this->validateMapEvolution($metaData['fields']);
            }
        }

    }

    public function ingestRecord(array $record, array $fieldYml, string $namespace)
    {
        $this->msg = [];

        $serde = SerdersFactory::factory(Config::$outputFormat);

        foreach (Config::$schema as $namespace => $schema) {
            $this->msg[$namespace] = $serde->defaultMessage($schema);
        }

        foreach ($record as $field => $value) {

            try {

                $this->ingestField($field, $value, $fieldYml, $this->msg[$namespace]);

            } catch (\Exception $e) {

                $type = $fieldYml[$field]['determined_type'];

                $this->validationErrors[$namespace]['ingest' . $field . $type] = "Field: $field - 
                cannot ingest and cast value ($value) to data type $type.";
//                            throw $e;

            }
        }


        // Test each field encodes
        $this->encodeAvroFields(Config::$avroSchemas[$namespace], $this->msg, $namespace);

        // Test encode of whole message (e.g. to check maps)
        try {
            $this->encodeRecord(Config::$avroSchemas[$namespace], $this->msg);

        } catch (\Exception $e) {

            // Very verbose for each messsage
//            SkipprLogger::error($e->getMessage());
//            SkipprLogger::error($e->getTraceAsString());
            
            $this->validationErrors[$namespace]['record_encode'] = "Cannot encode data, 
            one or more of the fields schema types are invalid.";
        }


    }

    public function isValid()
    {

//        $this->delete();

        $result = [];

        if (empty($this->validationErrors)) {

            SkipprLogger::info("Schema is valid");

            $result = [
                'result' => 'valid',
                'errors' => [],
            ];

            Config::$exitCode = 0;
            Config::setStatus($this->setStatusString(100, $result));

            return true;

        } else {

            $result = [
                'result' => 'invalid',
                'errors' => [],
            ];

            foreach ($this->validationErrors as $namespace => $error) {

                $result['errors'][$namespace][] = $error;

                SkipprLogger::error($error);
            }

            SkipprLogger::info("Schema not valid");

            Config::$exitCode = 0;
            Config::setStatus($this->setStatusString(100, $result));

            return false;
        }

        
    }

    public function encodeAvroFields($avroFields, array $record, string $namespace)
    {

        $recordsWithSchema = [];

        foreach ($avroFields as $key => $avroField) {

            $value = $record[$avroField['name']];
            $field = $avroField['name'];
            $fieldRecord = [];
            $fieldRecord[$field] = $value;

            if (!empty($avroField['type'][1]['type'])
                && in_array($avroField['type'][1]['type'], ['map', 'array', 'record'])) {

                $type = $avroField['type'][1]['type'];

            } else {
                $type = $avroField['type'][1];
            }


            try {

                if ($type == 'record') {

                    $recordsWithSchema[] = $this->encodeAvroFields($avroField['type'][1]['fields'],
                        $fieldRecord[$field], $namespace);

                } else { // primitive/map type

                    $recordsWithSchema[] = $this->encodeRecord([$avroField], $fieldRecord);
                }

            } catch (\Exception $e) {

                $serialisedValue = json_encode($value, true);

                $errorMsg = $e->getMessage();

                // unique error message
                $this->validationErrors[$namespace]['encode' . $field . $type] = "Field: $field - 
                cannot encode value ($serialisedValue) to data type $type.";

            }
        }


        return $recordsWithSchema;

    }

    public function encodeRecord($avroSchema, $record)
    {
        $schema = ['type' => 'record', 'name' => 'abc', 'fields' => $avroSchema];

        $recordsWithSchema = [];
        
        try {

            $valueSchemaJson = json_encode($schema);
            $valueSchema = \AvroSchema::parse($valueSchemaJson);

            // Encode with passed schema, without hitting schema registry
            // and therefore not having to save schema
            $subject = '';
            $version = 1;


            $this->serde->subjectVersionToWritersSet($subject, $version, $valueSchema);
            $recordsWithSchema = $this->serde->encodeRecordWithSubjectAndVersion($subject, $version, $record, false);

        } catch (\Exception $e) {

            // unique error message
//            $this->validationErrors[] = $e->getMessage();
//            $this->validationErrors[] = $e->getLine();
//            $this->validationErrors[] = $e->getFile();

            throw $e;
        }

        return $recordsWithSchema;
    }

    /**
     * @param $name
     * @return \Skipprd\Services\AvroSubPub\MessageSerializer
     */
    public function avroSerde(string $name) {

        $container = new CachedSchemaRegistryClient([]);

        $serde = new MessageSerializer($container, []);

        return $serde;
    }

}