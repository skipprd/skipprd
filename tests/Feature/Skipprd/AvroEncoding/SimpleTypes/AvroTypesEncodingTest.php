<?php

namespace Feature\Skipprd\AvroEncoding\SimpleTypes;

use App\IngestJob;
use App\Schema;
use App\User;
use Aws\MockHandler;
use GuzzleHttp\Client;
use GuzzleHttp\HandlerStack;
use Illuminate\Contracts\Container\Container;
use Illuminate\Support\Facades\Auth;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\Response;
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

class AvroTypesEncodingTest extends TestCase
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

    public function testGetLogicalTypeNull()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = null;
        $field = 'foo';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }


    public function testGetLogicalTypeBoolean()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = true;
        $field = 'foo';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

    public function testGetLogicalTypeString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '1567174185-journey_id-1307 FoO.can.speed';
        $field = 'foo';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

    public function testGetLogicalTypeTimestamp()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1567174185;
        $field = 'foo';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

    public function testGetLogicalTypeDate()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "2019-08-30T14:09:51.807Z";
//        $value = "2019-08-30";
        $field = 'foo';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals(1567174191, $record['foo']);

    }

    public function testGetLogicalTypeInt()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = 1307;

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);
    }

    public function testGetLogicalTypeStringInt()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = '271572434';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);
    }

    public function testGetLogicalTypeStringIntLeadingZero()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = '0271572434';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);
    }

    public function testGetLogicalTypeBigInt()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = 353386065604908;

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

    public function testGetLogicalTypeBigIntString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = '353386065604908';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

    public function testGetLogicalTypeBigIntStringLeadingZero()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = '0353386065604908';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

    public function testGetLogicalTypeDouble()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = 5.215797;

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

    public function testGetLogicalTypeStringDouble()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = '5.215797';

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

    public function testGetLogicalTypeLongDouble()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $field = 'foo';
        $value = 3.1415926535897932384626433832795;

        $avroFieldSchema = $this->discoverAndSaveSchema($container, $field, $value);

        $recordsWithSchema = $this->encodeWithSchema($container, $value, $field, $avroFieldSchema);

        $this->assertNotNull($recordsWithSchema);

        $record = $this->decodeAvro($recordsWithSchema);

        $this->assertArrayHasKey('foo', $record);
        $this->assertEquals($value, $record['foo']);

    }

}
