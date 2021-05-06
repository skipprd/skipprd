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

class IngestFieldEventTimeTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }


    public function testTimestamp()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

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

        Config::$timeFields[] = 'event_time';

        $container->parseTimeField($message);
         
        $this->assertIsArray($message);

        $this->assertEquals($message['event_time'], 0001);
        $this->assertEquals($message['skpr_event_ts'], 0001);


    }

    public function testTimestampNested()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

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
                'metadata' => [
                    'event_time' => 0001,
                ]
            ],
        ];

        Config::$timeFields[] = 'customer.metadata.event_time';

        $container->parseTimeField($message);

        $this->assertIsArray($message);

        $this->assertEquals($message['customer']['metadata']['event_time'], 0001);
        $this->assertEquals($message['skpr_event_ts'], 0001);


    }

    public function testTimestampNestedNotInMessage()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

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
                'metadata' => [
                    'event_time' => 0001,
                ]
            ],
        ];

        Config::$timeFields[] = 'customer.other.foo_time';
//        Config::$timeFields[] = 'customer.metadata.event_time';

        $container->parseTimeField($message);

        $this->assertIsArray($message);

        $this->assertEquals($message['customer']['metadata']['event_time'], 0001);
        $this->assertNotEquals($message['skpr_event_ts'], 0001);
        $this->assertEquals($message['skpr_event_ts'], 0);


    }
   
}