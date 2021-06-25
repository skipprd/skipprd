<?php


namespace Skipprd\Serders;

use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\Config;

class SerderParquet implements SerderBatchInterface
{

    private $parquet;

    public function __construct() {

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
}