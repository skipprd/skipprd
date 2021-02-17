<?php

namespace Skipprd\Plugins\DataSources;

use Skipprd\BufferAdaptors\Buffer;

Interface DataSourcePluginInterface
{

    public function __construct(array $config, Buffer $buffer);

    public function connect();

    public function commit(string $offset);

    public function sync($pipelineJob);

    public function doValidateConnection(array $config);

    public function doValidateConfig(array $config);

    public function doSave(array $config);

    public function resetSourceOffsets();

    public function shutdown();

    public function deletePlugin();


}