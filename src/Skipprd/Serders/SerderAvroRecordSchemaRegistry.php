<?php

namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderStreamInterface;
use Skipprd\Services\AvroSubPub\CachedSchemaRegistryClient;
use Skipprd\Services\AvroSubPub\MessageSerializer;
use Skipprd\Str;
use Skipprd\Traits\Config;

class SerderAvroRecordSchemaRegistry implements SerderStreamInterface
{

    protected $tenantId = '';

    protected $pipelineName = '';


    public function __construct()
    {

        $this->tenantId = getenv('TENANT_ID');
        
        $this->pipelineName = getenv('PIPELINE_NAME');
    }

    public function deserialize(string $record) : array
    {
        $datum = [];

        try {
            $datum = $this->decodeMessage($record);
        } catch (\Exception $e) {
        }

        return $datum;
    }

    public function serialize(array $record, $schema = null) : string
    {

        $recordsWithSchema = false;
        
        try {
            if ($record && $schema) {
                $recordsWithSchema = $this->encodeRecordWithSchema($schema, $record);
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

    /**
     * Decode a message from kafka that has been encoded for use with the schema registry.
     *
     * @param string $message
     *
     * @return array
     */
    public function decodeMessage($message)
    {
        if (strlen($message) < 1) {
            throw new \RuntimeException('Message is too small to decode');
        }

        $io = new AvroStringIO($message);

        $magic = unpack('C', $io->read(1));
        $magic = $magic[1];

        switch ($magic) {
            case static::MAGIC_BYTE_SCHEMAID:
                $id = unpack('N', $io->read(4));
                $id = $id[1];

                $decoder = $this->getDecoderById($id);
                break;
            case static::MAGIC_BYTE_SUBJECT_VERSION:
                $size = $io->read(4);
                $subjectSize = unpack('N', $size);
                $subjectBytes = unpack('C*', $io->read($subjectSize[1]));
                $version = unpack('N', $io->read(4));

                $version = $version[1];

                $subject = '';
                foreach ($subjectBytes as $subjectByte) {
                    $subject .= chr($subjectByte);
                }

                $decoder = $this->getDecoderBySubjectAndVersion($subject, $version);
                break;
            default:
                return $message;
        }

        return $decoder($io);
    }

    /**
     * Given a parsed avro schema, encode a record for the given topic.
     * The schema is registered with the subject of 'topic-value'
     *
     * @param string $topic Topic name
     * @param \AvroSchema $schema Avro Schema
     * @param array $record An object to serialize
     * @param bool $isKey If the record is a key
     *
     * @return string Encoded record with schema ID as bytes
     */
    public function encodeRecordWithSchema(\AvroSchema $schema, array $record)
    {

        $writer = new \AvroIODatumWriter($schema);

        $io = new \AvroStringIO();

        // write the header

        // magic byte
        $io->write(pack('C', static::MAGIC_BYTE_SCHEMAID));

        // write the schema ID in network byte order (big end)
        $io->write(pack('N', $schemaId));

        // write the record to the rest of it
        // Create an encoder that we'll write to
        $encoder = new \AvroIOBinaryEncoder($io);

        // write the object in 'obj' as Avro to the fake file...
        $writer->write($record, $encoder);

        return $io->string();
    }
}
