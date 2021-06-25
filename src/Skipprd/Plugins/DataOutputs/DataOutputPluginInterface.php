<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\ChunkedBuffer;

Interface DataOutputPluginInterface
{

    public function __construct(array $config, ChunkedBuffer $buffer);

    public function sync(string $format = '');

    public function shutdown();

}