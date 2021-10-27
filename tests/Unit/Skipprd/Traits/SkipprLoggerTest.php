<?php

namespace Skipprd\Traits;

use Monolog\Registry;
use PHPUnit\Framework\TestCase;

class SkipprLoggerTest extends TestCase
{

    public function testInit()
    {

        SkipprLogger::init();

        $this->assertTrue(Registry::hasLogger('skipprd'));
    }

    public function testError()
    {

        SkipprLogger::error('foo');
        $this->assertTrue(Registry::hasLogger('skipprd'));

    }

    public function testDebug()
    {
        SkipprLogger::error('foo');
        $this->assertTrue(Registry::hasLogger('skipprd'));

    }

    public function testInfo()
    {
        SkipprLogger::error('foo');
        $this->assertTrue(Registry::hasLogger('skipprd'));
    }

    public function testEmergency()
    {
        SkipprLogger::emergency('foo');
        $this->assertTrue(Registry::hasLogger('skipprd'));
    }

    public function testCritical()
    {
        SkipprLogger::critical('foo');
        $this->assertTrue(Registry::hasLogger('skipprd'));
    }

}
