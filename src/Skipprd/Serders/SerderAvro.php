<?php

namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderStreamInterface;
use Skipprd\Services\AvroSubPub\CachedSchemaRegistryClient;
use Skipprd\Services\AvroSubPub\MessageSerializer;
use Skipprd\Traits\Config;

class SerderAvro implements SerderStreamInterface
{

    protected $serializer;

    protected $defaultSchema = [];

    protected $tenantId = '';

    protected $pipelineName = '';


    public function __construct(\AvroSchema $schema = null)
    {

        $this->tenantId = getenv('TENANT_ID');
        
        Config::$pipelineName = getenv('PIPELINE_NAME');

        $registryUrl = [
            'base_uri' => 'http://' . getenv("SCHEMA_REGISTRY"),
            'timeout' => 0,
            'allow_redirects' => false,
            'headers' => ['Authorization' => "Bearer " . '0om47nyAr5YklzUdioGg21NTdgy56LPd'],
        ];

        $this->serializer = new MessageSerializer(new CachedSchemaRegistryClient($registryUrl));

        $this->defaultSchema = $schema;

    }

    public function deserialize(string $record) : array
    {
        $datum = [];

        try {

            $datum = $this->serializer->decodeMessage($record);

        } catch (\Exception $e) {

        }

        return $datum;
    }

    public function serialize(array $record) : string
    {

        $recordsWithSchema = false;
        
        try {

            $subject = $this->tenantId . '_' . Config::$pipelineName;

            if ($record && $this->defaultSchema) {

                $recordsWithSchema = $this->serializer->encodeRecordWithSchema($subject, $this->defaultSchema, $record, false);
            }


        } catch (\Exception $e) {

            // unique error message
//            $this->validationErrors[] = $e->getMessage();
//            $this->validationErrors[] = $e->getLine();
//            $this->validationErrors[] = $e->getFile();

            throw $e;
        }

        return $recordsWithSchema;

    }

}