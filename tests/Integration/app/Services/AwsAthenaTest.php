<?php
///**
// * Created by PhpStorm.
// * User: huders2000
// * Date: 01/09/2019
// * Time: 12:41
// */
//
//namespace Tests\Integration;
//
//use App\IngestJob;
//use App\Plugins\DataOutputs\AwsAthena;
//use App\User;
//use Aws\S3\S3Client;
//use Illuminate\Contracts\Container\Container;
//use Illuminate\Support\Facades\Auth;
//use Skipprd\Services\MessageSerializer;
//use Skipprd\Traits\AnalyseSchema;
//use Superbalist\LaravelPubSub\PubSubConnectionFactory;
//use Superbalist\PubSub\Utils;
//use Tests\TestCase;
//use Illuminate\Foundation\Testing\DatabaseMigrations;
//use Illuminate\Foundation\Testing\DatabaseTransactions;
//use Mockery;
//use AvroSchema;
//
//class AwsAthenaTest extends TestCase
//{
//
//    protected function setUp()
//    {
//        parent::setUp();
//
//        $this->user = factory(User::class)->create([
//            'name' => 'xyzuser',
//            'email' => 'user@xyz.com',
//            'password' => randomPassword(12),
//            'api_token' => 'baaaaa',
//            'tenant_id' => 'xyztest',
//        ]);
//
//
//    }
//
//    public function testdeleteClientWorkGroup()
//    {
//
//        Auth::setUser($this->user);
//
//        putenv('AWS_PROFILE=skippr');
//
//        $ingestJob = new IngestJob();
//
//        $bucket = $ingestJob->getWorkgroupOutputBucketName();
//
//        $athenaClient = new AwsAthena($ingestJob);
//
//        $athenaClient->createWorkGroup();
//
//        // S3 Eventual Consistency
//        sleep(10);
//
//        $s3Client = new S3Client([
//            'region' => 'eu-west-2',
//            'version' => '2006-03-01',
//        ]);
//
//        $exists = $s3Client->doesBucketExist($bucket);
//        $this->assertFalse($exists); // bucket not created till query time
//
//        $workgroupExists = $athenaClient->getWorkGroup();
//        $this->assertTrue($workgroupExists);
//
//        $athenaClient->deleteClientWorkGroup();
//
//        sleep(10);
//
//        $exists = $s3Client->doesBucketExist($bucket);
//
//        $this->assertFalse($exists);
//
//    }
//
//}