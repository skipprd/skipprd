<?php

namespace Feature\Skipprd;

use legacy\src\Skipprd\Commands\PipelineCommand;
use legacy\src\Skipprd\Converters\SkipprAvroSchemaConverter;
use legacy\src\Skipprd\Serders\SerderJson;
use legacy\src\Skipprd\Traits\AnalyseSchema;
use legacy\src\Skipprd\Traits\Config;
use legacy\src\Skipprd\Traits\Ingest;
use Mockery;
use PHPUnit\Framework\TestCase;


class DefaultMessageTest extends TestCase
{

    use Ingest;

    public function buildSchema($record) {

        $avroFieldSchema = [];

        $sub_field_count = [];

        foreach ($record as $field => $value) {

            $avroType = Config::$discoveredFieldOccurrence['foo_namespace'][$field]['determined_type'];

            SkipprAvroSchemaConverter::buildAvroFields($avroFieldSchema, $field, $avroType, Config::$discoveredFieldOccurrence['foo_namespace'], [], $sub_field_count);
        }

        return $avroFieldSchema;
    }

    public function testDefaultMessage()
    {

//        Config::$tenantId = 'foo';
//        Config::$pipelineName = 'bar';

        Config::$discoveredFieldOccurrence['foo_namespace'] = [];

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

        $field = [
            'foo' => [
                'array' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9], // array
                'array_strings' => ['a', 'b', 'c'], // array
                'array_ints' => ["1" => 'a', "0" => 'b', "2" => 'c'], // array - null
                'map_ints' => ["a" => 123, "b" => 456, "c" => 789], // map - null
                'map_strings' => ["a" => 'x', "b" => 'y', "c" =>'z'], // map - null
                'record' => [
                    'a0' => 'abc',
                    'a1' => 123,
                    'a2' => 0,
                    'a3' => 123.456,
                ], // record - record
                'float_field' => 1.1,
                'int_field' => 100,
                'bool_field' => true,
                'string_field' => 'a string',
            ],
        ];

        AnalyseSchema::analysePayload($field, Config::$discoveredFieldOccurrence['foo_namespace']);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence['foo_namespace']);

        $avroFieldSchema = $this->buildSchema($field);
        Config::$schema['foo_namespace'] = Config::schemaMerge(Config::$specialFieldsMapping, $avroFieldSchema);

        // Test default message values (empty array, maps and records
        // Particularly relevant for serder to parquet

        $serde = new SerderJson();
        $container->defaultMsg = $serde->defaultMessage(Config::$schema['foo_namespace']);

        $this->assertArrayHasKey('foo', $container->defaultMsg);

        $this->assertSame(null, $container->defaultMsg['foo']['float_field']);
        $this->assertSame(null, $container->defaultMsg['foo']['int_field']);
        $this->assertSame(null, $container->defaultMsg['foo']['bool_field']);
        $this->assertSame(null, $container->defaultMsg['foo']['string_field']);

        $this->assertSame([], $container->defaultMsg['foo']['array']);
        $this->assertSame([], $container->defaultMsg['foo']['array_strings']);
        $this->assertSame([], $container->defaultMsg['foo']['array_ints']);

        // map of ints
        $this->assertSame(['' => null], $container->defaultMsg['foo']['map_ints']);

        // map of strings
        $this->assertSame(['' => ''], $container->defaultMsg['foo']['map_strings']);

        // record
        $recordDefault = [
            'a0' => NULL,
            'a1' => NULL,
            'a2' => NULL,
            'a3' => NULL,
        ];
        $this->assertSame($recordDefault, $container->defaultMsg['foo']['record']);

    }
}
