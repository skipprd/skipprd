<?php

namespace Skipprd\Plugins\DataSources;

use Skipprd\Buffers\FileBuffer;

Interface DataSourcePluginInterface
{

    public function __construct(array $config, FileBuffer $buffer);

    public function connect();

    public function commit(string $offset);

    public function sync($pipelineJob);

    public function doValidateConnection();

    public function doValidateConfig();

    public function shutdown();

}