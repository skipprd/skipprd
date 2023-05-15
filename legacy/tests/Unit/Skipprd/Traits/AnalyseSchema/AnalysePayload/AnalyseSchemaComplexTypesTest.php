<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Traits\AnalyseSchema\AnalysePayload;

use Illuminate\Contracts\Container\Container;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use legacy\src\Skipprd\Commands\PipelineCommand;
use legacy\src\Skipprd\Traits\AnalyseSchema;
use legacy\src\Skipprd\Traits\Config;
use Mockery;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;

class AnalyseSchemaComplexTypesTestComplexTypesTest extends TestCase
{

    protected function setUp(): void
    {
        parent::setUp();

        Config::$discoveredFieldOccurrence['foo_namespace']['fields'] = [];

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
//                'crank' => [
//                    'voltage' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
//                    'start_temprature' => 5,
//                    'end_temprature' => 7,
//                    'engine' => [
//                        'manufacturer' => 'General Electric',
//                        'model' => 'PZ - 09 - 126178',
//                        'rebuild_dates' => [
//                            0 => '01/02/19/85',
//                            1 => '15/06/19/2005',
//                        ],
//                    ]
//                ],
//                'history' => [
//                    'crank' => [
//                        'voltage' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
//                        'start_temprature' => 5,
//                    ]
//                ],
//                'neighbours' => [
//                    0 => 'Westfields Farm',
//                    1 => 'Leeway Holdings',
//                    2 => 'Jolly Rodgers Hoedown',
//                ],
                'crank_torques' => [ # list of lists
                    [2, 15, 33, 45, 56, 57, 47, 36, 19, 5],
                    [1, 13, 33, 48, 56, 58, 45, 35, 15, 6],
                ],
            ],
        ];

        AnalyseSchema::analysePayload($field, Config::$discoveredFieldOccurrence['foo_namespace']['fields']);
        
//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['type']['record']);
//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['fields']['sheep']['type']['string']);
//
//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['fields']['sheep']['type']['string']);
//
//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['fields']['crank']['fields']['voltage']['type']['array']);
//
//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['fields']['crank']['fields']['engine']['type']['record']);
//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['fields']['crank']['fields']['engine']['fields']['rebuild_dates']['type']['array']);

        ///////////////////////////

//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['fields']['crank_torques']['type']['array']);
//
//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['fields']['crank_torques']['fields'][0]['type']['array']);
//
//        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['fields']['crank_torques']['fields'][0]['fields'][0]['type']['integer']);


        $container->determineFieldTypes(Config::$discoveredFieldOccurrence['foo_namespace']['fields']);

        $foo = Config::$discoveredFieldOccurrence['foo_namespace']['fields'];

        $this->assertEquals('record', $foo["foo"]["fields"]["crank_torques"]["parent_type"]);

        $this->assertEquals('record', $foo["foo"]["fields"]["crank_torques"]["determined_type"]);

        $this->assertEquals('array', $foo["foo"]["fields"]["crank_torques"]["fields"][0]["determined_type"]);
        $this->assertEquals('array', $foo["foo"]["fields"]["crank_torques"]["fields"][1]["determined_type"]);

        $this->assertEmpty($foo["foo"]["fields"]["crank_torques"]["fields"][1]["fields"]);


    }
}