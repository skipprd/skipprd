<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\BufferAdaptors\Buffer;

class DataOutputPluginBase implements DataOutputPluginInterface
{
    protected $tenantId = '';

    protected $pipelineName = '';

    public $flushBytes = 10000000;

    public function __construct(array $config, Buffer $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $this->buffer = $buffer;
    }

    public function doValidateConnection(array $config) {}

    public function doValidateConfig(array $config) {}

    public function doSave(array $config) {}

    public function createOrUpdateSchema(array $schema) {}

    public function sync(string $serde = '', \AvroSchema $schema = null) {}

    public function execQuery(array $config) {}

    public function deleteSchema() {}

    public function deletePlugin() {}

    public function shutdown() {}
}