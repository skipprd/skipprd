<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Traits\Serders;

use Illuminate\Contracts\Container\Container;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Services\AvroSubPub\MessageSerializer;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Ingest;
use Skipprd\Serders\SerdersFactory;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Superbalist\PubSub\Utils;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class AnalyseSchemaParseRecordAvroTest extends TestCase
{

    use AnalyseSchema;
    use Ingest;

    protected function setUp()
    {
        parent::setUp();

    }

//    public function testValidAvro()
//    {
//
//        $serder = 'avro';
//
//        $writers_schema_json = <<<_JSON
//{"name":"member",
// "type":"record",
// "fields":[{"name":"member_id", "type":"int"},
//           {"name":"name", "type":"string"}]}
//_JSON;
//
//        $data[] = array('member_id' => 1392, 'name' => 'Paul Hudson');
//
//        $writers_schema = AvroSchema::emitArray($writers_schema_json);
//
//        $serder = SerdersFactory::factory($serder, $writers_schema);
//
//        foreach ($data as $datum) {
//            $record = $serder->serialize($datum);
//            $msg[] = $serder->deserialize($record);
//        }
//
//        $this->assertEquals('Paul Hudson', $msg[0]['name']);
//        $this->assertEquals(1392, $msg[0]['member_id']);
//
//    }
//
//    public function testValidMultipleAvro()
//    {
//
//        $serder = 'avro';
//
//        $writers_schema_json = <<<_JSON
//{"name":"member",
// "type":"record",
// "fields":[{"name":"member_id", "type":"int"},
//           {"name":"name", "type":"string"}]}
//_JSON;
//
//        $data[] = array('member_id' => 1, 'name' => 'Paul Hudson');
//        $data[] = array('member_id' => 2, 'name' => 'Natalia Hudson');
//
//        $writers_schema = AvroSchema::emitArray($writers_schema_json);
//
//        $serder = SerdersFactory::factory($serder, $writers_schema);
//
//        foreach ($data as $datum) {
//            $record = $serder->serialize($datum);
//            $msg[] = $serder->deserialize($record);
//        }
//
//        $this->assertEquals('Paul Hudson', $msg[0]['name']);
//        $this->assertEquals(1, $msg[0]['member_id']);
//
//        $this->assertEquals('Natalia Hudson', $msg[1]['name']);
//        $this->assertEquals(2, $msg[1]['member_id']);
//
//    }

//    public function testValidAvroBinary()
//    {
//
//        $writers_schema_json = <<<_JSON
//{"name":"member",
// "type":"record",
// "fields":[{"name":"member_id", "type":"int"},
//           {"name":"name", "type":"string"}]}
//_JSON;
//
//        $data = array('member_id' => 1392, 'name' => 'Paul Hudson');
//
//        $schema = AvroSchema::emitArray($writers_schema_json);
//
//        $io = $this->encodeRecordWithSchema($schema, $data);
//
//
//        $record = $io;
//
//        $container = Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//
//        $msg = $container->factory($record);
//
//        $this->assertEquals('Paul Hudson', $msg[0]['name']);
//        $this->assertEquals(1392, $msg[0]['member_id']);
//
//    }
//
//    public function encodeRecordWithSchema(AvroSchema $schema, array $record)
//    {
//
//        $subject = 'foo';
//        $version = 1;
//
//        $writer = new \AvroIODatumWriter($schema);
//
//
//        $io = new \AvroStringIO();
//
//        // write the header
//
//        // magic byte
//        $io->write(pack('C', 1));
//
//        // write the subject length in network byte order (big end)
//        $io->write(pack('N', strlen($subject)));
//
//        // then the subject
//        foreach (str_split($subject) as $letter) {
//            $io->write(pack('C', ord($letter)));
//        }
//
//        // and finally the version
//        $io->write(pack('N', $version));
//
//        // write the record to the rest of it
//        // Create an encoder that we'll write to
//        $encoder = new \AvroIOBinaryEncoder($io);
//
//        // write the object in 'obj' as Avro to the fake file...
//        $writer->write($record, $encoder);
//
//        return $io->string();
//    }
}