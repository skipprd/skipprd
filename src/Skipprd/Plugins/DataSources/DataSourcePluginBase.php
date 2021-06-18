<?php

namespace Skipprd\Plugins\DataSources;

use Skipprd\Buffers\FileBuffer;

class DataSourcePluginBase implements DataSourcePluginInterface
{

    protected $tenantId = '';

    protected $pipelineName = '';

    public $flushBytes = 10000000;
    
    public $buffer = null;

    public $offsets;

    protected $config = [];

    public function __construct(array $config, FileBuffer $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $this->offsets = new Offsets();
        $this->buffer = $buffer;
        $this->buffer->flushBytes = $this->flushBytes;
        $this->config = $config;
    }


    public function splitPartitions(string $partitionField = '') : array
    {
        return explode($partitionField,  ',');
    }

    public function connect() { }

    public function commit(string $offset = '')
    {
        $this->offsets->setOffsets($offset);
    }
    
    public function sync($pipelineJob) {}

    public function doValidateConnection() {}

    public function doValidateConfig() {}

    public function shutdown() {}

}