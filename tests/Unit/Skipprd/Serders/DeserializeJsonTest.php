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

class DeserializeJsonTest extends TestCase
{

    protected $serder = 'json';

    protected function setUp()
    {
        parent::setUp();

    }

    // @todo - check parseRecordJson() only called once when parsing subsequent
    // json records. Parser should set $serder

    public function testBasicValidJson()
    {

        $record = '{"status": "200"}';

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);

    }

    public function testNestedValidJson()
    {

        $record = '{"status": "200", "items": {"foo": "bar"}}';

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);
        $this->assertEquals('bar', $msg[0]['items']['foo']);

    }

    public function testNestedArrayValidJson()
    {

        $record = '{"status": "200", "items": [{"foo": "bar"}]}';

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);
        $this->assertEquals('bar', $msg[0]['items'][0]['foo']);

    }

    public function testEscapedValidJson()
    {

        $record = '{\"status\": \"200\"}';

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);

    }

    public function testDoubleEscapedValidJson()
    {

        $record = '{\\\"time\\\":{\\\"start_time\\\":\\\"273.046328210292\\\",\\\"end_time\\\":\\\"16182\\\"},\\\"bike_id\\\":\\\"0.579087190592872\\\",\\\"location\\\":{\\\"start\\\":\\\"0.620131100002421\\\",\\\"end\\\":null}}';

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('0.579087190592872', $msg[0]['bike_id']);
        $this->assertEquals('273.046328210292', $msg[0]['time']['start_time']);

    }

    public function testNullValueValidJson()
    {

        $record = '{"start":"0.620131100002421","end":null}';

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('0.620131100002421', $msg[0]['start']);
        $this->assertEquals(null, $msg[0]['end']);

    }

    public function testSingleQuotesValidJson()
    {

        $record = "{'status': '200'}";

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);

    }

    public function testStringBeforeEscapedJson()
    {

        $record = 'some, string, that exists)/ 20080808115538 {\"status\":\"200\",\"length\":\"4742\",\"mime\":\"text/html\",\"offset\":\"16518203\"}';

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);
        $this->assertNotContains('some, string', $msg[0]);

    }


    public function testUnicodeValidJson()
    {

        $record = "{u'status': u'200'}";

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);

        $this->assertEquals('200', $msg[0]['status']);

    }

    public function testUnicodeValueValidJson()
    {

        $record = '{"status": "\u0023"}';

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('#', $msg[0]['status']);
        $this->assertNotContains('2605', $msg[0]['status']);

    }

    public function testValidJsonMultiRecordArray()
    {

        $record = <<<EOF
[{"status": "200"},{"status": "500"}]
EOF;

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);
        $this->assertEquals('500', $msg[1]['status']);

    }

    public function testValidJsonMultiLine()
    {

        $record = <<<EOF
{"status": "200"}\n{"status": "500"}
EOF;

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);
        $this->assertEquals('500', $msg[1]['status']);

    }

    public function testValidJsonMultiLineWithMultiRecordArrays()
    {

        $record = <<<EOF
[{"status": "200"},{"status": "201"}]\n[{"status": "202"},{"status": "203"}]
EOF;

        $serder = SerdersFactory::factory($this->serder);
        $msg = $serder->deserialize($record);


        $this->assertEquals('200', $msg[0]['status']);
        $this->assertEquals('201', $msg[1]['status']);
        $this->assertEquals('202', $msg[2]['status']);
        $this->assertEquals('203', $msg[3]['status']);

    }

//    public function testValidJsonMultiLineArrayPrettyPrint()
//    {
//
//
//        $record = <<<EOF
//[
//    {"status": "200"},
//    {"status": "500"},
//    {"status": "200"},
//    {"status": "200"},
//    {"status": "200"}
//]
//EOF;
//
//        $container = Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//
//        $msgs = [];
//
//        $stream = fopen("php://temp", 'w+');
//        fputs($stream, $record);
//        rewind($stream);
//
////        while ( ($payload = fgets($stream) ) !== false ) {
////            $msgs = Serders::factory($record, $serder);
////        }
//
//        $msgs = Serders::factory($record, $serder);
//
//        $this->assertEquals('200', $msgs[0]['status']);
//        $this->assertEquals('500', $msgs[1]['status']);
//
//    }
//
//    public function testValidJsonMultiLineArrayofRecordsPrettyPrint()
//    {
//
//
//        $record = <<<EOF
//[
//  {
//    "customer": {
//      "address": "509 Kings Hwy, Comptche, Missouri, 4848",
//      "phone": "+1 (999) 407-2274",
//      "email": "blankenship.patrick@orbin.ca",
//      "company": "ORBIN",
//      "name": {
//        "last": "Patrick",
//        "first": "Blankenship"
//      },
//      "_id": "5730864df388f1d653e37e6f"
//    }
//  },
//  {
//    "customer": {
//      "address": "290 Lefferts Avenue, Malott, Delaware, 1575",
//      "phone": "+1 (958) 411-2876",
//      "email": "anna.glass@snips.name",
//      "company": "SNIPS",
//      "name": {
//        "last": "Glass",
//        "first": "Anna"
//      },
//      "_id": "5730864d4d8523c8baa8baf6"
//    }
//  }
//]
//EOF;
//
//        $container = Mockery::mock(PipelineCommand::class)->makePartial();
//        $container->shouldReceive('AnalyseSchema');
//
//        $msgs = [];
//
//        $stream = fopen("php://temp", 'w+');
//        fputs($stream, $record);
//        rewind($stream);
//
////        while ( ($payload = fgets($stream) ) !== false ) {
////            $msgs = Serders::factory($record, $serder);
////        }
//
//        $msgs = Serders::factory($record, $serder);
//
//        $this->assertEquals('509 Kings Hwy, Comptche, Missouri, 4848', $msgs[0]['customer']['address']);
//        $this->assertEquals('Glass', $msgs[1]['customer']['name']['last']);
//
//    }

}