<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Traits\Ingest;

use Illuminate\Contracts\Container\Container;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use legacy\src\Skipprd\Commands\PipelineCommand;
use legacy\src\Skipprd\Serders\SerdersFactory;
use legacy\src\Skipprd\Traits\Config;
use Mockery;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;

class ingestMsgUnwrapOuterArrayTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function discoverSchema($container, array $record)
    {

        // Discover Schema
        Config::$discoveredFieldOccurrence['foo_namespace'] = [];
        
        $container->analysePayload($record, Config::$discoveredFieldOccurrence['foo_namespace']['fields']);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence['foo_namespace']['fields']);
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

        Config::$eventPath = 'metrics';

        Config::$analysing = false;

        $this->discoverSchema($container, $message['metrics'][0]);

        // emitArray via serder as it adds an outer array itself which unwrapEventPath() handles.
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


        Config::$eventPath = 'messages.user_data.events';

        Config::$analysing = false;

        $this->discoverSchema($container, $message['messages']['user_data']['events'][0]);

        // emitArray via serder as it adds an outer array itself which unwrapEventPath() handles.
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