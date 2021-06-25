<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Traits\AnalyseSchema;

use Illuminate\Contracts\Container\Container;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Services\MessageSerializer;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class AnalyseSchemaNumberTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function testSetValueInt()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 2147483647;
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('integer', $dataType);
        $this->assertEquals(2147483647, $value);
//                                        2147483647
    }

    public function testSetValueIntString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '8598265768';
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('long', $dataType);
        $this->assertEquals('8598265768', $value);

    }
    public function testSetValueIntTimestamp()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 123456;
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $value = $container->setValue($dataType,  $field, $value);
        
        $this->assertEquals('integer', $dataType);
        $this->assertEquals(123456, $value);

    }

    public function testSetValueIntStringTimestamp()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '123456';
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('integer', $dataType);
        $this->assertEquals('123456', $value);

    }

    public function testSetValueLong()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 853386065604908;
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('long', $dataType);
        $this->assertEquals(853386065604908, $value);

    }

    public function testSetValueLongString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '853386065604908';
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('long', $dataType);
        $this->assertEquals('853386065604908', $value);

    }

    public function testSetValueLongIntTimestampMilli()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 2147483647000;
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $value = $container->setValue($dataType,  $field, $value);
        
        $this->assertEquals('long', $dataType);
        $this->assertEquals(2147483647000, $value);

    }

    public function testSetValueLongIntStringTimestampMilli()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '2147483647000';
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $value = $container->setValue($dataType,  $field, $value);
        
        $this->assertEquals('long', $dataType);
        $this->assertEquals(2147483647000, $value);

    }


}