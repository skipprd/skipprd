<?php

namespace Tests\Integration\File;

use Docker\API\Endpoint\VolumeCreate;
use Docker\API\Model\ContainersCreatePostBody;
use Docker\API\Model\HostConfig;
use Docker\API\Model\HostConfigLogConfig;
use Docker\API\Model\Volume;
use Docker\API\Model\VolumesCreatePostBody;
use Docker\Docker;
use Tests\Integration\DockerRun;

class CsvToParquetTest extends DockerRun
{

    public function setUp()
    {
        $this->testFile = '100.csv.gz';

        parent::setUp();
    }

    public function testCsvToParquet() {

        $envs = [
            'DATA_SOURCE_PLUGIN_NAME=file',
            'DATA_SOURCE_PATH=/data/input',
            'DATA_SOURCE_FORMAT=csv',
//            'DEAD_LETTER_PLUGIN_NAME=file',
//            'DEAD_LETTER_PATH=/data/deadletters',
//            'DEAD_LETTER_FORMAT=json',
//            'DATA_OUTPUT_PLUGIN_NAME=file',
//            'DATA_OUTPUT_PATH=/data/output',
            'DATA_OUTPUT_FORMAT=parquet',
            'DATA_DIR=/data',
            'TENANT_ID=skippr',
            'PIPELINE_NAME=uattest',
            'ANONYMOUS_METRICS=false',
            'LICENSE_KEY=97d8afc1-6712-439b-9016-eee4ad6f37cc',
            'APP_ENV=dev',
        ];

        $this->containerConfig->setEnv($envs);

        echo "@todo - provide expected schema to assert against";
//        $this->dockerRun();

//        $this->assertParquetOutput();

        $this->assertTrue(false);
    }

}
