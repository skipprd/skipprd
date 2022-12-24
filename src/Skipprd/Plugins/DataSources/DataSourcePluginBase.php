<?php

namespace Skipprd\Plugins\DataSources;

use Monolog\Registry;
use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\OffsetDrivers\OffsetDriverFactory;
use Skipprd\Plugins\Offsets;
use Skipprd\Plugins\ValidationResponse;
use Skipprd\Traits\Config;
use Skipprd\SkipprLogger;

class DataSourcePluginBase implements DataSourcePluginInterface
{

    protected $tenantId = '';

    protected $pipelineName = '';

    public $continue = [];

    public $buffer = null;

    /**
     * @var \Skipprd\Plugins\Offsets
     */
    public $offsets;

    protected $config = [];

    public function __construct(array $config, BufferInterface $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $this->buffer = $buffer;
        $type = Config::getenv('OFFSET_DRIVER', 'skippr_file');
        $offsetClient = OffsetDriverFactory::factory($type);
        $this->offsets = new Offsets($offsetClient);
        $this->config = $config;
    }

    /**
     * Input connectors can specify multiple namespaces to ingest (tables, topics, streams, file paths, etc)
     * @param string $namespaces - comma separated list of namespaces
     * @return array
     * @todo - relocate this to Skippr SaaS, workers should be single process
     * able to operate on one namespace + partition pair for scaling
     */
    public function splitNamespaces(string $namespaces = ''): array
    {
        return explode(',', $namespaces);
    }

    public function connect(): void
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


    public function deletePlugin()
    {
    }

    public function resetSourceOffsets(): void
    {
    }
}
