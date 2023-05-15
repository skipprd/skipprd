<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Commands;


use legacy\src\Skipprd\InternalFields;
use legacy\src\Skipprd\Traits\Config;
use Tests\TestCase;

class ParseInternalFieldTest extends TestCase
{

    protected array $event;

    protected function setUp(): void
    {
        parent::setUp();

        $this->event = [
            [
                'event_type' => 'foo1',
                'address' => '509 Kings Hwy, Comptche, Missouri, 4848',
                'phone' => '+1 (999) 407-2274',
                'email' => 'blankenship.patrick@orbin.ca',
                'company' => 'ORBIN',
                'name' => [
                    'last' => 'Patrick',
                    'first' => 'Blankenship',
                ],
                '_id' => 'abc1',
            ],
            [
                'event_type' => 'foo2',
                'start' => 'london',
                'end' => 'NYC',
                'price' => 1000,
                'airline' => [
                    'code' => 'BA',
                    'call_sign' => 'speedbird',
                ],
                '_id' => 'abc2',
            ],
        ];

    }


    /**
     * Namespace
     */

    public function testParseNamespaceField()
    {

        Config::$eventTypeFields = ['event_type'];

        $i = 1;

        foreach ($this->event as $event) {

            $namespace = 'data_source_name';

            $namespace = InternalFields::parseNamespaceField($event, $namespace);

            $this->assertEquals("foo$i", $event['skpr_namespace']);

            $this->assertEquals("foo$i", $namespace);

            $i++;
        }
    }

    public function testParseNamespaceFields()
    {

        Config::$eventTypeFields = ['event_type', '_id'];

        $i = 1;

        foreach ($this->event as $event) {

            $namespace = 'data_source_name';

            $namespace = InternalFields::parseNamespaceField($event, $namespace);

            $this->assertEquals("foo$i" . '_' . "abc$i", $event['skpr_namespace']);

            $this->assertEquals("foo$i" . '_' . "abc$i", $namespace);

            $i++;
        }
    }

    public function testParseNamespaceFieldNoSuffix()
    {

        Config::$eventTypeFields = null;
        
        $i = 1;

        foreach ($this->event as $event) {

            $namespace = 'data_source_name';

            $namespace = InternalFields::parseNamespaceField($event, $namespace);

            $this->assertEquals("data_source_name", $event['skpr_namespace']);

            $this->assertEquals("data_source_name", $namespace);

            $i++;
        }
    }

    public function testParseSourceNamespace()
    {

        $namespace = "data_source_name-=foo1_id=abc1";

        $sourceNamespace = InternalFields::parseSourceNamespace($namespace);

        $this->assertEquals('data_source_name', $sourceNamespace);

        $this->assertEquals( "data_source_name-=foo1_id=abc1", $namespace);

    }

    public function testParseSourceNamespaceNoSuffix()
    {

        $namespace = "data_source_name";

        $sourceNamespace = InternalFields::parseSourceNamespace($namespace);

        $this->assertEquals('data_source_name', $sourceNamespace);

        $this->assertEquals( "data_source_name", $namespace);

    }

    /**
     * Partition
     */

    public function testParsePartitionField()
    {

        Config::$partitionByFields = ['event_type'];

        $i = 1;

        foreach ($this->event as $event) {

            $partition = 'data_source_name';

            $partition = InternalFields::parsePartitionField($event, $partition);

            $this->assertEquals("event_type=foo$i", $event['skpr_partition']);

            $this->assertEquals("event_type=foo$i", $partition);

            $i++;
        }
    }

    public function testParsePartitionFields()
    {

        Config::$partitionByFields = ['event_type', '_id'];

        $i = 1;

        foreach ($this->event as $event) {

            $partition = 'data_source_name';

            $partition = InternalFields::parsePartitionField($event, $partition);

            $this->assertEquals("event_type=foo$i-id=abc$i", $event['skpr_partition']);

            $this->assertEquals("event_type=foo$i-id=abc$i", $partition);

            $i++;
        }
    }

    public function testParseSourcePartition()
    {

        $partition = "data_source_name-event_type=foo-id=abc";

        $sourcePartitionName = InternalFields::parseSourcePartition($partition);

        $this->assertEquals('data_source_name', $sourcePartitionName);

        $this->assertEquals("data_source_name-event_type=foo-id=abc", $partition);

    }

    public function testParseSourcePartitionNoSuffix()
    {

        $partition = "data_source_name";

        $sourcePartitionName = InternalFields::parseSourceNamespace($partition);

        $this->assertEquals('data_source_name', $sourcePartitionName);

        $this->assertEquals( "data_source_name", $partition);

    }

}