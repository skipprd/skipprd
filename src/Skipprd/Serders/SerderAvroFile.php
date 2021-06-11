<?php

namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\Config;

class SerderAvroFile implements SerderBatchInterface
{

    protected $avroSchema = [];

    public function __construct(\AvroSchema $schema = null)
    {
        $this->avroSchema = $schema;
    }

    public function deserialize(string $payload): array
    {
        $data = [];

        try {

            $read_io = new \AvroStringIO($payload);
            $data_reader = new \AvroDataIOReader($read_io, new \AvroIODatumReader());

            foreach ($data_reader->data() as $datum) {
                $data[] = $datum;
            }

        } catch (\Exception $e) {

            throw $e;
        }

        return $data;
    }

    public function serialize(array $records, string $filename): void
    {
        
        try {

            if (!empty($records)) {

                $data_writer = \AvroDataIO::open_file($filename, 'w', $this->avroSchema);

                foreach ($records as $datum) {

                    $data_writer->append($datum);
                }

                $data_writer->close();
            }


        } catch (\Exception $e) {

            throw $e;
        }
    }

}