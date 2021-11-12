<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\ValidationResponse;
use Skipprd\Traits\Config;

class DataOutputPluginBase implements DataOutputPluginInterface
{
    protected $tenantId = '';

    protected $pipelineName = '';

    public $flushBytes = 10000000;

    public $buffer = null;

    public function __construct(array $config, BufferInterface $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $this->buffer = $buffer;
        $this->buffer->flushBytes = $this->flushBytes;
        $this->config = $config;
        
        if (in_array(Config::$outputFormat, Config::$batchFormats)) {
            $this->buffer->flushMemBytes = $this->flushBytes;
        }

        if (!empty(Config::$flushBytes)) {
            $this->buffer->flushMemBytes = Config::$flushBytes;
        }
    }

    public function sync(string $format = '')
    {
    }

    public function doValidateConnection(): ValidationResponse
    {
        $validationResp = new ValidationResponse('You must implement doValidateConnection()');

        return $validationResp;
    }

    public function doValidateConfig(): ValidationResponse
    {
        $validationResp = new ValidationResponse('You must implement doValidateConfig()');

        return $validationResp;
    }

    public function shutdown()
    {
    }
}
