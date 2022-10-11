<?php


namespace Skipprd\Serders;

use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class SerderParquet implements SerderBatchInterface
{

    private $parquet = false;

    private $records = [];

    public function __construct()
    {
    }

    public function deserialize(string $payload): array
    {

        throw new \Exception("Method not implemented");
    }

    public function openWriter(string $filename, array $schema): void {

        if (!$this->parquet) {
            $this->parquet = new \Parquet();

            $this->parquet->create_writer($filename, $schema, 'snappy');
        }
    }

    public function closeWriter(): void {

        $this->parquet->write($this->records);

        $this->parquet->close_writer();

        unset($this->records);
        unset($this->parquet);
    }


    public function serialize(array $record): void
    {

        try {
            $this->records[] = $record;

            unset($record);
        } catch (\Exception $exception) {

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
                                // @todo - I think parquet and athena might support null now?
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
