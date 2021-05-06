<?php

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

class AnalyseSchemaDateFieldCandidateTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public function testSetValueDate()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "2019-08-30T14:09:51.807Z";
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $this->assertArrayHasKey($field, Config::$dateFieldCandidates);
        $this->assertArrayHasKey('valid_count', Config::$dateFieldCandidates[$field]);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['valid_count']);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['check_count']);

    }

    public function testUpToMaxDateCandidatesWhenNotDate()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "notadate-dont-endlessly-check-this-field";
        $field = 'foo';

        $dataType = $container->getLogicalType($field, $value);

        $this->assertArrayNotHasKey('valid_count', Config::$dateFieldCandidates[$field]);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['check_count']);

    }

    public function testTimestamp()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1567174191;
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);
        
        $this->assertEquals(1567174191, $value);
        $this->assertArrayHasKey($field, Config::$dateFieldCandidates);
        $this->assertArrayHasKey('valid_count', Config::$dateFieldCandidates[$field]);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['valid_count']);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['check_count']);

    }

    public function testTimestampString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '1567174191';
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $this->assertEquals(1567174191, $value);
        $this->assertArrayHasKey($field, Config::$dateFieldCandidates);
        $this->assertArrayHasKey('valid_count', Config::$dateFieldCandidates[$field]);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['valid_count']);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['check_count']);

    }

    public function testMilli()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1567174191000;
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $this->assertEquals(1567174191000, $value);
        $this->assertArrayHasKey($field, Config::$dateFieldCandidates);
        $this->assertArrayHasKey('valid_count', Config::$dateFieldCandidates[$field]);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['valid_count']);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['check_count']);

    }

    public function testMilliString()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = '1567174191000';
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $this->assertEquals(1567174191000, $value);
        $this->assertArrayHasKey($field, Config::$dateFieldCandidates);
        $this->assertArrayHasKey('valid_count', Config::$dateFieldCandidates[$field]);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['valid_count']);
        $this->assertEquals(1, Config::$dateFieldCandidates[$field]['check_count']);

    }
    
}