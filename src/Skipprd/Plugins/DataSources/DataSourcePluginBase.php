<?php

namespace Skipprd\Plugins\DataSources;

use Skipprd\Buffers\FileBuffer;

class DataSourcePluginBase implements DataSourcePluginInterface
{

    protected $tenantId = '';

    protected $pipelineName = '';

    public $flushBytes = 10000000;
    
    public $buffer = null;

    public function __construct(array $config, FileBuffer $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $this->buffer = $buffer;
        $this->buffer->flushBytes = $this->flushBytes;
    }

    public function connect() { }

    public function commit(string $offset) {}

    public function sync($pipelineJob) {}

    public function shutdown() {}

}