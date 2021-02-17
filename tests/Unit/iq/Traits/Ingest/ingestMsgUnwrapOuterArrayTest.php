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
use Skipprd\Serders\SerdersFactory;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class ingestMsgUnwrapOuterArrayTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function discoverSchema($container, array $record)
    {

        // Discover Schema
//        $json = json_encode($record);
        $container->analysePayload($record);

        $container->determineFieldTypes($container->discoveredFieldOccurrence);
    }

    public function testUnwrapOuterArray()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');
        $container->shouldReceive('deadLetterMessage');

        $message = [
            'metrics' => [
                [
                    'address' => '509 Kings Hwy, Comptche, Missouri, 4848',
                    'phone' => '+1 (999) 407-2274',
                    'email' => 'blankenship.patrick@orbin.ca',
                    'company' => 'ORBIN',
                    'name' => [
                        'last' => 'Patrick',
                        'first' => 'Blankenship',
                    ],
                    '_id' => 'abc123',
                ],
                [
                    'address' => '509 Kings Hwy, Comptche, Missouri, 4848',
                    'phone' => '+1 (999) 407-2274',
                    'email' => 'blankenship.patrick@orbin.ca',
                    'company' => 'ORBIN',
                    'name' => [
                        'last' => 'Patrick',
                        'first' => 'Blankenship',
                    ],
                    '_id' => 'xyz789',
                ],
            ]
        ];

        $container->eventPath = 'metrics';

        $container->analysing = false;

        $this->discoverSchema($container, $message['metrics'][0]);

        // parse via serder as it adds an outer array itself which unwrap() handles.
        $payload = json_encode($message);

        $serder = SerdersFactory::factory('json');
        $sourceMessages = $serder->deserialize($payload);

        $offset = '';

        $unwrappedMessages = $container->unwrap($sourceMessages);

        $this->assertIsArray($unwrappedMessages);
        $this->assertNotEmpty($unwrappedMessages);

        $this->assertEquals('abc123', $unwrappedMessages[0]['_id']);
        $this->assertEquals('xyz789', $unwrappedMessages[1]['_id']);

    }

    public function testUnwrapOuterArrayDeep()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');
        $container->shouldReceive('deadLetterMessage');

        $message = [
            'messages' => [
                'metadata' => [
                    'useless' => 'data',
                    'irrelevant_time' => 0001
                ],
                'user_data' => [
                    'profile' => [
                        'name' => 'John',
                    ],
                    'events' => [
                        [
                            'address' => '509 Kings Hwy, Comptche, Missouri, 4848',
                            'phone' => '+1 (999) 407-2274',
                            'email' => 'blankenship.patrick@orbin.ca',
                            'company' => 'ORBIN',
                            'name' => [
                                'last' => 'Patrick',
                                'first' => 'Blankenship',
                            ],
                            '_id' => 'abc123',
                        ],
                        [
                            'address' => '509 Kings Hwy, Comptche, Missouri, 4848',
                            'phone' => '+1 (999) 407-2274',
                            'email' => 'blankenship.patrick@orbin.ca',
                            'company' => 'ORBIN',
                            'name' => [
                                'last' => 'Patrick',
                                'first' => 'Blankenship',
                            ],
                            '_id' => 'xyz789',
                        ],
                    ]
                ]
            ]
        ];


        $container->eventPath = 'messages.user_data.events';

        $container->analysing = false;

        $this->discoverSchema($container, $message['messages']['user_data']['events'][0]);

        // parse via serder as it adds an outer array itself which unwrap() handles.
        $payload = json_encode($message);
        
        $serder = SerdersFactory::factory('json');
        $sourceMessages = $serder->deserialize($payload);

        $offset = '';

        $unwrappedMessages = $container->unwrap($sourceMessages);

        $this->assertIsArray($unwrappedMessages);
        $this->assertNotEmpty($unwrappedMessages);

        $this->assertEquals('abc123', $unwrappedMessages[0]['_id']);
        $this->assertEquals('xyz789', $unwrappedMessages[1]['_id']);
        

    }

   
}