<?php

namespace Tests\Integration\S3ToFile;

use Docker\API\Endpoint\VolumeCreate;
use Docker\API\Model\HostConfig;
use Docker\API\Model\HostConfigLogConfig;
use Docker\API\Model\Volume;
use Docker\API\Model\VolumesCreatePostBody;
use Tests\Integration\DockerRun;
use PHPUnit\Framework\TestCase;
use Docker\API\Model\ContainersCreatePostBody;
use Docker\Docker;
use Skipprd\Helpers;

class JsonToParquetTest extends DockerRun
{

    public function setUp()
    {
        $this->testFile = '100.json.gz';

        parent::setUp();
    }

    public function testIsSequentialArrayKeys() {

        $envs = [
            'DATA_SOURCE_PLUGIN_NAME=S3 Demo',
            'DATA_SOURCE_S3_BUCKET=skpr-sample-data',
            'DATA_SOURCE_S3_REGION=eu-west-2',
            'DATA_SOURCE_AWS_ACCESS_ID=AKIAXZ5BBYBFKM24LVMM',
            'DATA_SOURCE_AWS_SECRET_KEY=FRudxnOkrdfu5ntqEh+cPPrwmOqIlOou0NP8YAhg',
            'DATA_SOURCE_S3_PREFIX=bike-hire-100/' . $this->testFile,
            'DATA_SOURCE_FORMAT=json',
            'DEAD_LETTER_PLUGIN_NAME=file',
            'DEAD_LETTER_PATH=/data/deadletters',
            'DEAD_LETTER_FORMAT=json',
            'DATA_OUTPUT_PLUGIN_NAME=file',
            'DATA_OUTPUT_PATH=/data/output',
            'DATA_OUTPUT_FORMAT=parquet',
            'DATA_DIR=/data',
//            'TENANT_ID=skippr',
//            'PIPELINE_NAME=uattest',
            'ANONYMOUS_METRICS=false',
            'LICENSE_KEY=94708298-498f-4c74-802c-ff359dd56cdf',
            'APP_ENV=dev',
        ];

        $this->containerConfig->setEnv($envs);

        $this->dockerRun();

        $this->assertParquetOutput();

    }

}
