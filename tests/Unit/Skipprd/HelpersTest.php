<?php

namespace Unit\Skipprd;

use PHPUnit\Framework\TestCase;
use Skipprd\Helpers;

class HelpersTest extends TestCase
{

    public function testIsSequentialArrayKeys()
    {
        $field = [2, 3, 4, 6, 7, 4, 3, 6, 7, 9];
        $isArray = Helpers::isSequentialArrayKeys($field);
        $this->assertTrue($isArray);

        $field = ['a', 'b', 'c'];
        $isArray = Helpers::isSequentialArrayKeys($field);
        $this->assertTrue($isArray);

        $field = ["0" => 'a', "1" => 'b', "2" => 'c'];
        $isArray = Helpers::isSequentialArrayKeys($field);
        $this->assertTrue($isArray);

        $field = ["1" => 'a', "0" => 'b', "2" => 'c'];
        $isArray = Helpers::isSequentialArrayKeys($field);
        $this->assertTrue($isArray);

    }

    public function testFlatten()
    {

        $message = [
            'customer' => [
                'address' => '509 Kings Hwy, Comptche, Missouri, 4848',
                'phone' => '+1 (999) 407-2274',
                'email' => 'blankenship.patrick@orbin.ca',
                'company' => 'ORBIN',
                'name' => [
                    'first' => 'Patrick',
                    'last' => 'Blankenship',
                ],
                '_id' => '5730864df388f1d653e37e6f',
            ],
            'event_time' => 0001,
        ];

        $msg = Helpers::flatten($message);

        $this->assertArrayHasKey('customer_address', $msg);
        $this->assertEquals('509 Kings Hwy, Comptche, Missouri, 4848', $msg['customer_address']);

        $this->assertArrayHasKey('customer_name_first', $msg);
        $this->assertEquals('Patrick', $msg['customer_name_first']);

        $this->assertArrayHasKey('customer__id', $msg);
        $this->assertEquals('5730864df388f1d653e37e6f', $msg['customer__id']);

        $this->assertArrayHasKey('event_time', $msg);
        $this->assertEquals(0001, $msg['event_time']);
    }

    public function testCleanFieldName() {

//        $fields = [
//            'detail-type',
//            'Detail Type',
//            'detail_Type',
//        ];
//
//        foreach ($fields as $field) {
//
//            $cleanField = Helpers::cleanFieldName($field);
//
//            $this->assertEquals('detail_type', $cleanField);
//        }
//
//        $field = 'DetailType';
//        $cleanField = Helpers::cleanFieldName($field);
//        $this->assertEquals('detailtype', $cleanField);
//
//        $field = '123detailtype';
//        $cleanField = Helpers::cleanFieldName($field);
//        $this->assertEquals('detailtype', $cleanField);
//
//        $field = '123detail -type.';
//        $cleanField = Helpers::cleanFieldName($field);
//        $this->assertEquals('detail__type', $cleanField);

        $field = '0';
        $cleanField = Helpers::cleanFieldName($field);
        $this->assertEquals('item_0', $cleanField);

    }
}
