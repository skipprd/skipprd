<?php

namespace Skipprd\Plugins;

use \Skipprd\Plugins\DataOutputs\DataOutputPluginBase;
use \Skipprd\Plugins\DataSources\DataSourcePluginBase;
use Skipprd\BufferAdaptors\Buffer;
use Skipprd\BufferAdaptors\FileBuffer;
use Illuminate\Support\Str;

class PluginFactory
{

    /**
     * @param String $type
     * @param String $name
     * @return DataSourcePluginBase|DataOutputPluginBase
     */
    static function factory(string $type, string $name, array $config = [], Buffer $buffer = null) {

        $type = Str::studly($type);
        $name = Str::studly($name);

        $buffer = ($buffer == null) ? new FileBuffer('temp', null) : $buffer;

        $factoryClass = "Skipprd\\Plugins\\$type" . "s\\$type" . "$name" . "\\$type" . "$name" . "Plugin";

        $factoryModel = new $factoryClass($config, $buffer);

        return $factoryModel;
    }

}