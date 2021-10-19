<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\ValidationResponse;

Interface DataOutputPluginInterface
{

    public function __construct(array $config, BufferInterface $buffer);

    public function sync(string $format = '');

    public function doValidateConnection(): ValidationResponse;

    public function doValidateConfig(): ValidationResponse;

    public function shutdown();

}
