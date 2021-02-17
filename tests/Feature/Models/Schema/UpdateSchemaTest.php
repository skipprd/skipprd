<?php

namespace Feature\Models\Schema;

use App\IngestJob;
use App\Schema;
use App\User;
use Illuminate\Foundation\Testing\Concerns\InteractsWithConsole;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Commands\PollSqs;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;

class UpdateSchemaTest extends TestCase
{

    public function setUp() {

        parent::setUp();
    }

    public function tearDown()
    {
        parent::tearDown();
    }

    public function discoverSchema($container, $field, $value)
    {

        // Discover Schema
//        $container->resolveFieldType($container->discoveredFieldOccurrence, $field, $value);

        $payload = [];
        $payload[$field] = $value;

//        $container->serder = 'json';
//        $container->schema(json_encode($payload));
        $container->analysePayload($payload);

        $container->determineFieldTypes($container->discoveredFieldOccurrence);

        $avroType = $container->discoveredFieldOccurrence[$field]['determined_type'];

        $avroFieldSchema = [];

        IngestJob::buildAvroFields($avroFieldSchema, $field, $avroType,
            $container->discoveredFieldOccurrence);

        return $avroFieldSchema;
    }

    public function testBuildComplexSchema() {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

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

        $ingestId = random_int(100, 1000);

        $ingestJob = factory(IngestJob::class)->create([
            'id' => $ingestId,
            'name' => randomPassword(),
            'tenant_id' => 'net',
            'analysing' => 1,
            'enabled' => 1,
        ]);

        $schema = factory(Schema::class)->states('new_schema')->create([
            'id' => random_int(100, 1000),
            'ingest_job_id' => $ingestId,
        ]);

        $schema->schema = $avroFieldSchema;
        $schema->save();

        $schemaArr = $schema->schema;

        $this->assertEquals('skpr_event_ts', $schemaArr[0]['name']);
        $this->assertEquals('int', $schemaArr[0]['type'][1]);

        $this->assertEquals('skpr_partition', $schemaArr[1]['name']);
        $this->assertEquals('string', $schemaArr[1]['type'][1]);

        $this->assertEquals('foo', $schemaArr[2]['name']);
        $this->assertEquals('record', $schemaArr[2]['type'][1]['type']);

        $this->assertEquals('sheep', $schemaArr[2]['type'][1]['fields'][0]['name']);
        $this->assertEquals('string', $schemaArr[2]['type'][1]['fields'][0]['type'][1]);

        $this->assertEquals('crank', $schemaArr[2]['type'][1]['fields'][2]['name']);
        $this->assertEquals('record', $schemaArr[2]['type'][1]['fields'][2]['type'][1]['type']);
        $this->assertEquals('voltage', $schemaArr[2]['type'][1]['fields'][2]['type'][1]['fields'][0]['name']);
        $this->assertEquals('array', $schemaArr[2]['type'][1]['fields'][2]['type'][1]['fields'][0]['type'][1]['type']);
        $this->assertEquals('int', $schemaArr[2]['type'][1]['fields'][2]['type'][1]['fields'][0]['type'][1]['items']);

        $this->assertEquals('neighbours', $schemaArr[2]['type'][1]['fields'][3]['name']);
        $this->assertEquals('array', $schemaArr[2]['type'][1]['fields'][3]['type'][1]['type']);
        $this->assertEquals('string', $schemaArr[2]['type'][1]['fields'][3]['type'][1]['items']);

    }

    public function testBuildArrayOfArraySchema()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

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

        $ingestId = random_int(100, 1000);

        factory(IngestJob::class)->create([
            'id' => $ingestId,
            'name' => randomPassword(),
            'tenant_id' => 'net',
//            'source_job_id' => 1,
//            'output_job_id' => 1,
            'analysing' => 1,
            'enabled' => 1,
        ]);
        
        $schema = factory(Schema::class)->states('new_schema')->create([
            'id' => random_int(100, 1000),
            'ingest_job_id' => $ingestId,
        ]);

        $schema->schema = $avroFieldSchema;
        $schema->save();

        $schemaArr = $schema->schema;

        $this->assertEquals('skpr_event_ts', $schemaArr[0]['name']);
        $this->assertEquals('skpr_partition', $schemaArr[1]['name']);

        $this->assertEquals('tags', $schemaArr[2]['name']);
        $this->assertEquals('record', $schemaArr[2]['type'][1]['type']);
        $this->assertEquals('a0', $schemaArr[2]['type'][1]['fields'][0]['name']);
        $this->assertEquals('a1', $schemaArr[2]['type'][1]['fields'][1]['name']);
        $this->assertEquals('map', $schemaArr[2]['type'][1]['fields'][0]['type'][1]['type']);
        $this->assertEquals('string', $schemaArr[2]['type'][1]['fields'][0]['type'][1]['values']);
    }

    public function testBuildMapOfArrayStringsWithSubArraySchema()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

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

        $ingestId = random_int(100, 1000);

        factory(IngestJob::class)->create([
            'id' => $ingestId,
            'name' => randomPassword(),
            'tenant_id' => 'net',
            'analysing' => 1,
            'enabled' => 1,
        ]);

        $schema = factory(Schema::class)->states('new_schema')->create([
            'id' => random_int(100, 1000),
            'ingest_job_id' => $ingestId,
        ]);

        $schema->schema = $avroFieldSchema;
        $schema->save();

        $schemaArr = $schema->schema;

        $this->assertEquals('skpr_event_ts', $schemaArr[0]['name']);
        $this->assertEquals('skpr_partition', $schemaArr[1]['name']);

        $this->assertEquals('entities', $schemaArr[2]['name']);
        $this->assertEquals('record', $schemaArr[2]['type'][1]['type']);

    }

    public function testBuildEmptyArraySchema()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $field = 'urls';
        $value = [];

        $avroFieldSchema = $this->discoverSchema($container,
            $field, $value);

        $ingestId = random_int(100, 1000);

        factory(IngestJob::class)->create([
            'id' => $ingestId,
            'name' => randomPassword(),
            'tenant_id' => 'net',
            'analysing' => 1,
            'enabled' => 1,
        ]);

        $schema = factory(Schema::class)->states('new_schema')->create([
            'id' => random_int(100, 1000),
            'ingest_job_id' => $ingestId,
        ]);

        $schema->schema = $avroFieldSchema;
        $schema->save();

        $schemaArr = $schema->schema;

        $this->assertEquals('skpr_event_ts', $schemaArr[0]['name']);
        $this->assertEquals('skpr_partition', $schemaArr[1]['name']);

        $this->assertArrayNotHasKey(2, $schemaArr);

    }


    public function testBuildNullValueSchema()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $field = 'urls';
        $value = null;

        $avroFieldSchema = $this->discoverSchema($container,
            $field, $value);

        $ingestId = random_int(100, 1000);

        factory(IngestJob::class)->create([
            'id' => $ingestId,
            'name' => randomPassword(),
            'tenant_id' => 'net',
            'analysing' => 1,
            'enabled' => 1,
        ]);

        $schema = factory(Schema::class)->states('new_schema')->create([
            'id' => random_int(100, 1000),
            'ingest_job_id' => $ingestId,
        ]);

        $schema->schema = $avroFieldSchema;
        $schema->save();

        $schemaArr = $schema->schema;

        $this->assertEquals('skpr_event_ts', $schemaArr[0]['name']);
        $this->assertEquals('skpr_partition', $schemaArr[1]['name']);

        $this->assertArrayHasKey(0, $schemaArr);
        $this->assertEquals('null', $schemaArr[2]['type'][0]);
        $this->assertNotContains(1, $schemaArr[2]['type']);

    }
}

