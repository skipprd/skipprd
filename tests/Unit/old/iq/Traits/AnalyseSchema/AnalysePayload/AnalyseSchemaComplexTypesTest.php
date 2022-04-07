<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Iq\Traits\AnalyseSchema\AnalysePayload;

use Illuminate\Contracts\Container\Container;
use Iq\Commands\PipelineCommand;
use Iq\Traits\AnalyseSchema;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class AnalyseSchemaComplexTypesTestComplexTypesTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function testComplexType()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

        $field = [
            'foo' => [
                'sheep' => 'dog',
                'arable' => false,
                'crank' => [
                    'voltage' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
                    'start_temprature' => 5,
                    'end_temprature' => 7,
                    'engine' => [
                        'manufacturer' => 'General Electric',
                        'model' => 'PZ - 09 - 126178',
                        'rebuild_dates' => [
                            0 => '01/02/19/85',
                            1 => '15/06/19/2005',
                        ],
                    ]
                ],
                'history' => [
                    'crank' => [
                        'voltage' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
                        'start_temprature' => 5,
                    ]
                ],
                'neighbours' => [
                    0 => 'Westfields Farm',
                    1 => 'Leeway Holdings',
                    2 => 'Jolly Rodgers Hoedown',
                ],
            ],
        ];

//        $json = json_encode($field);

        $container->analysePayload($field);
         
        $this->assertEquals(1, $container->discoveredFieldOccurrence['foo']['type']['record']);
        $this->assertEquals(1, $container->discoveredFieldOccurrence['foo']['fields']['sheep']['type']['string']);

        $this->assertEquals(1, $container->discoveredFieldOccurrence['foo']['fields']['sheep']['type']['string']);

        $this->assertEquals(1, $container->discoveredFieldOccurrence['foo']['fields']['crank']['fields']['voltage']['type']['array']);

        $this->assertEquals(1, $container->discoveredFieldOccurrence['foo']['fields']['crank']['fields']['engine']['type']['record']);
        $this->assertEquals(1, $container->discoveredFieldOccurrence['foo']['fields']['crank']['fields']['engine']['fields']['rebuild_dates']['type']['array']);
    }
}