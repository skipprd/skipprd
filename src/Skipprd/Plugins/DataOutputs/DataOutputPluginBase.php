<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\ChunkedBuffer;
use Skipprd\Traits\Config;

class DataOutputPluginBase implements DataOutputPluginInterface
{
    protected $tenantId = '';

    protected $pipelineName = '';

    public $flushBytes = 10000000;

    public $buffer = null;

    public function __construct(array $config, ChunkedBuffer $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $this->buffer = $buffer;
        $this->buffer->flushBytes = $this->flushBytes;
        $this->config = $config;
        
        if (in_array(Config::$outputFormat, Config::$batchFormats)) {
            $this->buffer->flushMemBytes = $this->flushBytes;
        }
    }

    public function sync(string $format = '') {}

    public function shutdown() {}
}