<?php

namespace Skipprd;

use PHPUnit\Framework\TestCase;
use Skipprd\SkipprLogger;

class SkipprPackTest extends TestCase
{

    public function testDecodeRecord()
    {

        $record = 'record_value';
        $offset = 'offset 123';

        $sp = new SkipprPack();

        $sp->encode($record, $offset);

        $decodedRecord = $sp->decodeRecord();

        $this->assertStringContainsString($record, $decodedRecord);
        $this->assertStringNotContainsString($offset, $decodedRecord);

    }

    public function testDecodeMessageLength()
    {

        $record = 'record_value';
        $offset = 'offset 123';

        $sp = new SkipprPack();

        $sp->encode($record, $offset);

        $msgLgn = $sp->decodeMessageLength();

        $this->assertStringContainsString(12, $msgLgn);
//        $this->assertStringNotContainsString($offset, $msgLgn);

    }

    public function testDecodeMessageBytes()
    {

        $string = 'record_value';
        $offset = 'offset 123';

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
        $offset = 'offset 123';

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


    public function testDecodeRecordFromBinaryFile()
    {

        $array = ['foo' => 123, 'bar' => 456];
        $offset = 'offset 123'; // NOTE, something about this offset is interpreted as a new line char when packed to binary

        $spw = new SkipprPack();
        $spr = new SkipprPack();

        $spw->encode(json_encode($array), $offset);
//        $spw->encode(serialize($array), $offset);
        $skipprPack = $spw->string();

//        $skipprPack = preg_replace('/[[:cntrl:]]/', '', $skipprPack);

        $fp = fopen('test', 'wb');
        fputs($fp, "$skipprPack");
        fflush($fp);
        fclose($fp);

        $fp = fopen('test', 'rb');

        $line = '';
//        while (($buf = fgets($fp)) !== false) {
//            $line .= $buf;
//        }
        $line = fgets($fp);

        fclose($fp);

        try {
            $spr->create($line);
            $record = json_decode($spr->decodeRecord(), true);
//            $record = msgpack_unpack($spr->decodeRecord());
            $offset = $spr->decodeOffset();
            $sizeBytes = $spr->length();
            $msgLgn = $spr->decodeMessageLength();

        } catch (\Exception $e) {
            SkipprLogger::info($e->getMessage());
        }
        catch (\Error $e) {
            SkipprLogger::info($e->getMessage());
        }

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
