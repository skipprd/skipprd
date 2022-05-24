<?php

namespace Skipprd\Plugins\OffsetDrivers;

use Skipprd\Str;
use Skipprd\Traits\SkipprLogger;

class OffsetDriverFactory
{

    /**
     * @param  String $type
     * @return SkipprSaasOffsetDriver|SkipprFileOffsetDriver
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
