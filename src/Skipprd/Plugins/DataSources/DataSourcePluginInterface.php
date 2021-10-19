<?php

namespace Skipprd\Plugins\DataSources;

use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\ValidationResponse;

Interface DataSourcePluginInterface
{

    public function __construct(array $config, BufferInterface $buffer);

    public function connect();

    public function commit(string $offset = '');
    
    public function sync($pipelineJob);

    public function doValidateConnection(): ValidationResponse;

    public function doValidateConfig(): ValidationResponse;

    public function shutdown();

}
