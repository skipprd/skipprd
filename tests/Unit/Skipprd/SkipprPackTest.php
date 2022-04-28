<?php

namespace Skipprd;

use PHPUnit\Framework\TestCase;

class SkipprPackTest extends TestCase
{

    public function testDecodeRecord()
    {

        $record = 'record_value';
        $offset = 'offset_value';

        $sp = new SkipprPack();

        $sp->encode($record, $offset);

        $decodedRecord = $sp->decodeRecord();

        $this->assertStringContainsString($record, $decodedRecord);
        $this->assertStringNotContainsString($offset, $decodedRecord);

    }

//    public function testString()
//    {
//
//    }
//
//    public function testSeek()
//    {
//
//    }
//
//    public function testTell()
//    {
//
//    }
//
//    public function testRewind()
//    {
//
//    }
//
//    public function testIs_eof()
//    {
//
//    }
//
//    public function testLength()
//    {
//
//    }
//
//    public function testRead()
//    {
//
//    }
//
//    public function testFpassthru()
//    {
//
//    }
//
//    public function testTruncate()
//    {
//
//    }
//
//    public function test__toString()
//    {
//
//    }
//
//    public function testDecodeOffset()
//    {
//
//    }
//
//    public function testEncode()
//    {
//
//    }
//
//    public function testWrite()
//    {
//
//    }
//
//    public function test__construct()
//    {
//
//    }
}
