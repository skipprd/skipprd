<?php

namespace Feature\Skipprd;


use Skipprd\Commands\PipelineCommand;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Helpers;
use Skipprd\Traits\Config;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;

class DiscoverSchemaTest extends TestCase
{

    public function setUp(): void {

        parent::setUp();
    }

    public function tearDown(): void
    {
        parent::tearDown();
    }

    public function discoverSchema($container, $field, $value)
    {

        // Discover Schema
//        $container->resolveFieldType(Config::$discoveredFieldOccurrence, $field, $value);

        $payload = [];
        $payload[$field] = $value;

//        $container->serder = 'json';
//        $container->schema(json_encode($payload));
        $container->analysePayload($payload, Config::$discoveredFieldOccurrence);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence);

        $skippr_schema = Config::$discoveredFieldOccurrence;

        $field = Helpers::cleanFieldName($field);
        
        $avroType = Config::$discoveredFieldOccurrence[$field]['determined_type'];

        $avroFieldSchema = [];

        SkipprAvroSchemaConverter::buildAvroFields($avroFieldSchema, $field, $avroType,
            Config::$discoveredFieldOccurrence);

        return $avroFieldSchema;
    }

    public function testBuildComplexSchema() {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = [
            'sheep' => 'dog',
            'arable' => false,
            'crank' => [
                'voltage' => [2,3,4,6,7,4,3,6,7,9],
                'start_temprature' => 5,
                'end_temprature' => 7,
                'engine' => [
                    'manufacturer' => 'General Electric',
                    'model' => 'PZ - 09 - 126178',
                    'rebuild_dates' => [
                        0 => '01/02/19/85',
                        1 => '15/06/19/2005',
                    ],
                ]
            ],
            'neighbours' => [
                0 => 'Westfields Farm',
                1 => 'Leeway Holdings',
                2 => 'Jolly Rodgers Hoedown',
            ],
        ];

        $avroFieldSchema = $this->discoverSchema($container, $field, $value);

//        $this->assertEquals('skpr_event_ts', $avroFieldSchema[0]['name']);
//        $this->assertEquals('int', $avroFieldSchema[0]['type'][1]);

//        $this->assertEquals('skpr_partition', $avroFieldSchema[1]['name']);
//        $this->assertEquals('string', $avroFieldSchema[1]['type'][1]);

        $this->assertEquals('foo', $avroFieldSchema[0]['name']);
        $this->assertEquals('record', $avroFieldSchema[0]['type'][1]['type']);

        $this->assertEquals('sheep', $avroFieldSchema[0]['type'][1]['fields'][0]['name']);
        $this->assertEquals('string', $avroFieldSchema[0]['type'][1]['fields'][0]['type'][1]);

        $this->assertEquals('crank', $avroFieldSchema[0]['type'][1]['fields'][2]['name']);
        $this->assertEquals('record', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['type']);
        $this->assertEquals('voltage', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['fields'][0]['name']);
        $this->assertEquals('array', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['fields'][0]['type'][1]['type']);
        $this->assertEquals('int', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['fields'][0]['type'][1]['items']);

        $this->assertEquals('neighbours', $avroFieldSchema[0]['type'][1]['fields'][3]['name']);
        $this->assertEquals('array', $avroFieldSchema[0]['type'][1]['fields'][3]['type'][1]['type']);
        $this->assertEquals('string', $avroFieldSchema[0]['type'][1]['fields'][3]['type'][1]['items']);

    }

    public function testBuildArrayOfArraySchema()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'tags';
        $value = [
            [
                'name' => 'project',
                'value' => 'abc 123',
            ],
            [
                'name' => 'environment',
                'value' => 'dev',
            ],
        ];

        $avroFieldSchema = $this->discoverSchema($container,
            $field, $value);

        $this->assertEquals('tags', $avroFieldSchema[0]['name']);
        $this->assertEquals('record', $avroFieldSchema[0]['type'][1]['type']);
        $this->assertEquals('item_0', $avroFieldSchema[0]['type'][1]['fields'][0]['name']);
        $this->assertEquals('item_1', $avroFieldSchema[0]['type'][1]['fields'][1]['name']);
        $this->assertEquals('map', $avroFieldSchema[0]['type'][1]['fields'][0]['type'][1]['type']);
        $this->assertEquals('string', $avroFieldSchema[0]['type'][1]['fields'][0]['type'][1]['values']);
    }

    public function testBuildMapOfArrayStringsWithSubArraySchema()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        // twitter example
        // at first this looks like a map for stings...
        // but diving into the second level we see it's a record.
        $field = 'entities';
        $value = [
            'user_mentions' => [
                'indices' => [3, 19],
            ],
            'screen_name' => 'PostGradProblem',
            'id_str' => '271572434',
        ];

        $avroFieldSchema = $this->discoverSchema($container,
            $field, $value);

        $this->assertEquals('entities', $avroFieldSchema[0]['name']);
        $this->assertEquals('record', $avroFieldSchema[0]['type'][1]['type']);

    }

    public function testBuildEmptyArraySchema()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'urls';
        $value = [];

        $avroFieldSchema = $this->discoverSchema($container,
            $field, $value);

        $this->assertArrayNotHasKey(0, $avroFieldSchema);

    }


    public function testBuildNullValueSchema()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'urls';
        $value = null;

        $avroFieldSchema = $this->discoverSchema($container,
            $field, $value);

        $this->assertArrayHasKey(0, $avroFieldSchema);
        $this->assertEquals('null', $avroFieldSchema[0]['type'][0]);
        $this->assertNotContains(1, $avroFieldSchema[0]['type']);

    }
}

