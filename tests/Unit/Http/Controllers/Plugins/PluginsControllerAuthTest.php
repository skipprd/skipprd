<?php

namespace Unit\Http\Controllers\Plugins;


use App\Permission;
use App\Plugin;
use App\User;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Illuminate\Support\Facades\DB;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;

class PluginsControllerAuthTest extends TestCase
{

//    public $user;
//
//    public $plugin;

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

    public function testGetPluginsCustomFields()
    {

        $yml = <<<EOF
name: S3
description: 'S3 Data Source'
type: datasource
enabled: true
tier: basic
config_fields:
    scheduled_job: '*/15 * * * *'
    cpu: '2048'
    mem: '4096'
connection_fields:
    bucket_name: { type: input, required: true }
    bucket_region: { type: input, required: true }
    aws_access_id: { type: input }
    aws_secret_key: { type: secret }
custom_fields:
    bucket_prefix: { type: input }
    
EOF;

        $this->plugin = factory(Plugin::class)->create([
            'name' => 'S3',
            'description' => 'S3',
            'config_yml' => Yaml::parse($yml),
            'type' => 'datasource',
            'tier' => 'basic',
            'enabled' => true,
        ]);

        $this->setUserPerm('plugin:Configure');

        $json = Yaml::parse($yml);

        $this->actingAs($this->user)
            ->get('/plugins/plugin-custom-fields/' . $this->plugin->id)
            ->assertStatus(200)
            ->assertJson($json['custom_fields']);

        $this->actingAs($this->user)
            ->get('/plugins/plugin-connection-fields/' . $this->plugin->id)
            ->assertStatus(200)
            ->assertJson($json['connection_fields']);
    }
}