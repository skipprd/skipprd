<?php

namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class SerderAvroFile implements SerderBatchInterface
{

    public function __construct()
    {
    }

    public function deserialize(string $payload): array
    {
        $data = [];

        try {
            $read_io = new \AvroStringIO($payload);
            $data_reader = new \AvroDataIOReader($read_io,
                new \AvroIODatumReader());

            foreach ($data_reader->data() as $datum) {
                $data[] = $datum;
            }
        } catch (\Exception $e) {
            throw $e;
        }

        return $data;
    }

    public function serialize(
        array $records,
        string $filename,
        $schema = null
    ): void {

        try {
            if (!empty($records)) {
                $data_writer = \AvroDataIO::open_file($filename, 'w', $schema);

                foreach ($records as $datum) {
                    $data_writer->append($datum);
                }

                $data_writer->close();
            }
        } catch (\Exception $e) {
            throw $e;
        }
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
                                $message[$field['name']] = ['' => ''];
//                                $message[$field['name']] = ['' => null];
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
