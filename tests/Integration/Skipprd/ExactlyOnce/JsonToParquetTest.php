<?php

namespace Tests\Integration\Skipprd\ExactlyOnce;

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

        $this->testFile = '0.json.gz';

        $this->envs = [
            'DATA_SOURCE_PLUGIN_NAME=file',
            'DATA_SOURCE_PATH=/data/input',
            'DATA_SOURCE_FORMAT=json',
//            'DEAD_LETTER_PLUGIN_NAME=file',
//            'DEAD_LETTER_PATH=/data/deadletters',
//            'DEAD_LETTER_FORMAT=json',
//            'DATA_OUTPUT_PLUGIN_NAME=file',
//            'DATA_OUTPUT_PATH=/data/output',
            'DATA_OUTPUT_FORMAT=parquet',
            'DATA_DIR=/data',
//            'TENANT_ID=skippr',
//            'PIPELINE_NAME=uattest',
            'ANONYMOUS_METRICS=false',
            'LICENSE_KEY=97d8afc1-6712-439b-9016-eee4ad6f37cc',
            'APP_ENV=dev',
        ];

        parent::setUp();
        
    }

    public function testRun1() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun2() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun3() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun4() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun5() {

        $this->containerConfig->setEnv($this->envs);
        
        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun6() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun7() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun8() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun9() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

    public function testRun10() {

        $this->containerConfig->setEnv($this->envs);

        $this->dockerRunWithInteruptions();

        $this->assertParquetOutput('100000');
    }

}
