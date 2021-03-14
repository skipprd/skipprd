<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\FileBuffer;
use Skipprd\Traits\Config;

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

        if (Config::$outputFormat == 'parquet') {
            $this->buffer->flushMemBytes = $this->flushBytes;
        }
    }

    public function sync(string $format = '', \AvroSchema $schema = null) {}

    public function shutdown() {}
}