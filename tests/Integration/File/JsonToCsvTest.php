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

class JsonToCsvTest extends DockerRun
{

    public function setUp()
    {
        $this->testFile = '0.json.gz';

        parent::setUp();
    }

    public function testIsSequentialArrayKeys() {

        $envs = [
            'DATA_SOURCE_PLUGIN_NAME=file',
            'DATA_SOURCE_PATH=/data',
            'DATA_SOURCE_FORMAT=json',
            'DEAD_LETTER_PLUGIN_NAME=file',
            'DEAD_LETTER_PATH=/data/deadletters',
            'DEAD_LETTER_FORMAT=json',
            'DATA_OUTPUT_PLUGIN_NAME=file',
            'DATA_OUTPUT_PATH=/data/output',
            'DATA_OUTPUT_FORMAT=csv',
            'DATA_DIR=/data',
            'TENANT_ID=skippr',
            'PIPELINE_NAME=uattest',
        ];

        $this->containerConfig->setEnv($envs);

        $this->dockerRun();

        $this->assertCsvOutput();

    }

}
