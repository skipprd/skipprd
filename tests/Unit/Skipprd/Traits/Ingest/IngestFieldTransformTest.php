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
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class ingestFieldTransformTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function discoverSchema($container, array $record)
    {

        // Discover Schema
        Config::$discoveredFieldOccurrence['foo_partition'] = [];

        $container->analysePayload($record, Config::$discoveredFieldOccurrence['foo_partition']);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence['foo_partition']);
    }


    public function testDrop()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');
        $container->createLogger();

        $origMessage = [
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
            ]
        ];

        $this->discoverSchema($container, $origMessage);

        Config::$discoveredFieldOccurrence['foo_partition']['customer']['fields']['phone']['transform'] = 'drop';
        Config::$discoveredFieldOccurrence['foo_partition']['customer']['fields']['email']['transform'] = 'drop';
        Config::$discoveredFieldOccurrence['foo_partition']['customer']['fields']['name']['fields']['first']['transform'] = 'drop';
        Config::$discoveredFieldOccurrence['foo_partition']['customer']['fields']['name']['fields']['last']['transform'] = 'drop';


        $message = $origMessage;

        foreach ($message as $field => $value) {
            $container->ingestField($field, $value, Config::$discoveredFieldOccurrence['foo_partition'], $message);
        }
         
        $this->assertIsArray($message);

        $this->assertEquals($origMessage['customer']['address'], $message['customer']['address']);

        $this->assertEquals(null, $message['customer']['phone']);
        $this->assertEquals(null, $message['customer']['email']);

        $this->assertIsArray($message['customer']['name']);
        $this->assertEquals(null, $message['customer']['name']['first']);
        $this->assertEquals(null, $message['customer']['name']['last']);

    }

    public function testMask()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

        $origMessage = [
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
            ]
        ];
        
        $this->discoverSchema($container, $origMessage);

        Config::$discoveredFieldOccurrence['foo_partition']['customer']['fields']['phone']['transform'] = 'mask';
        Config::$discoveredFieldOccurrence['foo_partition']['customer']['fields']['email']['transform'] = 'mask';
        Config::$discoveredFieldOccurrence['foo_partition']['customer']['fields']['name']['fields']['first']['transform'] = 'mask';
        Config::$discoveredFieldOccurrence['foo_partition']['customer']['fields']['name']['fields']['last']['transform'] = 'mask';


        $message = $origMessage;

        foreach ($message as $field => $value) {
            $container->ingestField($field, $value, Config::$discoveredFieldOccurrence['foo_partition'], $message);
        }

        $this->assertIsArray($message);

        $this->assertEquals($origMessage['customer']['address'], $message['customer']['address']);

        $this->assertEquals('', $message['customer']['phone']);
        $this->assertEquals('', $message['customer']['email']);

        $this->assertIsArray($message['customer']['name']);
        $this->assertEquals('', $message['customer']['name']['first']);
        $this->assertEquals('', $message['customer']['name']['last']);

    }
}