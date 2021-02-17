<?php

namespace Unit\Http\Controllers\Schema\Correctness;


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

/**
 * Test standard "subject name" strategy which is "topic_name-value".
 */


class SchemaControllerSubjectNameTest extends TestCase
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

        $role = Role::where(['name' => 'pipeline_system'])->first();

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

    public function testSubjects()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->get('subjects', $headers);

        $response->assertStatus(200);
        $response->assertJsonFragment(['subject' => $this->subjectName]);
    }

    /**
     * subjectsRegisteredCheck
     */

    public function testsubjectsName()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->post('/subjects/' . $this->subjectName, [], $headers);

        $response->assertStatus(200);
        $response->assertJsonFragment(['subject' => $this->subjectName]);
    }

    /**
     * subjectsGetVersion
     */

    public function testsubjectsGetVersion()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->get('/subjects/' . $this->subjectName . '/versions/1', $headers);

        $response->assertStatus(200);
        $response->assertJsonFragment(['subject' => $this->subjectName]);
    }

    /**
     * subjectsGetVersion (latest)
     */

    public function testsubjectsGetVersionLatest()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->get('/subjects/' . $this->subjectName .'/versions/latest', $headers);

        $response->assertStatus(200);
        $response->assertJsonFragment(['subject' => $this->subjectName]);
    }


    /**
     * schemasGet
     */

    public function testschemasGetId()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->get('/schemas/ids/1', $headers);

        $response->assertStatus(200);
        $response->assertSeeText($this->schemaName);
    }

}
