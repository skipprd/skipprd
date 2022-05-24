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

    public function doSave();

    public function createOrUpdateSchema(string $partition, array $schema);

    public function deleteSchema();

    public function deletePlugin();

    public function resetSourceOffsets(): void;
}
