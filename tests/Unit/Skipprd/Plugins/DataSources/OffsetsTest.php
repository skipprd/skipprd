<?php

namespace Skipprd\Plugins\DataSources;

use PHPUnit\Framework\TestCase;

class OffsetsTest extends TestCase
{

//    public function testGetOffsetsNew()
//    {
//
//        $partition = 'table_a';
//
//        $offsets = new Offsets();
//
//        $commits = $offsets->parseOffsets($partition);
//
//        self::assertEquals([0], $commits);
//
//    }

    public function testParseOffsets()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets('123 123');

        $commits = $offsets->parseOffsets();

        self::assertEquals([123, 123], $commits);

    }

    /**
     * Validate offsets
     */

    public function testValidateOffsetFloatDecimalLess()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets( '123 12');

        $valid = $offsets->validateOffset( '123 4');

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetFloatDecimalGreater()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets( '123 4');

        $valid = $offsets->validateOffset( '123 12');

        self::assertEquals(true, $valid);

    }

    public function testValidateOffsetBothLess()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets( '123 123');

        $valid = $offsets->validateOffset( '122 122');

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetMinorLess()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets('123 123');

        $valid = $offsets->validateOffset('123 122');

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetMajorLess()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets('123 123');

        $valid = $offsets->validateOffset('122 123');

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetBothEqual()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets('123 123');

        $valid = $offsets->validateOffset('123 123');

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetBothGreater()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets('123 123');

        $valid = $offsets->validateOffset('124 124');

        self::assertEquals(true, $valid);

    }

    public function testValidateOffsetMinorGreater()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets('123 123');

        $valid = $offsets->validateOffset('123 124');

        self::assertEquals(true, $valid);

    }

    public function testValidateOffsetMajorGreater()
    {

        $partition = 'table_a';

        $offsets = new Offsets();
        $offsets->setOffsets('123 123');

        $valid = $offsets->validateOffset('124 123');

        self::assertEquals(true, $valid);

    }

}
