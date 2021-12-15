<?php


namespace Skipprd\Serders;

use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class SerderParquet implements SerderBatchInterface
{

    private $parquet;

    public function __construct()
    {
    }

    public function deserialize(string $payload): array
    {

        throw new \Exception("Method not implemented");
    }

    public function serialize(array $records, string $filename, $schema = null): void
    {

        $this->parquet = new \Parquet();
        
        try {
            if (!empty($records)) {
                $this->parquet->create_writer($filename, $schema, 'snappy');

                foreach ($records as $record) {
                    $this->parquet->write([$record]);
                }

                $this->parquet->close_writer();
            }
        } catch (\Exception $exception) {
                        var_export($schema);
                        print("\n");

//                        var_export($records);
//                        print("\n");

                        print($exception->getMessage());
//                        print($exception->getTraceAsString());

            exit(1);
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
                                $message[$field['name']] = ['' => 0];
//                                $message[$field['name']] = ['' => null];
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
