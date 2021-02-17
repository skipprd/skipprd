<?php

namespace Feature\Models\Schema;

use App\IngestJob;
use App\Schema;
use App\User;
use Illuminate\Foundation\Testing\Concerns\InteractsWithConsole;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Skipprd\Commands\PollSqs;
use Tests\TestCase;

class SchemaModelTest extends TestCase
{
    use InteractsWithConsole;
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

    }

    public function tearDown()
    {
        parent::tearDown();
    }

    public function testBlah() {

//        $this->schema = factory(Schema::class)->create();

        $id = random_int(100, 1000);

        $this->schema = factory(\App\Schema::class)->states('new_schema')->create([
            'ingest_job_id' => $id,
        ]);

        $this->assertDatabaseHas('schemas', [
            'ingest_job_id' => $id,
        ]);
    }

    public function testSomething()
    {

        $ingestId = random_int(100, 1000);

        factory(\App\IngestJob::class)->create([
            'id' => $ingestId,
            'name' => randomPassword(),
            'tenant_id' => 'net',
//            'source_job_id' => 1,
//            'output_job_id' => 1,
            'analysing' => 1,
            'enabled' => 1,
        ]);

        factory(\App\Schema::class)->states('new_schema')->create([
            'id' => random_int(100, 1000),
            'ingest_job_id' => $ingestId]
        );

        $ingestJob = \App\IngestJob::where(['id' => $ingestId])->with('schemas')
            ->first();

        $schema = \App\Schema::where(['ingest_job_id' => $ingestId])
            ->first();

        $this->assertDatabaseHas('ingest_jobs', [
            'analysing' => 1,
        ]);

        $this->assertDatabaseHas('ingest_jobs', [
            'id' => $ingestId,
        ]);

        $ingestJob->buildJobConfig();

        $schemaField = $ingestJob->schema->schema[0];
        $this->assertEquals('skpr_event_ts', $schemaField['name']);
    }
    
}
