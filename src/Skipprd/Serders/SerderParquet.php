<?php


namespace Skipprd\Serders;

use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;

class SerderParquet implements SerderBatchInterface
{

    private $parquet;
    
    private $converter;

    public $parquetSchema;

    public function __construct(\AvroSchema $schema = null) {

        if (!empty($schema)) {
            $this->converter = new AvroParquetSchemaConverter();
            $this->parquetSchema = $this->converter->convert(Config::$avroSchema);
        }

    }

    public function deserialize(string $payload): array
    {

        throw new \Exception("Method not implemented");

    }

    public function serialize(array $records, string $filename): void
    {

        $this->parquet = new \Parquet();
        
        try {

            if (!empty($records)) {
                
                $this->parquet->create_writer($filename, $this->parquetSchema, 'snappy');

                foreach ($records as $record) {

                    $this->parquet->write([$record]);
                }

                $this->parquet->close_writer();

            }


        } catch (\Exception $exception) {

                        var_export($this->parquetSchema);
                        print("\n");

//                        var_export($records);
//                        print("\n");

                        print($exception->getMessage());
//                        print($exception->getTraceAsString());

            exit(1);
        }
    }
}