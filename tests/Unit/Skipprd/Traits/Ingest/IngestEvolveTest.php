<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Traits\Ingest;

use Illuminate\Contracts\Container\Container;
use Skipprd\Commands\PipelineCommand;
use Skipprd\InternalFields;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class IngestEvolveTest extends TestCase
{

    protected function setUp(): void
    {
        parent::setUp();

    }

    public function testIngestEvolveComplexRecord()
    {


//        $container = $this->getMockBuilder(Ingest::class)
//        $container = $this->getMockBuilder(PipelineCommand::class)
//            ->setMethods(['slowPathIngest'])
//            ->getMock();

        $ingest = Mockery::mock(Ingest::class)->makePartial();
        $pipelineContainer = Mockery::mock(PipelineCommand::class)->makePartial();
//        $pipelineContainer->shouldReceive('Ingest');
        $pipelineContainer->shouldReceive('slowPathIngest');
        $pipelineContainer->shouldReceive('fastPathIngest');
//        $pipelineContainer->shouldReceive('ingestPayload');
//        $pipelineContainer->shouldReceive('finaliseFieldMapping');

        $message = [
            'customer' => [
                'address' => '509 Kings Hwy, Comptche, Missouri, 4848',
                'phone' => '+1 (999) 407-2274',
                'email' => 'blankenship.patrick@orbin.ca',
                'company' => 'ORBIN',
                'name' => [
                    'last' => 'Patrick',
                    'first' => 'Blankenship',
                ],
                '_id' => '5730864df388f1d653e37e6f',
            ],
            'event_time' => 0001,
        ];

        Config::$analysing = false;
        Config::$runMode = Config::RUN_MODE_SYNC;
        Config::$mutableMode = Config::MUTABLE_MODE_EVOLVE;

        Config::$discoveredFieldOccurrence['foo']['fields'] = [];

//        foreach ($message as $field => $value) {
//        $dataType = $this->resolveFieldType( Config::$discoveredFieldOccurrence['foo']['fields'], $field, $value);
//            $this->analyseField($field, $value,
//                Config::$discoveredFieldOccurrence['foo']['fields']);
//                $dataType = Config::$discoveredFieldOccurrence[$field]['determined_type'];

//            $ingest->ingestField($field, $value, Config::$discoveredFieldOccurrence['foo']['fields'], $message);
//        }

        $metadata = Config::$discoveredFieldOccurrence;

        $payload = $pipelineContainer->slowPathIngest($message, 'foo');

        $this->assertIsArray($payload);
        $this->assertEquals($message, $payload);

        $metadata = Config::$discoveredFieldOccurrence;

        $payload = $pipelineContainer->fastPathIngest($message, 'foo');

        $metadata = Config::$discoveredFieldOccurrence;

        $this->assertIsArray($payload);
        $this->assertEquals($message, $payload);


    }

}