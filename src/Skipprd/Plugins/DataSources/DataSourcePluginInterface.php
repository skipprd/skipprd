<?php

namespace Skipprd\Plugins\DataSources;

use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\ValidationResponse;

interface DataSourcePluginInterface
{

    public function __construct(array $config, BufferInterface $buffer);

    public function connect(): void;

    public function sync();

    public function doValidateConnection(): ValidationResponse;

    public function doValidateConfig(): ValidationResponse;

    public function shutdown();

    public function doSave();

    public function deletePlugin();

}
