<?php

namespace legacy\src\Skipprd\Plugins\OffsetDrivers;

use legacy\src\Skipprd\SkipprLogger;
use legacy\src\Skipprd\Str;
use Skipprd\Plugins\OffsetDrivers\SkipprSqlliteOffsetDriver;

class OffsetDriverFactory
{

    /**
     * @param  String $type
     * @return SkipprSaasOffsetDriver|SkipprSqlliteOffsetDriver
     */
    static function factory(string $type)
    {

        SkipprLogger::info("Loading $type offset driver");

        $type = Str::studly(ucwords(strtolower($type)));

        $factoryClass = "\Skipprd\\Plugins\\OffsetDrivers\\$type" . "OffsetDriver";

        $factoryModel = new $factoryClass();

        return $factoryModel;
    }
}
