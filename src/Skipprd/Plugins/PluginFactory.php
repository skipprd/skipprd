<?php

namespace Skipprd\Plugins;

use Monolog\Registry;
use Skipprd\Plugins\DataOutputs\DataOutputPluginBase;
use Skipprd\Plugins\DataSources\DataSourcePluginBase;
use Skipprd\Buffers\FileBuffer;
use Skipprd\Str;


class PluginFactory
{

    /**
     * @param String $type
     * @param String $name
     * @return DataSourcePluginBase|DataOutputPluginBase
     */
    static function factory(string $type, string $name, FileBuffer $buffer = null) {

        $config = [];
        $envs = getenv();

        foreach ($envs as $key => $value) {
            if (strpos($key, strtoupper($type)) > -1) {
                $config[strtolower(substr($key, strlen(strtoupper($type) . '_')))] = $value;
            }
        }

        Registry::skipprd()->info("Loading $type plugin $name");

        $type = Str::studly(ucwords(strtolower($type)));
        $name = Str::studly(ucwords(strtolower($name)));

        $buffer = ($buffer == null) ? new FileBuffer('temp', null) : $buffer;

        $factoryClass = "\Skipprd\\$type" . "$name" . "\\$type" . "$name" . "Plugin";

        $factoryModel = new $factoryClass($config, $buffer);

        return $factoryModel;
    }

}