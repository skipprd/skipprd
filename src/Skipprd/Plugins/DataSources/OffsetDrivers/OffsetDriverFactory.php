<?php

namespace Skipprd\Plugins\DataSources\OffsetDrivers;

use Monolog\Registry;
use Skipprd\Str;
use Skipprd\Traits\SkipprLogger;


class OffsetDriverFactory
{

    /**
     * @param  String $type
     * @return SkipprInternalOffsetDriver|SkipprFileOffsetDriver
     */
    static function factory(string $type)
    {

        SkipprLogger::info("Loading $type offset driver");

        $type = Str::studly(ucwords(strtolower($type)));

        $factoryClass = "\Skipprd\\Plugins\\DataSources\\OffsetDrivers\\$type" . "OffsetDriver";

        $factoryModel = new $factoryClass();

        return $factoryModel;
    }

}
