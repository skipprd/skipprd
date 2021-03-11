<?php

namespace Skipprd\Plugins;

use Skipprd\DataOutputKafka\DataOutputKafkaPlugin;
use Skipprd\Plugins\DataOutputs\DataOutputPluginBase;
use Skipprd\Plugins\DataSources\DataSourcePluginBase;
use Skipprd\Buffers\BufferInterface;
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

        $type = Str::studly(ucwords(strtolower($type)));
        $name = Str::studly(ucwords(strtolower($name)));

        $buffer = ($buffer == null) ? new FileBuffer('temp', null) : $buffer;
        
        $factoryClass = "Skipprd\\$type" . "$name" . "\\$type" . "$name" . "Plugin";
//        $factoryClass = "SkipprdPlugins\\$type" . "s\\$type" . "$name" . "\\$type" . "$name" . "Plugin";
//        $factoryClass = "$type" . "$name" . "Plugin";
//        $factoryClass = "Skipprd\DataSources\DataSourceS3Demo\DataSourceS3DemoPlugin";
//                           Skipprd\DataSources\DataSourceS3Demo\DataSourceS3DemoPlugin

        $factoryModel = new $factoryClass($config, $buffer);

//        $factoryModel = new DataOutputKafkaPlugin($config, $buffer);

        return $factoryModel;
    }

}