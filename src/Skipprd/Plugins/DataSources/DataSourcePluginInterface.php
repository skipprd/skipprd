<?php

namespace Skipprd\Plugins\DataSources;

use Skipprd\Buffers\ChunkedBuffer;

Interface DataSourcePluginInterface
{

    public function __construct(array $config, ChunkedBuffer $buffer);

    public function connect();

    public function commit(string $offset = '');
    
    public function sync($pipelineJob);

    public function doValidateConnection();

    public function doValidateConfig();

    public function shutdown();

}