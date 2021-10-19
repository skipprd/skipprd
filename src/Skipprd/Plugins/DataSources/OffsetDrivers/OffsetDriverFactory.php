<?php

namespace Skipprd\Plugins\DataSources\OffsetDrivers;

use Monolog\Registry;
use Skipprd\Str;


class OffsetDriverFactory
{

    /**
     * @param  String $type
     * @return SkipprInternalOffsetDriver|SkipprFileOffsetDriver
     */
    static function factory(string $type)
    {

        Registry::skipprd()->info("Loading $type offset driver");

        $type = Str::studly(ucwords(strtolower($type)));

        $factoryClass = "\Skipprd\\Plugins\\DataSources\\OffsetDrivers\\$type" . "OffsetDriver";

        $factoryModel = new $factoryClass();

        return $factoryModel;
    }

}
