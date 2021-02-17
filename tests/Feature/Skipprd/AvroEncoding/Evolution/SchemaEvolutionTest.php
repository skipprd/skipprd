<?php

namespace Feature\Skipprd\AvroEncoding\Evolution;

use App\IngestJob;
use App\Schema;
use App\User;
use Illuminate\Foundation\Testing\Concerns\InteractsWithConsole;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Support\Facades\DB;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Services\AvroSubPub\CachedSchemaRegistryClient;
use Skipprd\Services\AvroSubPub\MessageSerializer;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;
use Mockery;
use AvroSchema;
use function Aws\map;

class SchemaEvolutionTest extends TestCase
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

    public static function buildSchemaFieldList($schema, $schemaFieldNames = []) {

        $foo = '';
        
        foreach ($schema as $key => $val) {
            if (is_array($val)) {
                $schemaFieldNames = self::buildSchemaFieldList($val, $schemaFieldNames);
            } else {
                if ($key === 'name') {
                    $schemaFieldNames[$val] = $val;
                }
            }
        }
        return $schemaFieldNames;
    }
    
    public function discoverSchema($container, array $record)
    {

        // Discover Schema
//        $json = json_encode($record);
        $container->analysePayload($record);

        $container->determineFieldTypes($container->discoveredFieldOccurrence);
    }

    public function buildAndSaveSchema($container, array $record)
    {
        
        $avroFieldSchema = [];

        $sub_field_count = [];

        foreach ($record as $field => $value) {

            $avroType = $container->discoveredFieldOccurrence[$field]['determined_type'];

//            IngestJob::buildAvroFields($avroFieldSchema, $field, $avroType,
//                $container->discoveredFieldOccurrence);

            IngestJob::buildAvroFields($avroFieldSchema, $field, $avroType, $container->discoveredFieldOccurrence, [], $sub_field_count);
        }


        // now save schema to DB so it's avalibe to schema API
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

    public function encodeWithSchema($container, array $record, $avroFieldSchema)
    {

        $container->tenant_id = $this->user->tenant_id;
        $container->schema = $avroFieldSchema;
        
        $container->defaultMsg = $container->defaultMessage();

        $message = $container->defaultMsg;

        foreach ($record as $field => $value) {
            $container->ingestField($field, $value, $container->discoveredFieldOccurrence, $message);
        }

        $recordsWithSchema = $this->encodeAvro($avroFieldSchema, $message);

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

    public function encodeAvro($avroField, array $record)
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

    public function testMerge()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fields = [
            'first_name' => '',
            'last_name' => 'hudson',
            'name_id' => 100,
        ];

        $this->discoverSchema($container, $fields);

        // Set schema evolution
        $container->discoveredFieldOccurrence['name_id']['evolution']['string']['type'] = 'merge';
        $container->discoveredFieldOccurrence['name_id']['evolution']['string']['new_value'] = 'first_name';
        $container->discoveredFieldOccurrence['name_id']['evolution']['string']['solved'] = true;

        $avroFieldSchema = $this->buildAndSaveSchema($container, $fields);

        $fields = [
            'first_name' => '',
            'last_name' => 'hudson',
            'name_id' => 'dave',
        ];

        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertEquals(null, $record['name_id']);

        $this->assertEquals($fields['name_id'], $record['first_name']);
        $this->assertEquals($fields['last_name'], $record['last_name']);

    }

    public function testMergeRecord()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'status' => '123-abc',
                'class' => 100
            ]
        ];

        $this->discoverSchema($container, $fields);

        // Set schema evolution
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['type'] = 'merge';
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['new_value'] = 'status';
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['solved'] = true;

        $avroFieldSchema = $this->buildAndSaveSchema($container, $fields);

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'status' => null,
                'class' => '123-foo'
            ]
        ];

        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertEquals(null, $record['account']['class']);

        $this->assertEquals($fields['account']['class'], $record['account']['status']);
        $this->assertEquals($fields['last_name'], $record['last_name']);


    }

    public function testCast()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fieldsOrg = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'status' => 123
        ];

        $this->discoverSchema($container, $fieldsOrg);

        // Set schema evolution
        $container->discoveredFieldOccurrence['status']['evolution']['string']['type'] = 'cast';
        $container->discoveredFieldOccurrence['status']['evolution']['string']['new_value'] = 'string';
        $container->discoveredFieldOccurrence['status']['evolution']['string']['solved'] = true;

        $avroFieldSchema = $this->buildAndSaveSchema($container, $fieldsOrg);

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'status' => '123',
        ];

        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        // status
        $this->assertEquals(gettype($fieldsOrg['status']), gettype($record['status']));
        $this->assertEquals(123, $record['status']);
        $this->assertEquals('integer', gettype($record['status']));

    }

    public function testCastMap()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fieldsOrig = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'status' => 'foo',
            ]
        ];

        $this->discoverSchema($container, $fieldsOrig);

        // Set schema evolution
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['type'] = 'cast';
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['new_value'] = 'string';
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['solved'] = true;

        $avroFieldSchema = $this->buildAndSaveSchema($container, $fieldsOrig);

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'status' => 123,
            ]
        ];

        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertEquals(gettype($fieldsOrig['account']['status']), gettype($record['account']['status']));
        $this->assertEquals('123', $record['account']['status']);
        $this->assertEquals('string', gettype($record['account']['status']));


    }


    public function testNew()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fieldsOrig = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'status' => 123,
        ];

        $this->discoverSchema($container, $fieldsOrig);

        // Set schema evolution
        $container->discoveredFieldOccurrence['status']['evolution']['string']['type'] = 'new';
        $container->discoveredFieldOccurrence['status']['evolution']['string']['new_value'] = 'status_str';
        $container->discoveredFieldOccurrence['status']['evolution']['string']['solved'] = true;

        $avroFieldSchema = $this->buildAndSaveSchema($container, $fieldsOrig);

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'status' => 'active',
        ];

        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertNotNull($recordsWithSchema);

        $this->assertEquals('string', gettype($record['status_str']));
        $this->assertEquals($fields['status'], $record['status_str']);
        
        $this->assertArrayHasKey('status', $record);
        $this->assertNull($record['status']);

    }

    public function testNewRecord()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fieldsOrig = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'client' => 'acme ltd',
                'status' => 123,
            ]
        ];

        $this->discoverSchema($container, $fieldsOrig);

        // Set schema evolution
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['type'] = 'new';
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['new_value'] = 'status_str';
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['solved'] = true;

        $avroFieldSchema = $this->buildAndSaveSchema($container, $fieldsOrig);

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'client' => 'acme ltd',
                'status' => 'active',
            ]
        ];

        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);

        $record = $this->decodeAvro($recordsWithSchema);
        
        $this->assertArrayHasKey('status', $record['account']);
        $this->assertNull($record['account']['status']);

        $this->assertEquals('string', gettype($record['account']['status_str']));


    }

//    public function testNewNestedMap()
//    {
//
//        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'account' => [
//                'status' => 'foo',
//            ]
//        ];
//
//        $this->discoverSchema($container, $fields);
//
//        // Set schema evolution
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['type'] = 'new';
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['new_value'] = 'status_int';
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['solved'] = true;
//
//        $avroFieldSchema = $this->buildAndSaveSchema($container, $fields);
//
//        // @note - maps only support one type, so we still use string
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'account' => [
//                'status' => 123,
//            ]
//        ];
//
//        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);
//
//        $record = $this->decodeAvro($recordsWithSchema);
//
//        // Cannot have multiple types in a map.
//        // Therefore we should never support new field types.
//        // Would need to do something safe like cast or drop.
//        $this->expectException('AvroIOTypeException');
//
//
//    }
//
//    public function testRename()
//    {
//
//        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'status' => 'foo',
//        ];
//
//        $this->discoverSchema($container, $fields);
//
//        // Set schema evolution
//        $container->discoveredFieldOccurrence['status']['evolution']['string']['type'] = 'rename';
//        $container->discoveredFieldOccurrence['status']['evolution']['string']['new_value'] = 'status_bar';
//        $container->discoveredFieldOccurrence['status']['evolution']['string']['solved'] = true;
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'status' => 'bar',
//        ];
//
//        $avroFieldSchema = $this->buildAndSaveSchema($container, $fields);
//
//        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);
//
//        $record = $this->decodeAvro($recordsWithSchema);
//
//        // old field name NOT in schema
//        // new field name IS in schema
//        $schemaFieldNames = self::buildSchemaFieldList($avroFieldSchema);
//
//        $this->assertArrayNotHasKey('status', $schemaFieldNames);
//        $this->assertArrayHasKey('status_bar', $schemaFieldNames);
//
//        $this->assertNotNull($recordsWithSchema);
//
//        $this->assertArrayNotHasKey('status', $record);
//
//        $this->assertEquals(gettype($fields['status']), gettype($record['status_bar']));
//        $this->assertEquals('string', gettype($record['status_bar']));
//
//
//    }
//
//    public function testRenameRecord()
//    {
//
//        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'account' => [
//                'status' => 'foo',
//                'id' => 123,
//            ]
//        ];
//
//        $this->discoverSchema($container, $fields);
//
//        // Set schema evolution
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['type'] = 'rename';
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['new_value'] = 'status_bar';
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['solved'] = true;
//
//        // @note - maps only support one type, so we still use string
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'account' => [
//                'status' => 'bar',
//                'id' => 123,
//            ]
//        ];
//
//        $avroFieldSchema = $this->buildAndSaveSchema($container, $fields);
//
//        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);
//
//        $record = $this->decodeAvro($recordsWithSchema);
//
//        // old field name NOT in schema
//        // new field name IS in schema
//        $schemaFieldNames = self::buildSchemaFieldList($avroFieldSchema);
//
//        $this->assertArrayNotHasKey('status', $schemaFieldNames);
//        $this->assertArrayHasKey('status_bar', $schemaFieldNames);
//
//
//        $this->assertNotNull($recordsWithSchema);
//
//        $this->assertArrayNotHasKey('status', $record['account']);
//
//        $this->assertEquals(gettype($fields['account']['status']), gettype($record['account']['status_bar']));
//        $this->assertEquals('string', gettype($record['account']['status_bar']));
//
//
//    }
//
//
//    public function testRenameNestedMap()
//    {
//
//        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'account' => [
//                'status' => 'foo',
//            ]
//        ];
//
//        $this->discoverSchema($container, $fields);
//
//        // Set schema evolution
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['type'] = 'rename';
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['new_value'] = 'status_bar';
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['solved'] = true;
//
//        // @note - maps only support one type, so we still use string
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'account' => [
//                'status' => 'bar',
//            ]
//        ];
//
//        $avroFieldSchema = $this->buildAndSaveSchema($container, $fields);
//
//        $recordsWithSchema = $this->encodeWithSchema($container, $fields, $avroFieldSchema);
//
//        $record = $this->decodeAvro($recordsWithSchema);
//
//        // old field name NOT in schema
//        // new field name IS in schema
//        $schemaFieldNames = self::buildSchemaFieldList($avroFieldSchema);
//
//        $this->assertArrayNotHasKey('status', $schemaFieldNames);
////        $this->assertArrayHasKey('status_bar', $schemaFieldNames['account']);
//
//
//        $this->assertNotNull($recordsWithSchema);
//
//        $this->assertArrayNotHasKey('status', $record['account']);
//
//        $this->assertEquals(gettype($fields['account']['status']), gettype($record['account']['status_bar']));
//        $this->assertEquals('string', gettype($record['account']['status_bar']));
//
//
//    }

}

