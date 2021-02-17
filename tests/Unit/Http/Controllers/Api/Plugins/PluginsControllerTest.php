<?php

namespace Unit\Http\Controllers\Api\Plugins;


use App\IngestJob;
use App\Permission;
use App\Role;
use App\Schema;
use App\User;
use Illuminate\Foundation\Testing\Concerns\InteractsWithConsole;
use Illuminate\Support\Facades\DB;
use Tests\TestCase;
use Illuminate\Foundation\Testing\WithoutMiddleware;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;

class PluginsControllerTest extends TestCase
{

//    use DatabaseMigrations;
//    use InteractsWithConsole;

    public function setUp() {

        parent::setUp();

        $this->userNoAuth = factory(User::class)->create([
            'name' => 'nouath',
            'email' => 'noauth@abc.com',
            'password' => 'boo',
            'api_token' => 'boo',
            'tenant_id' => 'abc',
        ]);

        $this->userAuth = factory(User::class)->create([
            'name' => 'Auth',
            'email' => 'Auth@abc.com',
            'password' => 'foo',
            'api_token' => 'foo',
            'tenant_id' => 'abc',
        ]);

        $role = Role::where(['name' => 'admin'])->first();

        DB::table('model_has_roles')
            ->insert([
                'role_id' => $role->id,
                'model_id' => $this->userAuth->id,
                'model_type' => 'App\User',
            ]);

        $this->ingest_job_id = random_int(100, 1000);
        
        $this->schema = factory(Schema::class)->states('new_schema')->create([
            'ingest_job_id' => $this->ingest_job_id,
        ]);

        $this->subjectName = 'abc_xyz-value';
        $this->schemaName = 'abc_xyz';
    }

    public function tearDown()
    {
        parent::tearDown();
    }

    /**
     * subjects()
     *
     */

    public function testIndex()
    {

        $response = $this->actingAs($this->userAuth)
            ->get('api/plugins?_query=&_skip=0&_take=25&_plugin_type=data_output');

        $response->assertStatus(200);
//        $response->assertRedirect('login');
    }

}
