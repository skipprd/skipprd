<?php

namespace Unit\Http\Controllers\DataSources;


use App\Permission;
use App\User;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Illuminate\Support\Facades\DB;
use Tests\TestCase;

class DataSourcePluginControllerAuthTest extends TestCase
{
    
    public function setUp()
    {
        parent::setUp();

//        $this->seed();

        $this->user = factory(User::class)->create([
            'name' => '123user',
            'email' => 'user@123.com',
            'password' => randomPassword(12),
            'api_token' => 'baaaaa',
            'tenant_id' => 'xyz',
        ]);

    }

    public function setUserPerm($permName) {

        $perm = Permission::where(['name' => $permName])->first();

        DB::table('model_has_permissions')
            ->insert([
                'permission_id' => $perm->id,
                'model_id' => $this->user->id,
                'model_type' => 'App\User',
            ]);

    }

    public function testAuthList()
    {

        $this->setUserPerm('route:dataSourcePlugins');

        $this->actingAs($this->user)
            ->get('/ingest-job/plugins/list')
            ->assertStatus(200)
            ->assertViewHas('show_filters', false);
    }

    public function testNoAuthList()
    {

        $this->actingAs($this->user)
            ->get('/ingest-job/plugins/list')
            ->assertStatus(403);
    }

}