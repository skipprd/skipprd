<?php

namespace Skipprd\Plugins\DataSources;

use Skipprd\Buffers\FileBuffer;

class DataSourcePluginBase implements DataSourcePluginInterface
{

    protected $tenantId = '';

    protected $pipelineName = '';

    public function __construct(array $config, FileBuffer $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        
    }

    public function connect() { }

    public function commit(string $offset) {}

    public function sync($pipelineJob) {}

    public function doValidateConnection(array $config) {}

    public function doValidateConfig(array $config) {}

    public function doSave(array $config) {}

    public function resetSourceOffsets() {}

    public function shutdown() {}

    public function deletePlugin() {}

}