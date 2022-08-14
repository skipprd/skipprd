<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Serders;

use Skipprd\Serders\SerdersFactory;
use Tests\TestCase;

class DeserializeCsvTest extends TestCase
{

    protected $format;

    protected $serder;

    protected function setUp(): void
    {
        parent::setUp();

        $this->format = 'csv';
        $this->serder = SerdersFactory::factory($this->format);

    }

    public function testValidCsvWithTrailingComma()
    {

        $record = <<<EOF
"Name","Age",\n"Paul Hudson","35",
EOF;

        $msgs = $this->serder->deserialize($record);

        $this->assertEquals('Paul Hudson', $msgs[0]['Name']);
        $this->assertEquals('35', $msgs[0]['Age']);
    }
    
    public function testValidCsvWithHeader()
    {

        
        $record = <<<EOF
"Name","Age"
"Paul Hudson","35"
EOF;

        $msgs = $this->serder->deserialize($record);

        $this->assertEquals('Paul Hudson', $msgs[0]['Name']);
        $this->assertEquals('35', $msgs[0]['Age']);

    }

    public function testValidCsvWithHeaderMultiLine()
    {

        
        $record = <<<EOF
"Name","Age"
"Paul Hudson","35";
"Natalia Hudson","35";
EOF;

        $msgs = $this->serder->deserialize($record);

        $this->assertEquals('Paul Hudson', $msgs[0]['Name']);
        $this->assertEquals('Natalia Hudson', $msgs[1]['Name']);

    }

    public function testValidCsvWithoutHeaderSingleLine()
    {

        
        $record = <<<EOF
Paul Hudson,35
EOF;

        $msgs = $this->serder->deserialize($record);

        $this->assertEquals('Paul Hudson', $msgs[0][0]);
        $this->assertEquals('35', $msgs[0][1]);

    }

    public function testValidCsvPipeDelimiter()
    {

        
        $record = <<<EOF
Paul Hudson|35
EOF;

        $msgs = $this->serder->deserialize($record);

        $this->assertEquals('Paul Hudson', $msgs[0][0]);
        $this->assertEquals('35', $msgs[0][1]);

    }

    public function testValidCsvTabDelimiter()
    {

        
        $record = <<<EOF
Paul Hudson\t35
EOF;

        $msgs = $this->serder->deserialize($record);

        $this->assertEquals('Paul Hudson', $msgs[0][0]);
        $this->assertEquals('35', $msgs[0][1]);

    }

    public function testValidCsvManyRows()
    {

        
        $record = <<<EOF
"Sex", "Weight (Sep)", "Weight (Apr)", "BMI (Sep)", "BMI (Apr)"
"M", 72, 59, 22.02, 18.14
"M", 97, 86, 19.70, 17.44
"M", 74, 69, 24.09, 22.43
"M", 93, 88, 26.97, 25.57
"F", 68, 64, 21.51, 20.10
"M", 59, 55, 18.69, 17.40
"F", 64, 60, 24.24, 22.88
"F", 56, 53, 21.23, 20.23
"F", 70, 68, 30.26, 29.24
"F", 58, 56, 21.88, 21.02
EOF;

        $msgs = $this->serder->deserialize($record);

        $this->assertEquals('M', $msgs[0]['Sex']);
        $this->assertEquals('72', $msgs[0]['Weight (Sep)']);

    }

    public function testValidCstDelimiterSpace()
    {
        

        $record = <<<EOF
"Sex", "Weight (Sep)", "Weight (Apr)", "BMI (Sep)", "BMI (Apr)"
"M", 72, 59, 22.02, 18.14
EOF;

        $msgs = $this->serder->deserialize($record);

        $this->assertEquals('M', $msgs[0]['Sex']);
        $this->assertEquals('72', $msgs[0]['Weight (Sep)']);

    }

}


