<?php

namespace Tests\Integration\File;

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

class FiltersJsonTest extends DockerRun
{

    public function setUp()
    {
        $this->testFile = '100.json.gz';

        parent::setUp();
    }

    public function testFilterJsonEquals() {

        $envs = [
            'DATA_SOURCE_PLUGIN_NAME=file',
            'DATA_SOURCE_PATH=/data/input',
            'DATA_SOURCE_FORMAT=json',

            'FILTER_RIDERID_FIELD_PATH=rider_id',
            'FILTER_RIDERID_OPERATOR=eq',
            'FILTER_RIDERID_COMPARISON=10e974bf-4a43-305a-9e39-1636c43cb22a',
            'FILTER_RIDERID_ACTION=allow_record',

            'DATA_OUTPUT_FORMAT=json',
            'DATA_DIR=/data',
            'TENANT_ID=skippr',
            'PIPELINE_NAME=uattest',
            'ANONYMOUS_METRICS=false',
            'LICENSE_KEY=97d8afc1-6712-439b-9016-eee4ad6f37cc',
            'APP_ENV=dev',
        ];

        $this->containerConfig->setEnv($envs);

        $this->dockerRun(1, 99);

        $this->assertJsonOutput();

    }

    public function testFilterJsonIn() {

        $envs = [
            'DATA_SOURCE_PLUGIN_NAME=file',
            'DATA_SOURCE_PATH=/data/input',
            'DATA_SOURCE_FORMAT=json',

            'FILTER_RIDERID_FIELD_PATH=rider_id',
            'FILTER_RIDERID_OPERATOR=in',
            'FILTER_RIDERID_COMPARISON=10e974bf-4a43-305a-9e39-1636c43cb22a',
            'FILTER_RIDERID_ACTION=allow_record',

            'DATA_OUTPUT_FORMAT=json',
            'DATA_DIR=/data',
            'TENANT_ID=skippr',
            'PIPELINE_NAME=uattest',
            'ANONYMOUS_METRICS=false',
            'LICENSE_KEY=94708298-498f-4c74-802c-ff359dd56cdf',
            'APP_ENV=dev',
        ];

        $this->containerConfig->setEnv($envs);

        $this->dockerRun(1, 99);

        $this->assertJsonOutput();

    }

}
