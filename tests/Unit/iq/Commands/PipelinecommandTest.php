<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Commands;

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

class PipelineCommandTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function testGetUnwrappedRewrapMetadata()
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

        // parse via serder as it adds an outer array itself which unwrap() handles.
        $payload = json_encode($message);

        $serder = SerdersFactory::factory('json');
        $sourceMessages = $serder->deserialize($payload);

        $unwrappedMessages = $container->unwrap($sourceMessages);

        $this->assertEquals($message['messages']['user_data']['events'], $unwrappedMessages);


    }

}