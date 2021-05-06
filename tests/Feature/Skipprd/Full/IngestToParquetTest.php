<?php

namespace Feature\Skipprd\Full;

use PHPUnit\Framework\TestCase;
use Skipprd\Buffers\BufferAdaptorsFactory;
use Skipprd\Commands\PipelineCommand;
use Mockery;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;
use Tests\Integration\DockerRun;


class IngestToParquetTest extends TestCase
{

    use Ingest;

    public function setUp() {

        parent::setUp();
    }

    public function buildSchema($record) {

         $avroFieldSchema = [];

        $sub_field_count = [];

        foreach ($record as $field => $value) {

            $avroType = Config::$discoveredFieldOccurrence[$field]['determined_type'];

            SkipprAvroSchemaConverter::buildAvroFields($avroFieldSchema, $field, $avroType, Config::$discoveredFieldOccurrence, [], $sub_field_count);
        }

        return $avroFieldSchema;
    }

    public function testDefaultMessage()
    {

        Config::$tenantId = 'foo';
        Config::$pipelineName = 'bar';

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

        $containerDockerRun = Mockery::mock(DockerRun::class)->makePartial();

        $fields = [
            [
                'foo' => [
                    'abc1' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9], // array
                    'abc2' => ['a', 'b', 'c'], // array
                    'abc3' => ["0" => 'a', "1" => 'b', "2" => 'c'], // array
                    'abc4' => ["1" => 'a', "0" => 'b', "2" => 'c'], // array - null
                    'abc5' => ["a" => 123, "b" => 456, "c" => 789], // map - null
                    'abc6' => [
                        'a0' => 'abc',
                        'a1' => 123,
                        'a2' => 0,
                        'a3' => 123.456,
                    ], // record - record
                ],
            ],
//            [
//                'foo' => [
//                    'abc1' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9], // array
//                    'abc2' => ['a', 'b', 'c'], // array
//                    'abc3' => ["0" => 'a', "1" => 'b', "2" => 'c'], // array
//                    'abc4' => ["1" => 'a', "0" => 'b', "2" => 'c'], // array - null
//                    'abc5' => ["a" => 123, "b" => 456, "c" => 789], // map - null
//                    'abc6' => ['a0' => 'abc',
//                        'a1' => 123,
//                        'a2' => 0,
//                        'a3' => 123.456,
//                      ], // record - record
//                ],
//            ],
            [
                'foo' => [
                    'abc1' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9], // array
                    'abc2' => ['a', 'b', 'c'], // array
                    'abc3' => ["0" => 'a', "1" => 'b', "2" => 'c'], // array
                    'abc4' => ["1" => 'a', "0" => 'b', "2" => 'c'], // array - null
                    'abc5' => ["a" => 123, "b" => 456, "c" => 789], // map - null
                    'abc6' => [
                        'a0' => 'abc',
                        'a1' => 123,
                        'a2' => 0,
                        'a3' => 123.456,
                    ], // record - record
                ],
            ],
        ];

        foreach ($fields as $field) {
            $container->analysePayload($field, Config::$discoveredFieldOccurrence);
        }

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence);

        $avroFieldSchema = $this->buildSchema($fields[0]);
        Config::$schema['fields'] = Config::schemaMerge(Config::$specialFieldsMapping, $avroFieldSchema);
        Config::$avroSchema = Config::buildAvroSchema();

        Config::$analysing = false;

        // Test default message values (empty array, maps and records
        // Particularly relevant for serder to parquet
        $container->defaultMsg = $this->defaultMessage(Config::$schema['fields']);

        $this->assertArrayHasKey('foo', $container->defaultMsg);

        $this->assertequals([], $container->defaultMsg['foo']['abc1']);
        $this->assertequals([], $container->defaultMsg['foo']['abc2']);
        $this->assertequals([], $container->defaultMsg['foo']['abc3']);
        $this->assertequals([], $container->defaultMsg['foo']['abc4']);
        $this->assertequals(['' => null], $container->defaultMsg['foo']['abc5']);

        $recordDefault = [
            'a0' => NULL,
            'a1' => NULL,
            'a2' => NULL,
            'a3' => NULL,
        ];
        $this->assertequals($recordDefault, $container->defaultMsg['foo']['abc6']);

        // Ingest messages
        Config::$dataDir = '/tmp';
        Config::$outputFormat = 'parquet';
        $buffer = BufferAdaptorsFactory::getAdaptor('output', 'file');

        foreach ($fields as $field) {

            $message = $container->ingestPayload($field, Config::$discoveredFieldOccurrence);

            $buffer->append($message, false);

        }

        $buffer->flush('output');

        $buffer->finalise(true);

//        $dockerrun = new DockerRun();
//
//        $path = Config::$dataDir . '/buffer';
//        $dockerrun->assertParquetOutput($buffer->serde->parquetSchema, $path);

    }
}
