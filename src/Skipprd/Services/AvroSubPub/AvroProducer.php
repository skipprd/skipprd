<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 09/11/2019
 * Time: 12:33
 */

namespace Skipprd\Services\AvroSubPub;

class AvroProducer
{

    private $pubsub;
    private $topic;
    /** @var MessageSerializer */
    private $serializer;
    private $defaultKeySchema;
    private $defaultValueSchema;
    
    public function __construct($subpub, $topic, $registryUrl, $defaultKeySchema = null, $defaultValueSchema = null, $options = [])
    {
        $this->pubsub = $subpub;
        $this->topic = $topic;
        $this->defaultKeySchema = $defaultKeySchema;
        $this->defaultValueSchema = $defaultValueSchema;
        $this->serializer = new MessageSerializer(new CachedSchemaRegistryClient($registryUrl), $options);
    }
    public function produce($value, $key = null, $keySchema = null, $valueSchema = null, $format = null)
    {
        $keySchema = $keySchema ?: $this->defaultKeySchema;
        $valueSchema = $valueSchema ?: $this->defaultValueSchema;
        if ($value && $valueSchema) {
            $value = $this->serializer->encodeRecordWithSchema($this->topic, $valueSchema, $value, false, $format);
        }
        if ($key && $keySchema) {
            $key = $this->serializer->encodeRecordWithSchema($this->topic, $keySchema, $key, true, $format);
        }


        $message = $value;

        $result = $this->pubsub->publish($this->topic, $message);

    }

    /**
     * @param int $timeout
     */
    public function flush(int $timeout)
    {
        $this->pubsub->getProducer()->flush($timeout);
    }
    
}