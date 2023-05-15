<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Commands;

use legacy\src\Skipprd\Commands\PipelineCommand;
use legacy\src\Skipprd\Serders\SerdersFactory;
use legacy\src\Skipprd\Traits\Config;
use Mockery;
use Tests\TestCase;

class UnwrapEventPathTest extends TestCase
{

    protected function setUp(): void
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

        Config::$eventPath = 'messages.user_data.events';

        // emitArray via serder as it adds an outer array itself which unwrapEventPath() handles.
        $payload = json_encode($message);

        $serder = SerdersFactory::factory('json');
        $sourceMessages = $serder->deserialize($payload);

        $unwrappedMessages = $container->unwrapEventPath($sourceMessages);

        $this->assertEquals($message['messages']['user_data']['events'], $unwrappedMessages);


    }

}