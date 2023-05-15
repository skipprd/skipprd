<?php

namespace legacy\src\Skipprd\Plugins\DataOutputs;

use legacy\src\Skipprd\Buffers\BufferInterface;
use legacy\src\Skipprd\Plugins\ValidationResponse;

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

    public function deleteSchema(string $namespace);

    public function deletePlugin();

    public function resetSourceOffsets(): void;
}
