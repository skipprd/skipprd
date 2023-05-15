<?php

namespace Feature\Skipprd\AvroEncoding\ComplexTypes;

use App\IngestJob;
use App\Schema;
use App\User;
use AvroSchema;
use Illuminate\Contracts\Container\Container;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Illuminate\Support\Facades\Auth;
use Illuminate\Support\Facades\DB;
use legacy\src\Skipprd\Commands\PipelineCommand;
use legacy\src\Skipprd\Services\AvroSubPub\CachedSchemaRegistryClient;
use legacy\src\Skipprd\Services\AvroSubPub\MessageSerializer;
use legacy\src\Skipprd\Traits\Config;
use Mockery;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;

class AvroComplexTypesEncodingTestWithNulls extends TestCase
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

//    public function testGetLogicalTypeMapOfStringAndNulls()
//    {
//
//        $container = Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();
//
//        $field = 'foo';
//        $value = [
//            'blue_widget' => 'foo',
//            'green_widget' => 'baz',
//            'yellow_widget' => 'bar'
//        ];
//
//        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);
//
//        $value = [
//            'blue_widget' => 'foo',
//            'green_widget' => null,
//            'yellow_widget' => 'bar'
//        ];
//
//        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);
//
//        $this->assertNotNull($recordsWithSchema);
//
//        $record = $this->decodeAvro($recordsWithSchema);
//
//        $this->assertArrayHasKey('foo', $record);
//        $this->assertEquals($value['blue_widget'], $record['foo']['blue_widget']);
//        $this->assertEquals('', $record['foo']['green_widget']); // null so defaults to empty field
//        $this->assertEquals($value['yellow_widget'], $record['foo']['yellow_widget']);
//    }

    public function testGetLogicalTypeRecordAndNulls()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $ingestJobContainer = Mockery::mock(IngestJob::class)->makePartial();

        $field = 'foo';
        $value = [
            'blue_widget' => 'foo',
            'green_widget' => 12345,
            'yellow_widget' => [2, 2, 3]
        ];

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $value = [
            'blue_widget' => 'foo',
            'green_widget' => null,
            'yellow_widget' => [2, 2, 3]
        ];

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value['blue_widget'], $record['foo']['blue_widget']);
        $this->assertEquals(0, $record['foo']['green_widget']); // null so defaults to empty field
        $this->assertEquals($value['yellow_widget'], $record['foo']['yellow_widget']);
    }

}
