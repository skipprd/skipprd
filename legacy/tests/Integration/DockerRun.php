<?php


namespace Tests\Integration;


use Docker\API\Model\BuildInfo;
use Docker\API\Model\ContainersCreatePostBody;
use Docker\API\Model\ContainersCreatePostBodyNetworkingConfig;
use Docker\API\Model\HostConfig;
use Docker\API\Model\Network;
use Docker\API\Model\NetworkContainer;
use Docker\API\Model\NetworkSettings;
use Docker\Docker;
use legacy\src\Skipprd\Helpers;
use legacy\src\Skipprd\Serders\SerderAvroFile;
use PHPUnit\Framework\ExpectationFailedException;
use PHPUnit\Framework\TestCase;

class DockerRun extends TestCase
{

    protected $docker;

    protected $containerConfig;

    public $basePath;

    public $dataPath;

    public $hostPath;

    public $testFile;

    protected $avroSchema = array (
        'skpr_event_ts' => NULL,
        'skpr_namespace' => NULL,
        'skpr_partition' => NULL,
        'rider_id' => NULL,
        'bike_id' => NULL,
        'isbn' => NULL,
        'trip' =>
            array (
                '' => 0,
            ),
        'last_crank' =>
            array (
            ),
        'crank_torques' =>
            array (
                'a0' =>
                    array (
                    ),
                'a1' =>
                    array (
                    ),
            ),
        'hardware' =>
            array (
                'manufacturer' => NULL,
                'model' => NULL,
                'maintenance' =>
                    array (
                        '' => '',
                    ),
            ),
        'metadata' =>
            array (
                'rcvd_time' => NULL,
                'sent_time' => NULL,
                'prcd_micro_time' => NULL,
                'tags' =>
                    array (
                        'a0' =>
                            array (
                                '' => '',
                            ),
                        'a1' =>
                            array (
                                'name' => NULL,
                                'value' => NULL,
                            ),
                    ),
            ),
    );

    protected $csvSchema = [
        'skpr_event_ts' => false,
        'skpr_namespace' => false,
        'skpr_partition' => false,
        'rider_id' => false,
        'bike_id' => false,
        'isbn' => false,
        'trip' => true,
        'last_crank' => true,
        'crank_torques' => true,
        'hardware' => true,
        'metadata' => true,
    ];

    protected $jsonSchema = [
            'skpr_event_ts' => 0,
            'skpr_namespace' => '',
            'skpr_partition' => '',
            'rider_id' => '10e974bf-4a43-305a-9e39-1636c43cb22a',
            'bike_id' => '8b86f753-05f8-3254-aba6-739188a3c0b6',
            'isbn' => 9407496597,
            'trip' =>
                array(
                    'start_temprature' => 0,
                    'end_temprature' => 2,
                ),
            'last_crank' =>
                array (
                    0 => 2,
                    1 => 15,
                    2 => 33,
                    3 => 45,
                    4 => 56,
                    5 => 57,
                    6 => 47,
                    7 => 36,
                    8 => 19,
                    9 => 5,
                ),
            'crank_torques' =>
                array(
                    'a0' =>
                        array (
                            0 => 2,
                            1 => 15,
                            2 => 33,
                            3 => 45,
                            4 => 56,
                            5 => 57,
                            6 => 47,
                            7 => 36,
                            8 => 19,
                            9 => 5,
                        ),
                    'a1' =>
                        array (
                            0 => 1,
                            1 => 13,
                            2 => 33,
                            3 => 48,
                            4 => 56,
                            5 => 58,
                            6 => 45,
                            7 => 35,
                            8 => 15,
                            9 => 6,
                        ),
                ),
            'hardware' =>
                array(
                    'manufacturer' => 'Beier, Emmerich and Rutherford',
                    'model' => 'synergize ubiquitous e-commerce',
                    'maintenance' =>
                        array(
                            'last_rebuild' => '20/04/2010',
                            'last_service' => '12/07/1973',
                        ),
                ),
            'metadata' =>
                array(
                    'rcvd_time' => 1615474895,
                    'sent_time' => 1615474930,
                    'prcd_micro_time' => 1615474853.999185,
                    'tags' =>
                        array(
                            'a0' =>
                                array(
                                    'name' => 'type',
                                    'value' => 'trip',
                                ),
                            'a1' =>
                                array(
                                    'name' => 'auto',
                                    'value' => false,
                                ),
                        ),
                ),
    ];

    protected $parquetSchema = array (
        'message schema {',
        '  optional int32 skpr_event_ts;',
        '  optional binary skpr_namespace (UTF8);',
        '  optional binary skpr_partition (UTF8);',
        '  optional binary rider_id (UTF8);',
        '  optional binary bike_id (UTF8);',
        '  optional int64 isbn;',
        '  required group trip (MAP) {',
        '    repeated group key_value (MAP_KEY_VALUE) {',
        '      required binary key (UTF8);',
        '      optional int32 value;',
        '    }',
        '  }',
        '  optional group last_crank (LIST) {',
        '    repeated int32 array;',
        '  }',
        '  optional group crank_torques {',
        '    optional group a0 (LIST) {',
        '      repeated int32 array;',
        '    }',
        '    optional group a1 (LIST) {',
        '      repeated int32 array;',
        '    }',
        '  }',
        '  optional group hardware {',
        '    optional binary manufacturer (UTF8);',
        '    optional binary model (UTF8);',
        '    optional group maintenance (MAP) {',
        '      repeated group key_value (MAP_KEY_VALUE) {',
        '        required binary key (UTF8);',
        '        optional binary value (UTF8);',
        '      }',
        '    }',
        '  }',
        '  optional group metadata {',
        '    optional int32 rcvd_time;',
        '    optional int32 sent_time;',
        '    optional double prcd_micro_time;',
        '    optional group tags {',
        '      optional group a0 (MAP) {',
        '        repeated group key_value (MAP_KEY_VALUE) {',
        '          required binary key (UTF8);',
        '          optional binary value (UTF8);',
        '        }',
        '      }',
        '      optional group a1 {',
        '        optional binary name (UTF8);',
        '        optional boolean value;',
        '      }',
        '    }',
        '  }',
        '}',
        '',
    );

    public function cleanupTestDir()
    {
        array_map('unlink', glob("$this->basePath/buffer/*"));
        array_map('rmdir', glob("$this->basePath/buffer"));
//        array_map('rmdir', glob("$this->dataPath/buffer"));
        array_map( 'unlink', glob("$this->basePath/output/*/*"));
        array_map('rmdir', glob("$this->basePath/output/*"));
        array_map('unlink', glob("$this->basePath/input/*"));
        array_map('unlink', glob("$this->basePath/*.*"));
        array_map('rmdir', glob("$this->basePath/*"));
        rmdir($this->basePath);

    }

    public function createTestDir()
    {

        $tempPath = md5(microtime());

        $this->dataPath = realpath(__DIR__ . '/../../') . '/test-data/';
        $this->basePath = realpath(__DIR__ . '/../../') . '/' . $tempPath;
        mkdir($this->basePath . '/input', 0777, true);

//        $this->hostPath = realpath(__DIR__ . '/../../') . '/' . $tempPath;

    }

    public function tearDown()
    {

        $this->cleanupTestDir();

        parent::tearDown(); // TODO: Change the autogenerated stub
    }

    public function setUp()
    {
        parent::setUp();

        $this->createTestDir();

//        \putenv('DOCKER_HOST=127.0.0.1:2375');

        $this->docker = Docker::create();

//        $inputStream = create_tar_stream_resource();
//        $buildStream = $this->docker->imageBuild($inputStream);
//        $buildStream->onFrame(function (BuildInfo $buildInfo) {
//            echo $buildInfo->getStream();
//        });
//        $buildStream->wait();

//        $this->docker->imageCreate( '',
//            [
//                'fromImage' => 'docker.io/skippr/skipprd',
//                'tag' => 'latest'
//            ]
//        );

        $this->containerConfig = new ContainersCreatePostBody();
        $this->containerConfig->setImage('skipprd:build');

        $hostConfig = new HostConfig();

        // volume
        $this->containerConfig->setVolumes(new \ArrayObject([$this->basePath => (object) []]));
        $hostConfig->setBinds([$this->basePath . ':/data']);

        // networking
//        $hostConfig->setNetworkMode('proxynet');

//        $net = new Network();
//        $net->setName('proxynet');
//        $netMap = new \ArrayObject();
//        $netMap[] = [$net];
//        $networkConfig = new NetworkSettings();
//        $networkConfig->setNetworks($netMap);


//        $networkContainer = new NetworkContainer();
//        $networkConfig->setNetworks(new \ArrayObject([
//            'proxynet' => $networkContainer
//        ]));

        $this->containerConfig->setHostConfig($hostConfig);

        $this->containerConfig->setAttachStdout(true);

    }

    public function dockerRun(int $msgCount = 100, int $deadletterCount = 0) {

        $src = $this->dataPath . $this->testFile;
        copy($src, $this->basePath . '/input/' . $this->testFile);

        $containerCreateResult = $this->dockerStart();

        $this->docker->containerWait($containerCreateResult->getId());

        $logs = (string) $this->docker->containerLogs($containerCreateResult->getId(), ['stdout' => true, 'stderr' => true], Docker::FETCH_RESPONSE)->getBody();

        try {
            $this->assertNotContains('error', $logs);
            $this->assertNotContains('fatal', $logs);
        } catch (ExpectationFailedException $e) {
            print $logs;
        }

        $containerCreateResult = $this->dockerStart();

        $this->docker->containerWait($containerCreateResult->getId());

        $logs = (string) $this->docker->containerLogs($containerCreateResult->getId(), ['stdout' => true, 'stderr' => true], Docker::FETCH_RESPONSE)->getBody();

        try {
            $this->assertNotContains('error', $logs);
            $this->assertNotContains('fatal', $logs);

            $this->assertContains("Ingested $msgCount messages", $logs);
            $this->assertContains("Dead Letters $deadletterCount dead letters", $logs);
        } catch (ExpectationFailedException $e) {
            print $logs;
        }

        $this->docker->containerDelete($containerCreateResult->getId());

    }

    public function dockerRunWithInteruptions() {

        $containerCreateResult = $this->dockerStart();

        $this->docker->containerWait($containerCreateResult->getId());

        $logs = (string) $this->docker->containerLogs($containerCreateResult->getId(), ['stdout' => true, 'stderr' => true], Docker::FETCH_RESPONSE)->getBody();

        try {
            $this->assertNotContains('error', $logs);
            $this->assertNotContains('fatal', $logs);
        } catch (ExpectationFailedException $e) {
            print $logs;
        }

        $containerCreateResult = $this->dockerStart();

        for ($i = 3; $i >= 0; $i--) {

            // interuption
            sleep(10);
            $this->docker->containerStop($containerCreateResult->getId());


            // continue run
            $containerCreateResult = $this->dockerStart();
        }

        $this->docker->containerWait($containerCreateResult->getId());

        $logs = (string) $this->docker->containerLogs($containerCreateResult->getId(), ['stdout' => true, 'stderr' => true], Docker::FETCH_RESPONSE)->getBody();

        $this->assertNotContains('error', $logs);
        $this->assertNotContains('fatal', $logs);

//        $this->assertContains('Ingested 100000 messages', $logs);
//        $this->assertContains('Dead Letters 0 dead letters', $logs);

        $this->docker->containerDelete($containerCreateResult->getId());
    }

    public function dockerStart() {

        $name = Helpers::randomStr(16);
        $containerCreateResult = $this->docker->containerCreate($this->containerConfig, ['name' => $name]);

        // analyse run
        $this->docker->containerStart($containerCreateResult->getId());

        return $containerCreateResult;

    }

    public function assertParquetOutput($itemCount = '100') {

        $path = "$this->basePath/buffer/*";

        $parquetSchema = $this->parquetSchema;

        $foundFiles = false;

        array_map(function ($file) use ($parquetSchema, &$foundFiles) {

            $file = escapeshellarg($file);

            exec('parquet-tools schema ' . $file . ' 2>/dev/null', $output, $return);

            $this->assertEquals(0, $return);
            
            foreach ($parquetSchema as $key => $line) {
                $this->assertContains($line, $output[$key]);
            }

            $foundFiles = true;

        }, glob($path));

        $this->assertTrue($foundFiles);

        // Row count
        $path = "$this->basePath/buffer";

        exec('parquet-tools rowcount ' . $path . ' 2>/dev/null', $rowsOutput, $return);

        print "Asserting expected row count of $itemCount equals actual " . $rowsOutput[0];

        $this->assertContains("Total RowCount: $itemCount", $rowsOutput);

    }

    public function assertJsonOutput() {

//        $path = realpath(__DIR__ . '/../../' . $this->dataPath);
        $path = "$this->basePath/buffer/*";

        $foundFiles = false;

        array_map(function ($file) use (&$foundFiles) {

            $fp = fopen($file, 'r');

            $line = fgets($fp);

            $data = json_decode($line, true);

            $this->assertIsArray($data);

            $missingFields = array_diff_key($data, $this->jsonSchema);

            $this->assertEmpty($missingFields);

            $foundFiles = true;

        }, glob($path));

        $this->assertTrue($foundFiles);

    }

    public function assertCsvOutput() {

//        $path = realpath(__DIR__ . '/../../' . $this->dataPath);
        $path = "$this->basePath/buffer/*";

        $foundFiles = false;

        array_map(function ($file) use (&$foundFiles) {

            $fp = fopen($file, 'r');

            $doAsserts = true;
            $i = 0;

            while (($data = fgetcsv($fp)) !== false && $doAsserts) {

                $this->assertIsArray($data);
                
                if ($i === 0) {

                    // assert all first level fields are present in csv headers
                    $dataKeys = array_flip($data);
                    $missingFields = array_diff_key($dataKeys, $this->csvSchema);

                    $this->assertEmpty($missingFields);

                    $i++;

                } elseif ($i === 1) {

                    // assert nested field data is json encoded

                    $tripField = json_decode($data[3], true);

                    $this->assertIsArray($tripField);

                    $missingFields = array_diff_key($tripField, ['start_temprature' => null, 'end_temprature' => null]);

                    $this->assertEmpty($missingFields);

                    $doAsserts = false;
                }
                
            }

            $foundFiles = true;

        }, glob($path));

        $this->assertTrue($foundFiles);

    }

    public function assertAvroFileOutput() {

        $path = "$this->basePath/buffer/*";

        $foundFiles = false;

        array_map(function ($file) use (&$foundFiles) {

            $content = file_get_contents($file);

            $serde = new SerderAvroFile();
            $data = $serde->deserialize($content);

            $this->assertIsArray($data);

            $missingFields = array_diff_key($data[0], $this->avroSchema);

            $this->assertEmpty($missingFields);

            $foundFiles = true;

        }, glob($path));

        $this->assertTrue($foundFiles);

    }

}
