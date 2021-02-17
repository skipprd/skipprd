<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Tests\Integration;

use App\IngestJob;
use App\Services\AwsS3;
use Aws\S3\S3Client;
use Illuminate\Contracts\Container\Container;
use Skipprd\Services\MessageSerializer;
use Skipprd\Traits\AnalyseSchema;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Superbalist\PubSub\Utils;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class AwsS3Test extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function testbucketCreateAndDestroy()
    {

        $ingestJob = new IngestJob();

        putenv('AWS_PROFILE=skippr');

        $bucket = 'skpr-test-' . strtolower(randomPassword(6));

        $s3 = new AwsS3($ingestJob);

        $s3->bucketCreateUpdate($bucket);

        // S3 Eventual Consistency 
        sleep(10);

        $client = new S3Client([
            'region' => 'eu-west-2',
            'version' => '2006-03-01',
        ]);

        $exists = $client->doesBucketExist($bucket);
        $this->assertTrue($exists);

        $s3->bucketDestroy($bucket);

        sleep(10);

        $exists = $client->doesBucketExist($bucket);

        $this->assertFalse($exists);

    }

}