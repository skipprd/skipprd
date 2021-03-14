<?php

require_once __DIR__ . '/../vendor/autoload.php';

$pipeline = new \Skipprd\Commands\PipelineCommand();

$pipeline->handle();
