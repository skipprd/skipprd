<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Traits\AnalyseSchema\AnalysePayload;

use Illuminate\Contracts\Container\Container;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class AnalyseSchemaAvroArrayTypesTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function testArrayTypes()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

        $field = [
            'foo' => [
                'abc1' => [2, 3, 4, 6, 7, 4, 3, 6, 7, 9],
                'abc2' => ['a', 'b', 'c'],
                'abc3' => ["0" => 'a', "1" => 'b', "2" => 'c'],
                'abc4' => ["1" => 'a', "0" => 'b', "2" => 'c'],
                'abc5' => ["a" => 123, "b" => 456, "c" => 789],
                'abc6' => ["abc", 123, null, 123.456],
            ]
        ];

        $container->analysePayload($field, Config::$discoveredFieldOccurrence);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence);
        
        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo']['type']['record']);

        $fieldYml = Config::$discoveredFieldOccurrence['foo']['fields'];

        $this->assertEquals('array', array_key_first($fieldYml['abc1']['type']));
        $this->assertEquals('array', array_key_first($fieldYml['abc2']['type']));
        $this->assertEquals('array', array_key_first($fieldYml['abc3']['type']));
        $this->assertEquals('map', array_key_first($fieldYml['abc4']['type']));
        $this->assertEquals('map', array_key_first($fieldYml['abc5']['type']));
        $this->assertEquals('record', array_key_first($fieldYml['abc6']['type']));

    }
}