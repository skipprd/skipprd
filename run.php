<?php

require_once 'vendor/autoload.php';

$pipeline = new \Skipprd\Commands\PipelineCommand();

$pipeline->handle();
