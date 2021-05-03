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
}
