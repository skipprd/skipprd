<?php

namespace Feature\Models\IngestJob;


use App\IngestJob;
use App\User;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Tests\TestCase;
use App\Exceptions\TenantConstraintException;

class MultiTenantTest extends TestCase
{
    public function setUp()
    {
        parent::setUp();

        $this->userNot = factory(User::class)->create([
            'name' => 'abcuser',
            'email' => 'user@abc.com',
            'password' => randomPassword(12),
            'api_token' => 'booooo',
            'tenant_id' => 'abc',
        ]);

        \DB::table('model_has_roles')
            ->insert([
                'role_id' => 1,
                'model_id' => $this->userNot->id,
                'model_type' => 'App\User',
            ]);

        $this->userIs = factory(User::class)->create([
            'name' => 'xyzuser',
            'email' => 'user@xyz.com',
            'password' => randomPassword(12),
            'api_token' => 'baaaaa',
            'tenant_id' => 'xyz',
        ]);

        \DB::table('model_has_roles')
            ->insert([
                'role_id' => 1,
                'model_id' => $this->userIs->id,
                'model_type' => 'App\User',
            ]);

        $tenant_id = 'xyz';
        \Auth::setUser($this->userIs);

        $model = factory(IngestJob::class)->create([
            'name' => "my-model-tenant-$tenant_id",
            'enabled' => 1,
        ]);
    }

    public function testNotCorrectTenantUpdate()
    {

        \Auth::setUser($this->userIs);

        $ingestJob = IngestJob::get()->load(['dataSourceJob', 'schema'])->first();

        \Auth::setUser($this->userNot);

        $ingestJob->enabled = 0;

        $this->expectException(TenantConstraintException::class);

        $ingestJob->save();
        // no code executes after caught exception

    }

    public function testNotCorrectTenantDelete()
    {

        \Auth::setUser($this->userIs);

        $ingestJob = IngestJob::get()->load(['dataSourceJob', 'schema'])->first();

        \Auth::setUser($this->userNot);

        $this->expectException(TenantConstraintException::class);

        $ingestJob->delete();
        // no code executes after caught exception

    }

    public function testNotCorrectTenantLoad()
    {

        \Auth::setUser($this->userNot);

        $ingestJobs = IngestJob::get()->load(['dataSourceJob', 'schema'])->all();

        $this->assertEmpty($ingestJobs);

    }

    public function testCorrectTenantLoad()
    {

        \Auth::setUser($this->userIs);

        $ingestJobs = IngestJob::get()->load(['dataSourceJob', 'schema'])->all();

        $this->assertNotEmpty($ingestJobs);
        $this->assertEquals("my-model-tenant-xyz", $ingestJobs[0]->name);

    }
}