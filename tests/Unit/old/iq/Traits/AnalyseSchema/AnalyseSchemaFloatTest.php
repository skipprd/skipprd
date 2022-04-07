<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Iq\Traits\AnalyseSchema;

use Illuminate\Contracts\Container\Container;
use Iq\Commands\PipelineCommand;
use Iq\Services\MessageSerializer;
use Iq\Traits\AnalyseSchema;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class AnalyseSchemaFloatTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function testSetValueFloat()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1.234;
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('double', $dataType);
        $this->assertEquals(1.234, $value);

    }

    public function testSetValueNegativeFloat()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = -1.234;
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('double', $dataType);
        $this->assertEquals(-1.234, $value);

    }

    public function testSetValueFloatString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '1.234';
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('double', $dataType);
        $this->assertEquals(1.234, $value);

    }

    public function testSetValueLongFloat()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1.234386065604908;
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('double', $dataType);
        $this->assertEquals(1.234386065604908, $value);

    }

    public function testSetValueLongFloatString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '1.234386065604908';
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('double', $dataType);
        $this->assertEquals(1.234386065604908, $value);

    }

    public function testSetValueVeryLongFloat()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 3.1415926535897932384626433832795;
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('double', $dataType);
        $this->assertEquals(3.1415926535897932384626433832795, $value);

    }

    public function testSetValueVeryLongFloatString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '3.1415926535897932384626433832795';
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $this->assertEquals('double', $dataType);
        $this->assertEquals(3.1415926535897932384626433832795, $value);

    }


}