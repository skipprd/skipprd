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

/**
 * When inferring the field data type, we demote certain types (like bool) in favour
 * of primitive types.
 */
class AnalyseSchemaBooleanTest extends TestCase
{

    protected function setUp(): void
    {
        parent::setUp();

        Config::$discoveredFieldOccurrence['foo_namespace']['fields'] = [];
        
    }

    public function testBoolean()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');
        $field = [
            'foo' => [
                'boolean1' => 0,
                'boolean2' => 1,
                'boolean3' => false,
                'boolean4' => true,
                'boolean5' => '0',
                'boolean6' => '1',
                'boolean7' => 'false',
                'boolean8' => 'true',
                'blar' => 'to make this a record',

            ],
            'map' => [
                'boolean1' => [1, 0, 1, 1],
                'boolean2' => [false, false, false],
                'boolean3' => [true, true, true],
                'boolean4' => [1, 0, 1, 0, false, true, false, true],

            ]
        ];

        $container->analysePayload($field, Config::$discoveredFieldOccurrence['foo_namespace']['fields']);

        $container->determineFieldTypes(Config::$discoveredFieldOccurrence['foo_namespace']['fields']);

        $metadata = Config::$discoveredFieldOccurrence['foo_namespace']['fields'];

        $this->assertEquals(1, Config::$discoveredFieldOccurrence['foo_namespace']['fields']['foo']['type']['record']);

        $fieldYml = Config::$discoveredFieldOccurrence['foo_namespace']['fields']['foo']['fields'];

        $this->assertEquals('boolean', array_key_first($fieldYml['boolean1']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean1']['determined_type']);

        $this->assertEquals('boolean', array_key_first($fieldYml['boolean2']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean2']['determined_type']);

        $this->assertEquals('boolean', array_key_first($fieldYml['boolean3']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean3']['determined_type']);

        $this->assertEquals('boolean', array_key_first($fieldYml['boolean4']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean4']['determined_type']);

        $this->assertEquals('boolean', array_key_first($fieldYml['boolean5']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean5']['determined_type']);

        $this->assertEquals('boolean', array_key_first($fieldYml['boolean6']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean6']['determined_type']);

        $this->assertEquals('boolean', array_key_first($fieldYml['boolean7']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean7']['determined_type']);

        $this->assertEquals('boolean', array_key_first($fieldYml['boolean8']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean8']['determined_type']);


        $fieldYml = Config::$discoveredFieldOccurrence['foo_namespace']['fields']['map']['fields'];

        $this->assertEquals('array', array_key_first($fieldYml['boolean1']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean1']['determined_type_values']);

        $this->assertEquals('array', array_key_first($fieldYml['boolean2']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean2']['determined_type_values']);

        $this->assertEquals('array', array_key_first($fieldYml['boolean3']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean3']['determined_type_values']);

        $this->assertEquals('array', array_key_first($fieldYml['boolean4']['type']));
        $this->assertEquals('boolean', $fieldYml['boolean4']['determined_type_values']);

    }
}