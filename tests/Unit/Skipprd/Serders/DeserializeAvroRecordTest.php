<?php

namespace Unit\Skipprd\Serders;

use Skipprd\Serders\SerdersFactory;
use Tests\TestCase;

class DeserializeAvroRecordTest extends TestCase
{

    protected $format;

    protected $serder;

    protected function setUp()
    {
        parent::setUp();

        $this->format = 'avro_record';

    }

    public function writeDatum(array $records, string $writers_schema_json)
    {

        $avroSchema = \AvroSchema::parse($writers_schema_json);

        $this->serder = SerdersFactory::factory($this->format, $avroSchema);

        $io = new \AvroStringIO();

        $writer = new \AvroIODatumWriter($avroSchema);
        $data_writer = new \AvroDataIOWriter($io, $writer, $avroSchema);

        foreach ($records as $datum) {

            $data_writer->append($datum);
        }

        $data_writer->close();

        return $binary_string = $io->string();

    }

    public function testValidAvro()
    {

        $writers_schema_json = <<<_JSON
{"name":"member",
 "type":"record",
 "fields":[{"name":"member_id", "type":"int"},
           {"name":"name", "type":"string"}]}
_JSON;

        $data = array('member_id' => 1392, 'name' => 'Paul Hudson');

//        $record = $this->writeDatum($data, $writers_schema_json);

        $avroSchema = \AvroSchema::parse($writers_schema_json);

        $this->serder = SerdersFactory::factory($this->format, $avroSchema);

        $record = $this->serder->serialize($data);
        $msg = $this->serder->deserialize($record);

        $this->assertEquals('Paul Hudson', $msg[0]['name']);
        $this->assertEquals(1392, $msg[0]['member_id']);

    }

    public function testValidMultipleAvro()
    {

        $writers_schema_json = <<<_JSON
{"name":"member",
 "type":"record",
 "fields":[{"name":"member_id", "type":"int"},
           {"name":"name", "type":"string"}]}
_JSON;

        $data[] = array('member_id' => 1, 'name' => 'Paul Hudson');
        $data[] = array('member_id' => 2, 'name' => 'Natalia Hudson');

        $record = $this->writeDatum($data, $writers_schema_json);

        $msg = $this->serder->deserialize($record);

        $this->assertEquals('Paul Hudson', $msg[0]['name']);
        $this->assertEquals(1, $msg[0]['member_id']);

        $this->assertEquals('Natalia Hudson', $msg[1]['name']);
        $this->assertEquals(2, $msg[1]['member_id']);

    }
}