<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\FileBuffer;

Interface DataOutputPluginInterface
{

    public function __construct(array $config, FileBuffer $buffer);

    public function sync(string $format = '', \AvroSchema $schema = null);

    public function shutdown();

}