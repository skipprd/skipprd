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

/**
 * When inferring the field data type, we demote certain types (like bool) in favour
 * of primitive types.
 */
class AnalyseSchemaDemotedTypesTest extends TestCase
{

    protected function setUp(): void
    {
        parent::setUp();

        Config::$discoveredFieldOccurrence['foo_namespace']['fields'] = [];
        
    }

    /**
     * When we discover multiples, and one of those types is a logical type, we
     * demoted that type in favour of the other discovered primitive types.
     * However, when we only discover logically types, we should accept that as
     * the inferred type of the map's values
     *
     * @return void
     */
    public function testDemotedTypesTypes()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');
        $field = [
            'foo' => [
                'boolean' => [1, 0, 1, 1], // boolean
                'boolean2' => [false], // boolean
                'date' => ['2022-08-08', '2022-08-09'], // date
                'timestamp' => [1660331829, 1660331829], // timestamp
                'timestamp_milli' => [1660331874804, 1660331874805], // timestamp_milli
                'abc3' => [0, 1, 2, 3, 4],
                'abc4' => [0, 1, 2, 3], // @todo - this will fail as we infer 2x int and 2x bool
            ]
        ];

        AnalyseSchema::analysePayload($field, Config::$discoveredFieldOccurrence['foo_namespace']['fields']);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence['foo_namespace']['fields']);

        $metadata = Config::$discoveredFieldOccurrence['foo_namespace']['fields'];

        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo_namespace']['fields']['foo']['type']['record']);

        $fieldYml = Config::$discoveredFieldOccurrence['foo_namespace']['fields']['foo']['fields'];

        $this->assertEquals('array', array_key_first($fieldYml['boolean']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean']['determined_type_values']);

        $this->assertEquals('array', array_key_first($fieldYml['boolean2']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean2']['determined_type_values']);

        $this->assertEquals('array', array_key_first($fieldYml['date']['type']));
        $this->assertEquals('date', $fieldYml['date']['determined_type_values']);

//        $this->assertEquals('array', array_key_first($fieldYml['timestamp']['type']));
//        $this->assertEquals('timestamp', $fieldYml['timestamp']['determined_type_values']);
//
//        $this->assertEquals('array', array_key_first($fieldYml['timestamp_milli']['type']));
//        $this->assertEquals('timestamp_milli', $fieldYml['timestamp_milli']['determined_type_values']);

        $this->assertEquals('array', array_key_first($fieldYml['abc3']['type']));
        $this->assertEquals('integer', $fieldYml['abc3']['determined_type_values']);

    }

}