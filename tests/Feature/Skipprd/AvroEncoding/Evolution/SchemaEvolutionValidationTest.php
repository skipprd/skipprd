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
use Skipprd\Events\ValidateSchemaRequested;
use Skipprd\Listeners\ValidateSchemaFile;
use Skipprd\Listeners\ValidateSchemaKafka;
use Skipprd\Services\AvroSubPub\CachedSchemaRegistryClient;
use Skipprd\Services\AvroSubPub\MessageSerializer;
use Skipprd\Traits\AnalyseSchema;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;
use Mockery;
use AvroSchema;
use function Aws\map;

class SchemaEvolutionValidationTest extends TestCase
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

        $container::determineFieldTypes($container->discoveredFieldOccurrence);
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

    public function encodeWithSchema($container, array $record, $avroFieldSchema)
    {

        $json = json_encode($record);

        $container->tenant_id = $this->user->tenant_id;
        $container->schema = $avroFieldSchema;

        $container->ingest($json);

//        foreach ($record as $field => $value) {
//            $container->ingestField($field, $value, $container->discoveredFieldOccurrence, $record);
//        }

        $message = $container->entries[0];

        $recordsWithSchema = $this->encodeAvroFields($avroFieldSchema, $message);

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

    public function encodeAvroFields($avroField, array $record)
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
            'skpr_event_ts' => 0,
            'skpr_partition' => '',
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'name_id' => 123,
        ];

        $this->discoverSchema($container, $fields);

        // Set schema evolution
        $container->discoveredFieldOccurrence['name_id']['evolution']['string']['type'] = 'merge';
        $container->discoveredFieldOccurrence['name_id']['evolution']['string']['new_value'] = 'first_name';
        $container->discoveredFieldOccurrence['name_id']['evolution']['string']['solved'] = true;

        $fields['name_id'] = 'dave';

        $validator = new ValidateSchemaFile();

        $validator->ingestRecord($fields, $container->discoveredFieldOccurrence);
        $isValid = $validator->isValid();

//        $this->assertEquals(true, $isValid);

//        $this->assertNull($validator->msg['name_id']);
        $this->assertEquals('dave', $validator->msg['first_name']);
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
                'status' => 'active',
                'class' => 123
            ]
        ];

        $this->discoverSchema($container, $fields);

        // Set schema evolution
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['type'] = 'merge';
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['new_value'] = 'status';
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['solved'] = true;

        $fields['account']['class'] = 'paused';

        $validator = new ValidateSchemaFile();

        $validator->ingestRecord($fields, $container->discoveredFieldOccurrence);
        $isValid = $validator->isValid();

//        $this->assertEquals(true, $isValid);

//        $this->assertNull($validator->msg['account']['class']);
        $this->assertEquals('paused', $validator->msg['account']['status']);

    }

    public function testMergeMap()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'status' => 999,
                'class' => 123
            ]
        ];

        $this->discoverSchema($container, $fields);

        // Set schema evolution
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['type'] = 'merge';
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['new_value'] = 'status';
        $container->discoveredFieldOccurrence['account']['fields']['class']['evolution']['string']['solved'] = true;

        $fields['account']['class'] = 'paused';

        $validator = new ValidateSchemaFile();

        $validator->ingestRecord($fields, $container->discoveredFieldOccurrence);
        $isValid = $validator->isValid();

        // should fail
        // no possible to put a string in a map of ints
        $this->assertEquals(false, $isValid);

//        $this->assertNull($validator->msg['account']['class']);
//        $this->assertEquals('paused', $validator->msg['account']['status']);

    }

//    public function testCast()
//    {
//
//        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'id' => 10009,
//            'status' => 'foo',
//        ];
//
//        $this->discoverSchema($container, $fields);
//
//        // Set schema evolution
//        $container->discoveredFieldOccurrence['status']['evolution']['double']['type'] = 'cast';
//        $container->discoveredFieldOccurrence['status']['evolution']['double']['new_value'] = 'string';
//        $container->discoveredFieldOccurrence['status']['evolution']['double']['solved'] = true;
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'id' => 10009,
//            'status' => 1.123,
//        ];
//
//        $configRequest = new ValidateSchemaRequested($ingestJobContainer,  $container->discoveredFieldOccurrence);
//
//        $validator = new ValidateSchema();
//        $isValid = $validator->handle($configRequest);
//
//
//        $this->assertEquals(true, $isValid);
//
//    }
//
//    public function testCastInvalid()
//    {
//
//        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'id' => 10009,
//            'status' => 123,
//        ];
//
//        $this->discoverSchema($container, $fields);
//
//        // Set schema evolution
//        $container->discoveredFieldOccurrence['status']['evolution']['string']['type'] = 'cast';
//        $container->discoveredFieldOccurrence['status']['evolution']['string']['new_value'] = 'integer';
//        $container->discoveredFieldOccurrence['status']['evolution']['string']['solved'] = true;
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'id' => 10009,
//            'status' => 'foo',
//        ];
//
//        $configRequest = new ValidateSchemaRequested($ingestJobContainer,  $container->discoveredFieldOccurrence);
//
//        $validator = new ValidateSchema();
//        $isValid = $validator->handle($configRequest);
//
//
//        $this->assertEquals(false, $isValid);
//
//    }
//
//    public function testCastMap()
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
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['type'] = 'cast';
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['new_value'] = 'string';
//        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['integer']['solved'] = true;
//
//        $fields = [
//            'first_name' => 'paul',
//            'last_name' => 'hudson',
//            'account' => [
//                'status' => 123,
//            ]
//        ];
//
//        $configRequest = new ValidateSchemaRequested($ingestJobContainer,  $container->discoveredFieldOccurrence);
//
//        $validator = new ValidateSchema();
//        $isValid = $validator->handle($configRequest);
//
//
//        $this->assertEquals(true, $isValid);
//
//
//    }


    public function testNew()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'status' => 123,
        ];

        $this->discoverSchema($container, $fields);

        // Set schema evolution
        $container->discoveredFieldOccurrence['status']['evolution']['string']['type'] = 'new';
        $container->discoveredFieldOccurrence['status']['evolution']['string']['new_value'] = 'status_str';
        $container->discoveredFieldOccurrence['status']['evolution']['string']['solved'] = true;

        $fields['status'] = 'dave';

        $validator = new ValidateSchemaFile();

        $validator->ingestRecord($fields, $container->discoveredFieldOccurrence);
        $isValid = $validator->isValid();


//        $this->assertEquals(true, $isValid);

//        $this->assertNull($validator->msg['status']);
        $this->assertEquals('dave', $validator->msg['status_str']);

    }

    public function testNewRecord()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'status' => 'active',
                'id' => 123,
            ]
        ];

        $this->discoverSchema($container, $fields);

        // Set schema evolution
        $container->discoveredFieldOccurrence['account']['fields']['id']['evolution']['string']['type'] = 'new';
        $container->discoveredFieldOccurrence['account']['fields']['id']['evolution']['string']['new_value'] = 'id_str';
        $container->discoveredFieldOccurrence['account']['fields']['id']['evolution']['string']['solved'] = true;

        $fields['account']['id'] = 'dave';

        $validator = new ValidateSchemaFile();

        $validator->ingestRecord($fields, $container->discoveredFieldOccurrence);
        $isValid = $validator->isValid();

//        $this->assertEquals(true, $isValid);

//        $this->assertNull($validator->msg['account']['id']);
        $this->assertEquals('dave', $validator->msg['account']['id_str']);

    }

    public function testNewNestedMap()
    {

        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = \Mockery::mock(IngestJob::class)->makePartial();

        $fields = [
            'first_name' => 'paul',
            'last_name' => 'hudson',
            'account' => [
                'status' => 123,
                'client' => 999,
            ]
        ];

        $this->discoverSchema($container, $fields);

        // Set schema evolution
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['type'] = 'new';
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['new_value'] = 'status_str';
        $container->discoveredFieldOccurrence['account']['fields']['status']['evolution']['string']['solved'] = true;

        $fields['account']['status'] = 'dave';

        $validator = new ValidateSchemaFile();

        $validator->ingestRecord($fields, $container->discoveredFieldOccurrence);
        $isValid = $validator->isValid();

        // should fail
        // no possible to put a string in a map of ints
        $this->assertEquals(false, $isValid);

    }

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
//        $configRequest = new ValidateSchemaRequested($ingestJobContainer,  $container->discoveredFieldOccurrence);
//
//        $validator = new ValidateSchema();
//        $isValid = $validator->handle($configRequest);
//
//
//        $this->assertEquals(true, $isValid);
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
//        $configRequest = new ValidateSchemaRequested($ingestJobContainer,  $container->discoveredFieldOccurrence);
//
//        $validator = new ValidateSchema();
//        $isValid = $validator->handle($configRequest);
//
//
//        $this->assertEquals(true, $isValid);
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
//        $configRequest = new ValidateSchemaRequested($ingestJobContainer,  $container->discoveredFieldOccurrence);
//
//        $validator = new ValidateSchema();
//        $isValid = $validator->handle($configRequest);
//
//
//        $this->assertEquals(true, $isValid);
//
//
//    }

}

