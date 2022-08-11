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

    public function testDecodeMessageLength()
    {

        $record = 'record_value';
        $offset = 'offset_value';

        $sp = new SkipprPack();

        $sp->encode($record, $offset);

        $msgLgn = $sp->decodeMessageLength();

        $this->assertStringContainsString(28, $msgLgn);
//        $this->assertStringNotContainsString($offset, $msgLgn);

    }

    public function testDecodeMessageBytes()
    {

        $string = 'record_value';
        $offset = 'offset_value';

        $spw = new SkipprPack();
        $spr = new SkipprPack();

        $spw->encode($string, $offset);
        $skipprPack = $spw->string();

        $spr->create($skipprPack);
        $record = $spr->decodeRecord();
        $offset = $spr->decodeOffset();
        $sizeBytes = $spr->length();
        $msgLgn = $spr->decodeMessageLength();

        $this->assertEquals($record, $string);
        $this->assertEquals(strlen($record), strlen($string));

    }

    public function testDecodeMessageJson()
    {

        $array = ['foo' => 123, 'bar' => 456];
        $offset = 'offset_value';

        $spw = new SkipprPack();
        $spr = new SkipprPack();

        $spw->encode(json_encode($array), $offset);
        $skipprPack = $spw->string();

        $spr->create($skipprPack);
        $record = json_decode($spr->decodeRecord(), true);
        $offset = $spr->decodeOffset();
        $sizeBytes = $spr->length();
        $msgLgn = $spr->decodeMessageLength();

        $this->assertEquals($record, $array);

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
