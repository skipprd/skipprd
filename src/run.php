<?php

require_once __DIR__ . '/../vendor/autoload.php';

$pipeline = new \Skipprd\Commands\PipelineCommand();

function skippr_emit(
    string $payload,
    string $offset,
    string $namespace,
    string $partition = '0'
): void {

    global $pipeline;

    $pipeline->emit($payload, $offset, $namespace, $partition);
}

$pipeline->handle();
