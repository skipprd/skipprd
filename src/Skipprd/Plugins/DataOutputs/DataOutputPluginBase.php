<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\OffsetDrivers\OffsetDriverFactory;
use Skipprd\Plugins\Offsets;
use Skipprd\Plugins\ValidationResponse;
use Skipprd\Traits\Config;

class DataOutputPluginBase implements DataOutputPluginInterface
{
    protected $tenantId = '';

    protected $pipelineName = '';

    /**
     * @var \Skipprd\Buffers\ChunkedBuffer
     */
    public $buffer;

    /**
     * @var \Skipprd\Plugins\Offsets
     */
    public $offsets;

    public function __construct(array $config, BufferInterface $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $type = Config::getenv('OFFSET_DRIVER', 'skippr_file');
        $offsetClient = OffsetDriverFactory::factory($type);
        $this->offsets = new Offsets($offsetClient);
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

    public function doSave()
    {
    }

    public function createOrUpdateSchema(string $partition, array $schema)
    {
    }

    public function deleteSchema(string $namespace)
    {
    }

    public function deletePlugin()
    {
    }

    public function resetSourceOffsets(): void
    {
    }
}
