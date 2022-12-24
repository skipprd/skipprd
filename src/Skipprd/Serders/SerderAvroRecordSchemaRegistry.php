<?php

namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderStreamInterface;
use Skipprd\Services\AvroSubPub\CachedSchemaRegistryClient;
use Skipprd\Services\AvroSubPub\MessageSerializer;
use Skipprd\Str;
use Skipprd\Traits\Config;
use Skipprd\SkipprLogger;

class SerderAvroRecordSchemaRegistry implements SerderStreamInterface
{

    public $compressionType = self::NO_COMPRESSION;

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

    public function serialize(array $record, $schema = null) : void
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

//        return $recordsWithSchema;
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

    public function defaultMessage(array $schema = []): array
    {

        try {
            // Init with internal special fields
            if (empty($schema)) {
                $message = Config::$specialFields;
            }

            foreach ($schema as $i => $field) {
                if (!empty($field['type'][1]['fields'])) {
                    $message[$field['name']] = $this->defaultMessage($field['type'][1]['fields']);
                } else {
                    if (!empty($field['type'][1]['type'])) {
                        if ($field['type'][1] == 'record') {
                            $message[$field['name']] = ['' => null];
                        } elseif ($field['type'][1]['type'] == 'array') {
                            $message[$field['name']] = [];
                        } elseif ($field['type'][1]['type'] == 'map') {
                            if ($field['type'][1]['values'] == 'string') {
//                                $message[$field['name']] = ['' => ''];
                                $message[$field['name']] = ['' => null];
                            }
                            if ($field['type'][1]['values'] == 'int') {
//                                $message[$field['name']] = ['' => 0];
                                $message[$field['name']] = ['' => null];
                            }
                        }
                    } else {
                        $message[$field['name']] = null;
                    }
                }
            }
        } catch (\Exception $e) {
            SkipprLogger::error('Unable to build default message.');
            throw $e;
        }

        return $message;
    }
}
