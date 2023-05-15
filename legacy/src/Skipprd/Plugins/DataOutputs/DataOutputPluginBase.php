<?php

namespace legacy\src\Skipprd\Plugins\DataOutputs;

use legacy\src\Skipprd\Buffers\BufferInterface;
use legacy\src\Skipprd\Plugins\OffsetDrivers\OffsetDriverFactory;
use legacy\src\Skipprd\Plugins\Offsets;
use legacy\src\Skipprd\Plugins\ValidationResponse;
use legacy\src\Skipprd\Traits\Config;

class DataOutputPluginBase implements DataOutputPluginInterface
{
    protected $tenantId = '';

    protected $pipelineName = '';

    /**
     * @var \legacy\src\Skipprd\Buffers\ChunkedBuffer
     */
    public $buffer;

    /**
     * @var \legacy\src\Skipprd\Plugins\Offsets
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
