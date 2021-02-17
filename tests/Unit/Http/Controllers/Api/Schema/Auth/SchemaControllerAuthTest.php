<?php

namespace Unit\Http\Controllers\Schema\Auth;


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
use Illuminate\Support\Str;

class SchemaControllerAuthTest extends TestCase
{

//    use DatabaseMigrations;
//    use InteractsWithConsole;

    public $userNoAuth;

    public $userAuth;

    public function setUp() {

        parent::setUp();

        $this->noAuthToken = Str::random(60);

        $this->userNoAuth = factory(User::class)->create([
            'name' => 'nouath',
            'email' => 'noauth@abc.com',
            'password' => 'boo',
            'api_token' => $this->noAuthToken,
            'tenant_id' => 'abc',
        ]);

        $this->authToken = Str::random(60);

        $this->userAuth = factory(User::class)->create([
            'name' => 'auth',
            'email' => 'auth@abc.com',
            'password' => 'foo',
            'api_token' => $this->authToken,
            'tenant_id' => 'abc',
        ]);

        $role = Role::where(['name' => 'pipeline_system'])->first();

        DB::table('model_has_roles')
            ->insert([
                'role_id' => $role->id,
                'model_id' => $this->userAuth->id,
                'model_type' => 'App\User',
            ]);

        $id = random_int(100, 1000);

        $this->schema = factory(Schema::class)->states('new_schema')->create([
            'ingest_job_id' => $id,
        ]);

        $this->schemaName = 'abc_xyz-value';

    }

    public function tearDown()
    {
        parent::tearDown();
    }

    /**
     * subjects()
     *
     */

    public function testSubjectsNotAuthedUserAndNoToken()
    {

        $response = $this->actingAs($this->userNoAuth)
            ->get('subjects');

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testSubjectsAuthedUserButNoToken()
    {

        $response = $this->actingAs($this->userAuth)
            ->get('subjects');

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testSubjectsNoAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userNoAuth->api_token];

        $response = $this->get('subjects', $headers);

        $response->assertStatus(403);

    }

    public function testSubjectsAuthToken()
    {

        echo $this->userAuth->api_token;
        echo "\n";
        echo $this->authToken;

        $headers = ['Authorization' => "Bearer " . $this->authToken];

        $response = $this->get('subjects', $headers);

        $response->assertStatus(200);

    }

    public function testSubjectsAuthBasic()
    {

        $headers = ['Authorization' => "Basic " . base64_encode($this->userAuth->name. ':' . $this->userAuth->api_token)];

        $response = $this->get('subjects', $headers);

        $response->assertStatus(200);

    }

    /**
     * subjectsRegisteredCheck
     */

    public function testsubjectsRegisteredCheckNotAuthedUserAndNoToken()
    {

        $response = $this->actingAs($this->userNoAuth)
            ->post('/subjects/' . $this->schemaName);

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testsubjectsRegisteredCheckAuthedUserButNoToken()
    {

        $response = $this->actingAs($this->userAuth)
            ->post('/subjects/' . $this->schemaName);

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testsubjectsRegisteredCheckNoAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userNoAuth->api_token];

        $response = $this->post('/subjects/' . $this->schemaName, [], $headers);

        $response->assertStatus(403);

    }

    public function testsubjectsRegisteredCheckAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->post('/subjects/' . $this->schemaName, [], $headers);

        $response->assertStatus(200);

    }

    public function testsubjectsRegisteredCheckAuthBasic()
    {

        $headers = ['Authorization' => "Basic " . base64_encode($this->userAuth->name. ':' . $this->userAuth->api_token)];

        $response = $this->post('/subjects/' . $this->schemaName, [], $headers);

        $response->assertStatus(200);

    }

    /**
     * subjectsGetVersion
     */

    public function testsubjectsGetVersionsNotAuthedUserAndNoToken()
    {

        $response = $this->actingAs($this->userNoAuth)
            ->get('/subjects/' . $this->schemaName . '/versions/1');

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testsubjectsGetVersionAuthedUserButNoToken()
    {

        $response = $this->actingAs($this->userAuth)
            ->get('/subjects/' . $this->schemaName . '/versions/1');

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testsubjectsGetVersionNoAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userNoAuth->api_token];

        $response = $this->get('/subjects/' . $this->schemaName . '/versions/1', $headers);

        $response->assertStatus(403);

    }

    public function testsubjectsGetVersionAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->get('/subjects/' . $this->schemaName . '/versions/1', $headers);

        $response->assertStatus(200);

    }

    public function testsubjectsGetVersionAuthBasic()
    {

        $headers = ['Authorization' => "Basic " . base64_encode($this->userAuth->name. ':' . $this->userAuth->api_token)];

        $response = $this->get('/subjects/' . $this->schemaName . '/versions/1', $headers);

        $response->assertStatus(200);

    }

    /**
     * subjectsGetVersion (latest)
     */

    public function testsubjectsGetVersionLatestNotAuthedUserAndNoToken()
    {

        $response = $this->actingAs($this->userNoAuth)
            ->get('/subjects/' . $this->schemaName . '/versions/latest');

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testsubjectsGetVersionLatestAuthedUserButNoToken()
    {

        $response = $this->actingAs($this->userAuth)
            ->get('/subjects/' . $this->schemaName . '/versions/latest');

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testsubjectsGetVersionLatestNoAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userNoAuth->api_token];

        $response = $this->get('/subjects/' . $this->schemaName . '/versions/latest', $headers);

        $response->assertStatus(403);

    }

    public function testsubjectsGetVersionLatestAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->get('/subjects/' . $this->schemaName . '/versions/latest', $headers);

        $response->assertStatus(200);

    }

    public function testsubjectsGetVersionLatestAuthBasic()
    {

        $headers = ['Authorization' => "Basic " . base64_encode($this->userAuth->name. ':' . $this->userAuth->api_token)];

        $response = $this->get('/subjects/' . $this->schemaName . '/versions/latest', $headers);

        $response->assertStatus(200);

    }

    /**
     * schemasGet
     */

    public function testschemasGetNotAuthedUserAndNoToken()
    {

        $response = $this->actingAs($this->userNoAuth)
            ->get('/schemas/ids/1');

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testschemasGetAuthedUserButNoToken()
    {

        $response = $this->actingAs($this->userAuth)
            ->get('/schemas/ids/1');

        $response->assertStatus(302);
        $response->assertRedirect('register');

    }

    public function testschemasGetNoAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userNoAuth->api_token];

        $response = $this->get('/schemas/ids/1', $headers);

        $response->assertStatus(403);

    }

    public function testschemasGetAuthToken()
    {

        $headers = ['Authorization' => "Bearer " . $this->userAuth->api_token];

        $response = $this->get('/schemas/ids/1', $headers);

        $response->assertStatus(200);

    }

    public function testschemasGetAuthBasic()
    {

        $headers = ['Authorization' => "Basic " . base64_encode($this->userAuth->name. ':' . $this->userAuth->api_token)];

        $response = $this->get('/schemas/ids/1', $headers);

        $response->assertStatus(200);

    }

}
