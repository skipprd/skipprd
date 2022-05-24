<?php

use Skipprd\Traits\Config;

require_once __DIR__ . '/../vendor/autoload.php';

Config::getConfig();

//if (Config::$runMode == Config::RUN_MODE_VALIDATE_SCHEMA) {
//    $command = new \Skipprd\Commands\ValidateSchemaFile();
//} else {
    $command = new \Skipprd\Commands\PipelineCommand();

    function skippr_emit(
        string $payload,
        string $offset,
        string $namespace,
        string $partition = '0'
    ): void {

        global $command;

        $command->emit($payload, $offset, $namespace, $partition);
    }
//}


$command->handle();
