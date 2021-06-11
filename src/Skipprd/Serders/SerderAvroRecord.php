<?php

namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderStreamInterface;

class SerderAvroRecord implements SerderStreamInterface
{

    protected $avroSchema = [];

    public function __construct(\AvroSchema $schema = null)
    {
        $this->avroSchema = $schema;
    }

    public function deserialize(string $record) : array
    {
        $data = [];

        try {

            $read_io = new \AvroStringIO($record);
            $data_reader = new \AvroDataIOReader($read_io, new \AvroIODatumReader());

            foreach ($data_reader->data() as $datum)
                $data[] = $datum;

        } catch (\Exception $e) {

        }

        return $data;
    }

    public function serialize(array $record) : string
    {

        try {

            $io = new \AvroStringIO();

            $writer = new \AvroIODatumWriter($this->avroSchema);
            $data_writer = new \AvroDataIOWriter($io, $writer, $this->avroSchema);

            $data_writer->append($record);

            $data_writer->close();

            return $io->string();

        } catch (\Exception $e) {

            throw $e;
        }

    }
}