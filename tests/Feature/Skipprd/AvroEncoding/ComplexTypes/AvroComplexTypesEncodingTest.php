<?php

namespace Feature\Skipprd\AvroEncoding\ComplexTypes;

use App\IngestJob;
use App\Schema;
use App\User;
use Illuminate\Contracts\Container\Container;
use Illuminate\Support\Facades\Auth;
use Illuminate\Support\Facades\DB;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Services\AvroSubPub\CachedSchemaRegistryClient;
use Skipprd\Services\AvroSubPub\MessageSerializer;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class AvroComplexTypesEncodingTest extends TestCase
{

    public $valueAvroSchema = [];

    protected function setUp()
    {
        parent::setUp();

        $this->valueAvroSchema['namespace'] = 'test.avro';
        $this->valueAvroSchema['name'] = 'test';
        $this->valueAvroSchema['type'] = 'record';
        $this->valueAvroSchema['fields'] = [];

        $this->user = factory(User::class)->create([
            'name' => 'xyzuser',
            'email' => 'user@xyz.com',
            'password' => randomPassword(12),
            'api_token' => str_random(60),
            'tenant_id' => 'xyztest',
        ]);

        DB::table('model_has_roles')
            ->insert([
                'role_id' => 1,
                'model_id' => $this->user->id,
                'model_type' => 'App\User',
            ]);

    }

    public function discoverAndSaveSchema($container, $field, $value)
    {

        // Discover Schema
//        $json = json_encode([$field => $value]);
        $container->analysePayload([$field => $value], Config::$discoveredFieldOccurrence);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence);

        $avroType = Config::$discoveredFieldOccurrence[$field]['determined_type'];

        $avroFieldSchema = [];

        IngestJob::buildAvroFields($avroFieldSchema, $field, $avroType,
            Config::$discoveredFieldOccurrence);

        // now save schema to DB so it's avalibe to scheam API
        $ingestId = random_int(100, 1000);

        $ingestJob = factory(IngestJob::class)->create([
            'id' => $ingestId,
            'name' => randomPassword(),
            'tenant_id' => $this->user->tenant_id,
            'analysing' => 1,
            'enabled' => 1,
        ]);

        $schemaName = 'test';
        $schemaNamespace = "io.skippr." . $this->user->tenant_id . "." . $schemaName;

        $schema = factory(Schema::class)->create([
            'subject' => $schemaName . '-value',
            'name' => $schemaName,
            'type' => 'AVRO',
            'namespace' => $schemaNamespace,
            'tenant_id' => $this->user->tenant_id,
            'ingest_job_id' =>random_int(100, 1000),
        ]);

        $schema->schema = $avroFieldSchema;
        $schema->save();


        return $avroFieldSchema;
    }

    public function encodeWithSchema($container, $value, $field, $avroFieldSchema)
    {

        // Ingest Record
        $record = [$field => $value];

        $container->ingestField($field, $value, Config::$discoveredFieldOccurrence, $record);

        $recordsWithSchema = $this->encodeAvro($avroFieldSchema, $field, $record);

        return $recordsWithSchema;

    }

    public function avroSerde($name) {

        $headers = ['Authorization' => "Bearer " . $this->user->api_token];

        $response = $this->post('/subjects/' . $name, [], $headers);

        $container = Mockery::mock(CachedSchemaRegistryClient::class)->makePartial();
        $container->shouldReceive('sendRequest')
            ->andReturn([$response->getStatusCode(), json_decode($response->getContent(), true), true]);

        $serde = new MessageSerializer($container, []);

        return $serde;

    }

    public function encodeAvro($avroField, $field, $record)
    {

        $this->serde = $this->avroSerde('test-value');

        //        $avroField = array_merge($avroType, ['name' => $field]);
        $this->valueAvroSchema['fields'] = $avroField;
        $valueSchemaJson = json_encode($this->valueAvroSchema);
        $valueSchema = AvroSchema::parse($valueSchemaJson);

//        $this->serde->registry->cacheSchema($valueSchema, 1, 'test', 1);

        $recordsWithSchema = $this->serde->encodeRecordWithSchema('test', $valueSchema, $record, false, 1);

        return $recordsWithSchema;

    }

    public function decodeAvro($record)
    {

        $recordsWithSchema = $this->serde->decodeMessage($record);

        return $recordsWithSchema;

    }
    
    public function testGetLogicalTypeMapOfStrings()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [
            'blue_widget' => 'good',
            'green_widget' => 'bad'
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value['blue_widget'], $record['foo']['blue_widget']);

    }

    public function testGetLogicalTypeMapOfInts()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [
            'blue_widget' => 4,
            'green_widget' => 2,
            'yellow_widget' => 3
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value['blue_widget'], $record['foo']['blue_widget']);
    }

    public function testGetLogicalTypeMapOfStringAndInt()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [
            'blue_widget' => 'foo',
            'green_widget' => 1234,
            'yellow_widget' => 'bar'
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value['blue_widget'], $record['foo']['blue_widget']);
        $this->assertEquals($value['green_widget'], $record['foo']['green_widget']);
        $this->assertEquals($value['yellow_widget'], $record['foo']['yellow_widget']);
    }

//    public function testGetLogicalTypeMapOfStringAndIntStrings()
//    {
//
//        $container = Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();
//
//        $field = 'foo';
//        $value = [
//            'blue_widget' => 'foo',
//            'green_widget' => '1234',
//            'yellow_widget' => 'bar'
//        ];
//
//        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);
//
//        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);
//
//        $this->assertNotNull($recordsWithSchema);
//
//        $record = $this->decodeAvro($recordsWithSchema);
//
//        $this->assertArrayHasKey('foo', $record);
//        $this->assertEquals($value['blue_widget'], $record['foo']['blue_widget']);
//        $this->assertEquals($value['green_widget'], $record['foo']['green_widget']);
//        $this->assertEquals($value['yellow_widget'], $record['foo']['yellow_widget']);
//    }

    public function testGetLogicalTypeArrayOfStrings()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = ['sheep', 'sheep_dog'];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value[0], $record['foo'][0]);
    }


    public function testGetLogicalTypeArrayOfInts()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [
            0 => 2,
            1 => 4
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value[0], $record['foo'][0]);
    }

    public function testGetLogicalTypeKeylessArrayOfInts()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [2,3,4,6,7,4,3,6,7,9];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value[0], $record['foo'][0]);

    }

    public function testGetLogicalTypeRecordOneDimensional()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [
            'sheep' => 'dog',
            'arable' => false,
            'acres' => 105,
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value['sheep'], $record['foo']['sheep']);
        $this->assertEquals($value['arable'], $record['foo']['arable']);
        $this->assertEquals($value['acres'], $record['foo']['acres']);

    }

    public function testGetLogicalTypeRecordOneMultidimensional()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [
            'sheep' => 'dog',
            'arable' => false,
            'neighbours' => [
                0 => 'Westfields Farm',
                1 => 'Leeway Holdings',
                2 => 'Jolly Rodgers Hoedown',
            ],
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value['neighbours'], $record['foo']['neighbours']);
    }

    public function testGetLogicalTypeRecordComplexMultidimensional()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

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

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value['crank'], $record['foo']['crank']);
        $this->assertEquals($value['crank']['start_temprature'], $record['foo']['crank']['start_temprature']);
        $this->assertEquals($value['neighbours'], $record['foo']['neighbours']);
    }

    //////////////////////////////////////////////////


    public function testBuildArrayOfArraySchema()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
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

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('tags', $record);

        $this->assertEquals('project', $record['tags']['a0']['name']);
        $this->assertEquals('abc 123', $record['tags']['a0']['value']);
        $this->assertEquals('environment', $record['tags']['a1']['name']);
        $this->assertEquals('dev', $record['tags']['a1']['value']);

    }

    public function testBuildMapOfArrayStringsWithSubArraySchema()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        // twitter example
        // at first this looks like a map for stings...
        // but diving into the second level we see it's a record.
        $field = 'entities';
        $value = [
            'screen_name' => 'PostGradProblem',
            'user_mentions' => [
                [
                    'indices' => [3,3,4,6,7,4,3,6,7,9],
                    'screen_name' => 'PostGradProblem',
                    'id_str' => '271572434',
                    'name' => 'PostGradProblems',
                    'id' => 271572434,
                ]

            ],
            "urls" => [],
            "hashtags" => []
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('entities', $record);

        $this->assertEquals('PostGradProblem', $record['entities']['screen_name']);

        $this->assertEquals(3, $record['entities']['user_mentions']['a0']['indices'][0]);
        $this->assertEquals('PostGradProblem', $record['entities']['user_mentions']['a0']['screen_name']);
        $this->assertEquals('271572434', $record['entities']['user_mentions']['a0']['id_str']);
        $this->assertEquals('PostGradProblems', $record['entities']['user_mentions']['a0']['name']);
        $this->assertEquals('271572434', $record['entities']['user_mentions']['a0']['id']);
    }

    public function testBuildEmptyArraySchema()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'urls';
        $value = [];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayNotHasKey('urls', $record);
//        $this->assertEmpty('urls', $record['urls']);

    }

    public function testBuildNullValueSchema()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'urls';
        $value = null;

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('urls', $record);
        $this->assertEquals(null, $record['urls']);

    }

    public function testBuildNestedDuplicateFieldNames()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

//        $field = 'trace';
//        $value = [
//            'crank' => [
//                'voltage' => [2,3,4,6,7,4,3,6,7,9],
//            ],
//            'last_crank' => [
//                'voltage' => [2,3,4,6,7,4,3,6,7,9],
//            ],
//        ];

        $field = 'trace';
        $value = [
                'sheep' => 'dog',
                'arable' => false,
                'crank' => [
                    'voltage' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
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
                'history' => [
                    'crank' => [
                        'voltage' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
                        'start_temprature' => 5,
                    ]
                ],
                'neighbours' => [
                    0 => 'Westfields Farm',
                    1 => 'Leeway Holdings',
                    2 => 'Jolly Rodgers Hoedown',
                ],
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('trace', $record);

        $this->assertEquals(2, $record['trace']['crank']['voltage'][0]);
        $this->assertEquals(2, $record['trace']['history']['crank']['voltage'][0]);

    }

    public function testBuildNestedDuplicateRecord()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'twitter';
        $value = [
            'user_mentions' => [
                [
                    'indices' => [3,3,4,6,7,4,3,6,7,9],
                    'screen_name' => 'PostGradProblem',
                    'id_str' => '271572434',
                    'name' => 'PostGradProblems',
                    'id' => 271572434,
                ]

            ],
            'history' => [
                'user_mentions' => [
                    [
                        'indices' => [3,3,4,6,7,4,3,6,7,9],
                        'screen_name' => 'PostGradProblem',
                        'id_str' => '271572434',
                        'name' => 'PostGradProblems',
                        'id' => 271572434,
                    ]

                ],
            ],
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('twitter', $record);

        $this->assertEquals(3, $record['twitter']['user_mentions']['a0']['indices'][0]);
        $this->assertEquals(3, $record['twitter']['history']['user_mentions']['a0']['indices'][0]);

    }

    public function testBuildNestedEnsureFieldsNotRepeated()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'twitter';
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

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('twitter', $record);

        $this->assertEquals('paul', $record['twitter']['contact']['name']);
        $this->assertEquals('01.01', $record['twitter']['location']['start_geo']['lat']);

        $this->assertNotContains('name', $record['twitter']['location']);

    }

}
