<?php

namespace Unit\Skipprd\Traits\AnalyseSchema;

use Illuminate\Contracts\Container\Container;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Services\MessageSerializer;
use Skipprd\Traits\AnalyseSchema;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class AnalyseSchemaDateTest extends TestCase
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

        $value = $container->setValue($dataType,  $field, $value);

        $dateCandidates = $container->dateFieldCandidates;

        $this->assertArrayHasKey($field, $dateCandidates);
        
        $this->assertEquals('date', $dataType);
        $this->assertEquals(1567174191, $value);

    }

    public function testMaxDateCandidatesDateCheck()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "2019-08-30T14:09:51.807Z";
        $field = 'foo';
        $container->dateFieldCandidates[$field]['check_count'] = 100;
        $container->dateFieldCandidates[$field]['valid_count'] = 100;

        $dataType = $container->getLogicalType($field, $value);

        $dateCandidates = $container->dateFieldCandidates;

        $this->assertArrayHasKey($field, $dateCandidates);

        $this->assertEquals('date', $dataType);
        $this->assertEquals(100, $container->dateFieldCandidates[$field]['check_count']);

    }

    public function testUpToMaxDateCandidatesDate()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "2019-08-30T14:09:51.807Z";
        $field = 'foo';
        $container->dateFieldCandidates[$field]['check_count'] = 99;

        $dataType = $container->getLogicalType($field, $value);

        $dateCandidates = $container->dateFieldCandidates;

        $this->assertArrayHasKey($field, $dateCandidates);

        $this->assertEquals('date', $dataType);
        $this->assertEquals(100, $container->dateFieldCandidates[$field]['check_count']);

    }

    public function testUpToMaxDateCandidatesWhenNotDate()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "notadate-dont-endlessly-check-this-field";
        $field = 'foo';
        $container->dateFieldCandidates[$field]['check_count'] = 99;

        $dataType = $container->getLogicalType($field, $value);

        // Second check (101) should skip
        $dataType = $container->getLogicalType($field, $value);

        $dateCandidates = $container->dateFieldCandidates;

        $this->assertArrayHasKey($field, $dateCandidates);

        $this->assertNotEquals('date', $dataType);
        $this->assertEquals(100, $container->dateFieldCandidates[$field]['check_count']);

    }

//    public function testSetValueDate2()
//    {
//
//        $container = Mockery::mock(IngestSchema::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//
//        $value = "2019-08-30 14:09:51";
//        $field = 'foo';
//        $dataType = $container->getLogicalType($field, $value);
//
//        $value = $container->setValue($dataType,  $field, $value);
//
//        $this->assertEquals('date', $dataType);
//        $this->assertEquals(1567174191, $value);
//
//    }

    public function testSetValueDateOnly()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = "2015-03-23";
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $dateCandidates = $container->dateFieldCandidates;

        $this->assertArrayHasKey($field, $dateCandidates);

        $this->assertEquals('date', $dataType);
        $this->assertEquals(1427068800, $value);

    }

    public function testTimestamp()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1567174191;
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $dateCandidates = $container->dateFieldCandidates;

        $this->assertArrayHasKey($field, $dateCandidates);

        $this->assertEquals('integer', $dataType);
        $this->assertEquals(1567174191, $value);

    }

    public function testTimestampMilliseconds()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');

        $value = 1567174191000;
        $field = 'foo';
        $dataType = $container->getLogicalType($field, $value);

        $value = $container->setValue($dataType,  $field, $value);

        $dateCandidates = $container->dateFieldCandidates;

        $this->assertArrayHasKey($field, $dateCandidates);

        $this->assertEquals('long', $dataType);
        $this->assertEquals(1567174191000, $value);

    }

    
}