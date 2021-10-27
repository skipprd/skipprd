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
use PHPUnit\Framework\ExpectationFailedException;
use PHPUnit\Framework\TestCase;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Helpers;
use Skipprd\Serders\SerderAvroFile;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class DockerRun extends TestCase
{

    protected $docker;

    protected $containerConfig;

    public $dataPath;

    public $hostPath;

    public $testFile;

    protected $avroSchema = array (
        'skpr_event_ts' => NULL,
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
        0 => 'message schema {',
        1 => '  optional int32 skpr_event_ts;',
        2 => '  optional binary skpr_partition (UTF8);',
        3 => '  optional binary rider_id (UTF8);',
        4 => '  optional binary bike_id (UTF8);',
        5 => '  optional int64 isbn;',
        6 => '  required group trip (MAP) {',
        7 => '    repeated group key_value (MAP_KEY_VALUE) {',
        8 => '      required binary key (UTF8);',
        9 => '      optional int32 value;',
        10 => '    }',
        11 => '  }',
        12 => '  optional group last_crank (LIST) {',
        13 => '    repeated int32 array;',
        14 => '  }',
        15 => '  optional group crank_torques {',
        16 => '    optional group a0 (LIST) {',
        17 => '      repeated int32 array;',
        18 => '    }',
        19 => '    optional group a1 (LIST) {',
        20 => '      repeated int32 array;',
        21 => '    }',
        22 => '  }',
        23 => '  optional group hardware {',
        24 => '    optional binary manufacturer (UTF8);',
        25 => '    optional binary model (UTF8);',
        26 => '    optional group maintenance (MAP) {',
        27 => '      repeated group key_value (MAP_KEY_VALUE) {',
        28 => '        required binary key (UTF8);',
        29 => '        optional binary value (UTF8);',
        30 => '      }',
        31 => '    }',
        32 => '  }',
        33 => '  optional group metadata {',
        34 => '    optional int32 rcvd_time;',
        35 => '    optional int32 sent_time;',
        36 => '    optional double prcd_micro_time;',
        37 => '    optional group tags {',
        38 => '      optional group a0 (MAP) {',
        39 => '        repeated group key_value (MAP_KEY_VALUE) {',
        40 => '          required binary key (UTF8);',
        41 => '          optional binary value (UTF8);',
        42 => '        }',
        43 => '      }',
        44 => '      optional group a1 {',
        45 => '        optional binary name (UTF8);',
        46 => '        optional boolean value;',
        47 => '      }',
        48 => '    }',
        49 => '  }',
        50 => '}',
        51 => '',
    );

    public function cleanupTestDir()
    {
        array_map('unlink', glob("$this->dataPath/buffer/*"));
        array_map('rmdir', glob("$this->dataPath/buffer"));
//        array_map('rmdir', glob("$this->dataPath/buffer"));
        array_map( 'unlink', glob("$this->dataPath/output/*/*"));
        array_map('rmdir', glob("$this->dataPath/output/*"));
        array_map('unlink', glob("$this->dataPath/input/*"));
        array_map('unlink', glob("$this->dataPath/*.*"));
        array_map('rmdir', glob("$this->dataPath/*"));
        rmdir($this->dataPath);

    }

    public function createTestDir()
    {

        $tempPath = 'test-tmp/' . Helpers::randomStr(16);

        $this->basePath = realpath(__DIR__ . '/../../') . '/test-data/';
        $src = $this->basePath . $this->testFile;
        $this->dataPath = $this->basePath . '/' . $tempPath;
        mkdir($this->dataPath . '/input', 0777, true);
        copy($src, $this->dataPath . '/input/' . $this->testFile);

        $this->hostPath = getenv('HOST_PATH') . '/' . $tempPath;

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
        $this->containerConfig->setVolumes(new \ArrayObject([$this->dataPath => (object) []]));
        $hostConfig->setBinds([$this->hostPath . ':/data']);

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

    public function dockerRun() {

        $containerCreateResult = $this->dockerStart();

        $this->docker->containerWait($containerCreateResult->getId());

        $logs = (string) $this->docker->containerLogs($containerCreateResult->getId(), ['stdout' => true, 'stderr' => true], Docker::FETCH_RESPONSE)->getBody();

        $this->assertNotContains('error', $logs);
        $this->assertNotContains('fatal', $logs);

        $containerCreateResult = $this->dockerStart();

        $this->docker->containerWait($containerCreateResult->getId());

        $logs = (string) $this->docker->containerLogs($containerCreateResult->getId(), ['stdout' => true, 'stderr' => true], Docker::FETCH_RESPONSE)->getBody();

        $this->assertNotContains('error', $logs);
        $this->assertNotContains('fatal', $logs);

        $this->assertContains('Ingested 100 messages', $logs);
        $this->assertContains('Dead Letters 0 dead letters', $logs);

    }

    public function dockerRunWithInteruptions() {

        $containerCreateResult = $this->dockerStart();

        $this->docker->containerWait($containerCreateResult->getId());

        $logs = (string) $this->docker->containerLogs($containerCreateResult->getId(), ['stdout' => true, 'stderr' => true], Docker::FETCH_RESPONSE)->getBody();

        $this->assertNotContains('error', $logs);
        $this->assertNotContains('fatal', $logs);

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

    }

    public function dockerStart() {

        $name = Helpers::randomStr(16);
        $containerCreateResult = $this->docker->containerCreate($this->containerConfig, ['name' => $name]);

        // analyse run
        $this->docker->containerStart($containerCreateResult->getId());

        return $containerCreateResult;

    }

    public function assertParquetOutput($itemCount = '100') {

        $path = "$this->dataPath/buffer/*";

        $parquetSchema = $this->parquetSchema;

        $foundFiles = false;

        array_map(function ($file) use ($parquetSchema, &$foundFiles) {

            exec('parquet-tools schema ' . $file . ' 2>/dev/null', $output, $return);

            $this->assertEquals(0, $return);
            
            foreach ($parquetSchema as $key => $line) {
                $this->assertContains($line, $output[$key]);
            }

            $foundFiles = true;

        }, glob($path));

        $this->assertTrue($foundFiles);

        // Row count
        $path = "$this->dataPath/buffer";

        exec('parquet-tools rowcount ' . $path . ' 2>/dev/null', $rowsOutput, $return);

        print "Asserting expected row count of $itemCount equals actual " . $rowsOutput[0];

        $this->assertContains("Total RowCount: $itemCount", $rowsOutput);

    }

    public function assertJsonOutput() {

//        $path = realpath(__DIR__ . '/../../' . $this->dataPath);
        $path = "$this->dataPath/buffer/*";

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
        $path = "$this->dataPath/buffer/*";

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

                    // assert nested feild data is json encoded

                    $tripField = json_decode($data[5], true);

                    $this->assertIsArray($tripField);

                    $missingFields = array_diff_key($tripField, ['start_temprature' => '', 'end_temprature' => '']);

                    $this->assertEmpty($missingFields);

                    $doAsserts = false;
                }
                
            }

            $foundFiles = true;

        }, glob($path));

        $this->assertTrue($foundFiles);

    }

    public function assertAvroFileOutput() {

        $path = "$this->dataPath/buffer/*";

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
