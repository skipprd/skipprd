<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\FileBuffer;

class DataOutputPluginBase implements DataOutputPluginInterface
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
        $this->buffer->flushMemBytes = $this->flushBytes;
    }

    public function doValidateConnection(array $config) {}

    public function doValidateConfig(array $config) {}

    public function doSave(array $config) {}

    public function createOrUpdateSchema(array $schema) {}

    public function sync(string $format = '', \AvroSchema $schema = null) {}

    public function execQuery(array $config) {}

    public function deleteSchema() {}

    public function deletePlugin() {}

    public function shutdown() {}
}