<?php

namespace Feature\Skipprd;

use PHPUnit\Framework\TestCase;
use Skipprd\Commands\PipelineCommand;
use Mockery;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;


class DefaultMessageTest extends TestCase
{

    use Ingest;

    public function buildSchema($record) {

        $avroFieldSchema = [];

        $sub_field_count = [];

        foreach ($record as $field => $value) {

            $avroType = Config::$discoveredFieldOccurrence['foo_partition'][$field]['determined_type'];

            SkipprAvroSchemaConverter::buildAvroFields($avroFieldSchema, $field, $avroType, Config::$discoveredFieldOccurrence['foo_partition'], [], $sub_field_count);
        }

        return $avroFieldSchema;
    }

    public function testDefaultMessage()
    {

//        Config::$tenantId = 'foo';
//        Config::$pipelineName = 'bar';

        Config::$discoveredFieldOccurrence['foo_partition'] = [];

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
                'abc6' => ["a" => 'x', "b" => 'y', "c" =>'z'], // map - null
                'abc7' => [
                    'a0' => 'abc',
                    'a1' => 123,
                    'a2' => 0,
                    'a3' => 123.456,
                ], // record - record
            ],
        ];

        $container->analysePayload($field, Config::$discoveredFieldOccurrence['foo_partition']);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence['foo_partition']);

        $avroFieldSchema = $this->buildSchema($field);
        Config::$schema['fields'] = Config::schemaMerge(Config::$specialFieldsMapping, $avroFieldSchema);

        // Test default message values (empty array, maps and records
        // Particularly relevant for serder to parquet
        $container->defaultMsg = $this->defaultMessage(Config::$schema['fields']);

        $this->assertArrayHasKey('foo', $container->defaultMsg);

        $this->assertequals([], $container->defaultMsg['foo']['abc1']);
        $this->assertequals([], $container->defaultMsg['foo']['abc2']);
        $this->assertequals([], $container->defaultMsg['foo']['abc3']);
        $this->assertequals([], $container->defaultMsg['foo']['abc4']);

        // map of ints
        $this->assertequals(['' => 0], $container->defaultMsg['foo']['abc5']);

        // map of strings
        $this->assertequals(['' => null], $container->defaultMsg['foo']['abc6']);

        // record
        $recordDefault = [
            'a0' => NULL,
            'a1' => NULL,
            'a2' => NULL,
            'a3' => NULL,
        ];
        $this->assertequals($recordDefault, $container->defaultMsg['foo']['abc7']);

    }
}
