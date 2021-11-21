<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\ValidationResponse;

interface DataOutputPluginInterface
{

    public function __construct(array $config, BufferInterface $buffer);

    public function connect() : void;

    public function sync();

    public function doValidateConnection(): ValidationResponse;

    public function doValidateConfig(): ValidationResponse;

    public function shutdown();
}
