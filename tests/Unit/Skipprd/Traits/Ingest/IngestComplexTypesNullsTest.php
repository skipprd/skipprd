<?php

namespace Unit\Skipprd\Traits\Ingest;

use PHPUnit\Framework\TestCase;
use Skipprd\Commands\PipelineCommand;
use Mockery;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;


class IngestComplexTypesNullsTest extends TestCase
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

        $field = [
            'foo' => [
                'abc1' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9], // array
                'abc2' => ['a', 'b', 'c'], // array
                'abc3' => ["0" => 'a', "1" => 'b', "2" => 'c'], // array
                'abc4' => ["1" => 'a', "0" => 'b', "2" => 'c'], // array - null
                'abc5' => ["a" => 123, "b" => 456, "c" => 789], // map - null
                'abc6' => ["abc", 123, null, 123.456], // record - record
            ]
        ];

        $container->analysePayload($field, Config::$discoveredFieldOccurrence);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence);

        $avroFieldSchema = $this->buildSchema($field);
        Config::$schema['fields'] = Config::schemaMerge(Config::$specialFieldsMapping, $avroFieldSchema);
        Config::$avroSchema = Config::buildAvroSchema();

        // Test default message values (empty array, maps and records
        // Particularly relevant for serder to parquet
        $defaultMessage = $this->defaultMessage(Config::$schema['fields']);

        $this->assertArrayHasKey('foo', $defaultMessage);

        $this->assertequals([], $defaultMessage['foo']['abc1']);
        $this->assertequals([], $defaultMessage['foo']['abc2']);
        $this->assertequals([], $defaultMessage['foo']['abc3']);
        $this->assertequals([], $defaultMessage['foo']['abc4']);
        $this->assertequals(['' => null], $defaultMessage['foo']['abc5']);

        $recordDefault = [
            'a0' => NULL,
            'a1' => NULL,
            'a2' => NULL,
            'a3' => NULL,
        ];
        $this->assertequals($recordDefault, $defaultMessage['foo']['abc6']);

        $message = $container->ingestPayload($defaultMessage, Config::$discoveredFieldOccurrence);

        

    }
}
