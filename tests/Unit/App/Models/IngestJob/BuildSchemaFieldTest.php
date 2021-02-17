<?php

namespace Unit\App\Models\IngestJob;

use App\IngestJob;
use App\Schema;
use App\User;
use Illuminate\Foundation\Testing\Concerns\InteractsWithConsole;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Support\Facades\Auth;
use Skipprd\Commands\PipelineCommand;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;

class BuildSchemaFieldTest extends TestCase
{
//    use DatabaseMigrations;
//    use RefreshDatabase;

//    use DatabaseMigrations, RefreshDatabase {
//        refreshDatabase as baseRefreshDatabase;
//    }
//
//    public $ingestJob;
//
//    public function refreshDatabase()
//    {
////        $this->baseRefreshDatabase();
//
//        // Seed the database on every database refresh.
//
//    }

    public function setUp() {

//        parent::tearDown();

        parent::setUp();
//        $this->artisan('db:seed');

//        $this->seed();
//        $this->artisan('db:seed');

//        $this->seed();

        $this->user = factory(User::class)->create([
            'name' => 'auth',
            'email' => 'auth@abc.com',
            'password' => randomPassword(12),
            'api_token' => 'fooooo',
            'tenant_id' => 'abc',
        ]);


    }

    public function tearDown()
    {
        parent::tearDown();
    }

    public function discoverAndEncodeWithSchema($container, $field, $value)
    {

        // Discover Schema
//        $container->resolveFieldType($container->discoveredFieldOccurrence, $field, $value);

        $payload = [];
        $payload[$field] = $value;

//        $container->serder = 'json';
        $container->analysePayload($payload);
//        $container->schema(json_encode($payload));

        $container->determineFieldTypes($container->discoveredFieldOccurrence);

        $avroType = $container->discoveredFieldOccurrence[$field]['determined_type'];

        $avroFieldSchema = [];

        // reset count or tests break
        IngestJob::$recordFieldNamesCount = [];

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

        $avroFieldSchema = $this->discoverAndEncodeWithSchema($container, $field, $value);

        $this->assertArrayHasKey(0, $avroFieldSchema);
        $this->assertEquals($field, $avroFieldSchema[0]['name']);

        $this->assertEquals('null', $avroFieldSchema[0]['type'][0]);
        $this->assertEquals('record', $avroFieldSchema[0]['type'][1]['type']);
        $this->assertEquals('sheep', $avroFieldSchema[0]['type'][1]['fields'][0]['name']);
        $this->assertEquals('null', $avroFieldSchema[0]['type'][1]['fields'][0]['type'][0]);
        $this->assertEquals('string', $avroFieldSchema[0]['type'][1]['fields'][0]['type'][1]);


        $this->assertEquals('crank', $avroFieldSchema[0]['type'][1]['fields'][2]['name']);
        $this->assertEquals('null', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][0]);
        $this->assertEquals('record', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['type']);
        $this->assertEquals('crank', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['name']);
        $this->assertEquals('voltage', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['fields'][0]['name']);
        $this->assertEquals('null', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['fields'][0]['type'][0]);
        $this->assertEquals('array', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['fields'][0]['type'][1]['type']);
        $this->assertEquals('int', $avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['fields'][0]['type'][1]['items']);
//        $this->assertIsArray($avroFieldSchema[0]['type'][1]['fields'][2]['type'][1]['fields'][0]['fields'][0]['voltage']);

    }


    /**
     * ensure one records fields don't bleed into another record
     */
    public function testBuildComplexNestedEnsureFieldsNotRepeated()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [
            'contact' => [
                'name' => 'paul',
                'postcode' => 'ss7 3ll',
                'email' => 'foo@foo.com',
                'profile' => [
                    'nickname' => 'plonka',
                ]
            ],
            'location' => [
                'start_geo' => [
                    'lat' => '01.01',
                    'lon' => '01.01',
                ],
                'end_geo' => [
                    'lat' => '01.01',
                    'lon' => '01.01',
                ],
            ]
        ];

        $avroFieldSchema = $this->discoverAndEncodeWithSchema($container,
            $field, $value);

        $this->assertArrayHasKey(0, $avroFieldSchema);
        $this->assertEquals($field, $avroFieldSchema[0]['name']);


        $this->assertEquals('start_geo',
            $avroFieldSchema[0]['type'][1]['fields'][1]['type'][1]['fields'][0]['name']);


        $this->assertEquals('end_geo',
            $avroFieldSchema[0]['type'][1]['fields'][1]['type'][1]['fields'][1]['name']);
    }
    
}
