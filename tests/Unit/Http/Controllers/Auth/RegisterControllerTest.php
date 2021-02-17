<?php

namespace Unit\Http\Controllers\Auth;


use App\Client;
use App\User;
use Illuminate\Foundation\Testing\WithoutMiddleware;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use App\Schema;
use Illuminate\Support\Facades\DB;
use Tests\TestCase;


class RegisterControllerTest extends TestCase
{

    public function setUp() {

        parent::setUp();

    }

    public function tearDown()
    {
        parent::tearDown();
    }


    public function testPreventRegisterWithExistingTenant()
    {
        $tenant = Client::create([
            'name' => 'ABC Test',
            'tenant_id' => 'abctest',
        ]);

        $this->userAuth = factory(User::class)->create([
            'name' => 'auth',
            'email' => 'auth@abc.com',
            'password' => 'foo',
            'api_token' => 'foo',
            'tenant_id' => 'abctest',
        ]);

        $user = [
            'name' => 'ABC Test',
            'company_name' => 'ABC TEST',
            'email' => 'testemail@test.com',
            'password' => 'passwordtest',
            'password_confirmation' => 'passwordtest',
        ];

        $response = $this->post('/register', $user);

        $response
            ->assertStatus(302)
            ->assertDontSeeText('Select and configure a data source you\'d like to sync with any destination');
//            ->assertSessionHas('status', 'Zodra uw account is goedgekeurd ontvangt u een email');

        //Remove password and password_confirmation from array
        $expectingUser = [
            'name' => 'auth',
            'email' => 'auth@abc.com',
            'api_token' => 'foo',
            'tenant_id' => 'abctest',
        ];

        $this->assertDatabaseHas('users', $expectingUser);

        $notExpectingUser = [
            'name' => 'ABC Test',
            'tenant_id' => 'abctest',
            'email' => 'testemail@test.com',
            'api_token' => null,
        ];

        $this->assertDatabaseMissing('users', $notExpectingUser);
    }

    public function testRegisterValid()
    {
        $user = [
            'name' => 'ABC Test',
            'company_name' => 'Acme Foo Ltd',
            'email' => 'testemail@test.com',
            'password' => 'passwordtest',
            'password_confirmation' => 'passwordtest',
        ];

        $response = $this->post('/register', $user);

        $response
            ->assertRedirect('/ingest-job/plugins/list');
//            ->assertSeeText('Select and configure a data source you\'d like to sync with any destination');
//            ->assertSessionHas('status', 'Zodra uw account is goedgekeurd ontvangt u een email');

        //Remove password and password_confirmation from array
        $expectingUser = [
            'name' => 'ABC Test',
            'tenant_id' => 'acmefooltd',
            'email' => 'testemail@test.com',
            'api_token' => null,
        ];

        $this->assertDatabaseHas('users', $expectingUser);
    }




}
