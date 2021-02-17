<?php

namespace Unit\App\Models\IngestJob;

use App\IngestJob;
use App\Schema;
use App\User;
use Illuminate\Foundation\Testing\Concerns\InteractsWithConsole;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Support\Facades\Auth;
use Skipprd\Commands\PollSqs;
use Mockery\Mock;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;

class IngestJobMethodsTest extends TestCase
{
//    use DatabaseMigrations;
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

        parent::tearDown();

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

    public function testGetCleanNameMethod() {

        $model = new IngestJob();
        $model->name = 'Messy name 123 test-ing';
        
        $cleanName = $model->getCleanName();
        
        $this->assertEquals('messyname123testing', $cleanName);
    }

    public function testisAnalysingOneCreate()
    {

        $ingestJob = \Mockery::mock(IngestJob::class)->makePartial();

        $this->assertEquals(true, $ingestJob->isAnalysing());
    }

    public function testNotEnabledOneCreate() {

        $ingestJob = \Mockery::mock(IngestJob::class)->makePartial();

        $this->assertEquals(false, $ingestJob->enabled);

    }


//    public function testisAnalysingEmptySchema() {
//
//        Auth::setUser($this->user);
//
//        $ingestJob = factory(IngestJob::class)
//            ->create()
//            ->each(function ($ingestJob) {
//                $ingestJob->schemas()->save(factory(Schema::class)->states('empty_schema')->make());
//            });
//
//        $isAnalysing = $ingestJob->isAnalysing();
//
//        $this->assertEquals(true, $isAnalysing);
//    }
//
//    public function testisAnalysingHasSchema() {
//
//        Auth::setUser($this->user);
//
//        $ingestJob = factory(IngestJob::class)
//            ->create()
//            ->each(function ($ingestJob) {
//                $ingestJob->schemas()->save(factory(Schema::class)->states('has_schema')->make());
//            });
//
//        $isAnalysing = $ingestJob->isAnalysing();
//
//        $this->assertEquals(false, $isAnalysing);
//    }

//    public function testSomething()
//    {
//
//
//        $this->ingestJob = factory(IngestJob::class)->create();
//
//        $this->assertDatabaseHas('ingest_jobs', [
//            'analysing' => 1,
//        ]);
//
//        $this->assertDatabaseHas('ingest_jobs', [
//            'id' => 1,
//        ]);
//
//        // try and catch exit here to continue test
//        $this->artisan('Iq:ingest-schema')
//            ->assertExitCode(0);
//
//        $this->assertDatabaseHas('ingest_jobs', [
//            'analysing' => 0,
//        ]);
//
//        $ingestJob = IngestJob::where(['id' => 1])->first();
//
//        $this->assertArrayHasKey('uuid', $ingestJob->field_yml);
//    }
}
