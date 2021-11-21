<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\ValidationResponse;
use Skipprd\Traits\Config;

class DataOutputPluginBase implements DataOutputPluginInterface
{
    protected $tenantId = '';

    protected $pipelineName = '';

    public $buffer = null;

    public function __construct(array $config, BufferInterface $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $this->buffer = $buffer;
        $this->config = $config;
    }

    public function connect() : void
    {
    }

    public function sync()
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
