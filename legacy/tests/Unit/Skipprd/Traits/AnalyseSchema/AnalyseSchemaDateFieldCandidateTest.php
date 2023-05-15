<?php

namespace Unit\Skipprd\Traits\AnalyseSchema;

use Illuminate\Contracts\Container\Container;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use legacy\src\Skipprd\Commands\PipelineCommand;
use legacy\src\Skipprd\Traits\AnalyseSchema;
use legacy\src\Skipprd\Traits\Config;
use Mockery;
use Skipprd\Services\MessageSerializer;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;

class AnalyseSchemaDateFieldCandidateTest extends TestCase
{

    protected function setUp(): void
    {
        parent::setUp();

    }

    public function testSetValueDate()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "2019-08-30T14:09:51.807Z";
        $field = 'foo';
        $dataType = AnalyseSchema::getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

//        $this->assertArrayHasKey($field, Config::$discoveredFieldOccurrence[$field]['date_candidate']);
        $this->assertArrayHasKey('valid_count', Config::$discoveredFieldOccurrence[$field]['date_candidate']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['valid_count']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['check_count']);

    }

    public function testUpToMaxDateCandidatesWhenNotDate()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "notadate-dont-endlessly-check-this-field";
        $field = 'foo';

        $dataType = AnalyseSchema::getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $this->assertArrayNotHasKey('valid_count', Config::$discoveredFieldOccurrence[$field]['date_candidate']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['check_count']);

    }

    public function testTimestamp()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1567174191;
        $field = 'foo';
        $dataType = AnalyseSchema::getLogicalType($field, $value, Config::$discoveredFieldOccurrence);
        
        $this->assertEquals(1567174191, $value);
//        $this->assertArrayHasKey($field, Config::$discoveredFieldOccurrence['date_candidate']);
        $this->assertArrayHasKey('valid_count', Config::$discoveredFieldOccurrence[$field]['date_candidate']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['valid_count']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['check_count']);

    }

    public function testTimestampString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '1567174191';
        $field = 'foo';
        $dataType = AnalyseSchema::getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $this->assertEquals(1567174191, $value);
//        $this->assertArrayHasKey($field, Config::$discoveredFieldOccurrence['date_candidate']);
        $this->assertArrayHasKey('valid_count', Config::$discoveredFieldOccurrence[$field]['date_candidate']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['valid_count']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['check_count']);

    }

    public function testMilli()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1567174191000;
        $field = 'foo';
        $dataType = AnalyseSchema::getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $this->assertEquals(1567174191000, $value);
//        $this->assertArrayHasKey($field, Config::$discoveredFieldOccurrence['date_candidate']);
        $this->assertArrayHasKey('valid_count', Config::$discoveredFieldOccurrence[$field]['date_candidate']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['valid_count']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['check_count']);

    }

    public function testMilliString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '1567174191000';
        $field = 'foo';
        $dataType = AnalyseSchema::getLogicalType($field, $value, Config::$discoveredFieldOccurrence);

        $this->assertEquals(1567174191000, $value);
//        $this->assertArrayHasKey($field, Config::$discoveredFieldOccurrence['date_candidate']);
        $this->assertArrayHasKey('valid_count', Config::$discoveredFieldOccurrence[$field]['date_candidate']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['valid_count']);
        $this->assertEquals(1, Config::$discoveredFieldOccurrence[$field]['date_candidate']['check_count']);

    }
    
}