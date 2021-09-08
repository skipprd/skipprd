<?php

namespace Tests\Integration\KafkaToFile;

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

        // seed kafka with test data
//        $envs = [
//            'APP_ENV=dev',
//            'DATA_SOURCE_PLUGIN_NAME=file',
//            'DATA_SOURCE_PATH=/data/input',
//            'DATA_SOURCE_FORMAT=json',
//            'DEAD_LETTER_PLUGIN_NAME=file',
//            'DEAD_LETTER_PATH=/data/deadletters',
//            'DEAD_LETTER_FORMAT=json',
//            'DATA_OUTPUT_PLUGIN_NAME=kafka',
//            'DATA_OUTPUT_BROKERS=kafka:9092',
//            'DATA_OUTPUT_TOPIC=skippr_new',
//            'DATA_OUTPUT_KAFKA_CONFIG=compression.codec=zstd',
//            'DATA_OUTPUT_FORMAT=json',
//            'DATA_DIR=/data',
////            'TENANT_ID=skippr',
////            'PIPELINE_NAME=uattest',
//            'ANONYMOUS_METRICS=false',
////            'SCHEMA_REGISTRY=host.docker.internal:8081',
////            'SCHEMA_API_TOKEN=uyWYU7nxPzBAHZutIpjf376K368rL9r2',
//            'LICENSE_KEY=94708298-498f-4c74-802c-ff359dd56cdf',
//        ];
//
//        $this->containerConfig->setEnv($envs);
//
//        $containerCreateResult = $this->docker->containerCreate($this->containerConfig);
//
//        $this->docker->containerStart($containerCreateResult->getId());
//        $this->docker->containerWait($containerCreateResult->getId());
//
//        $containerCreateResult = $this->docker->containerCreate($this->containerConfig);
//
//        $this->docker->containerStart($containerCreateResult->getId());
//        $this->docker->containerWait($containerCreateResult->getId());
//
//        $this->cleanupTestDir();
//        $this->createTestDir();

    }

    public function testIsSequentialArrayKeys() {



        $envs = [
            'APP_ENV=dev',
            'DATA_SOURCE_PLUGIN_NAME=kafka',
            'DATA_SOURCE_BROKERS=kafka:9092',
            'DATA_SOURCE_TOPIC=skippr_new',
            'DATA_SOURCE_KAFKA_CONFIG=fetch.min.bytes=2000',
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
//            'SCHEMA_REGISTRY=host.docker.internal:8081',
//            'SCHEMA_API_TOKEN=uyWYU7nxPzBAHZutIpjf376K368rL9r2',
            'LICENSE_KEY=94708298-498f-4c74-802c-ff359dd56cdf',
        ];

        $this->containerConfig->setEnv($envs);

        $this->dockerRun();

        $this->assertParquetOutput();

    }

}
