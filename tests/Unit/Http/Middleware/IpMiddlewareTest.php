<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 26/02/2020
 * Time: 17:22
 */

namespace Unit\Http\Middleware;

use App\Http\Middleware\IpMiddleware;
use App\User;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Illuminate\Support\Facades\DB;
use Tests\TestCase;
use App\Exceptions\TenantConstraintException;


class IpMiddlewareTest extends TestCase
{

    public function setUp()
    {
        parent::setUp();

        $this->userAuth = factory(User::class)->create([
            'name' => '123user',
            'email' => 'user@123.com',
            'password' => randomPassword(12),
            'api_token' => 'baaaaa',
            'tenant_id' => 'xyz',
        ]);

        DB::table('model_has_roles')
            ->insert([
                'role_id' => 1,
                'model_id' => $this->userAuth->id,
                'model_type' => 'App\User',
            ]);
    }

    public function testValidIp()
    {

        $result = IpMiddleware::ipCIDRCheck('172.0.0.2', IpMiddleware::privateCidr);

        $this->assertTrue($result);
        
    }

    public function testNotValidIpOutOfRange()
    {
        $result = IpMiddleware::ipCIDRCheck('172.0.0.2', '172.22.0.0/16');

        $this->assertFalse($result);

    }

    public function testNotValidIp()
    {
        $result = IpMiddleware::ipCIDRCheck('192.168.0.2', IpMiddleware::privateCidr);

        $this->assertFalse($result);

    }

}