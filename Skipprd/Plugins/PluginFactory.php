<?php

namespace Skipprd\Plugins;

use Skipprd\Plugins\DataOutputs\DataOutputPluginBase;
use Skipprd\Plugins\DataSources\DataSourcePluginBase;
use Skipprd\Buffers\BufferInterface;
use Skipprd\Buffers\FileBuffer;
use Skipprd\Str;
use Skipprd\DataSources\DataSourceS3Demo\DataSourceS3DemoPlugin;


class PluginFactory
{

    /**
     * @param String $type
     * @param String $name
     * @return DataSourcePluginBase|DataOutputPluginBase
     */
    static function factory(string $type, string $name, array $config = [], FileBuffer $buffer = null) {

        $type = Str::studly(ucwords(strtolower($type)));
        $name = Str::studly(ucwords(strtolower($name)));

        $buffer = ($buffer == null) ? new FileBuffer('temp', null) : $buffer;

        $factoryClass = "Skipprd\\$type" . "s\\$type" . "$name" . "\\$type" . "$name" . "Plugin";
//        $factoryClass = "$type" . "$name" . "Plugin";
//        $factoryClass = "Skipprd\DataSources\DataSourceS3Demo\DataSourceS3DemoPlugin";
//                           Skipprd\DataSources\DataSourceS3Demo\DataSourceS3DemoPlugin


        $factoryModel = new $factoryClass($config, $buffer);

        return $factoryModel;
    }

}