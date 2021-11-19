<?php

namespace Skipprd\Plugins\DataSources;

use Monolog\Registry;
use Skipprd\Buffers\BufferInterface;
use Skipprd\Plugins\ValidationResponse;
use Skipprd\Traits\SkipprLogger;

class DataSourcePluginBase implements DataSourcePluginInterface
{

    protected $tenantId = '';

    protected $pipelineName = '';

    public $continue = [];

    public $buffer = null;

    /**
     * @var \Skipprd\Plugins\DataSources\Offsets
     */
    public $offsets;

    protected $config = [];

    public function __construct(array $config, BufferInterface $buffer)
    {
        $this->tenantId = getenv('TENANT_ID');
        $this->pipelineName = getenv('PIPELINE_NAME');
        $this->offsets = new Offsets();
        $this->buffer = $buffer;
        $this->config = $config;
    }

    /**
     * Input connectors can specify multiple namespaces to ingest (tables, topics, streams, file paths, etc)
     * @param string $namespaces - comma separated list of namespaces
     * @return array
     */
    public function splitNamespaces(string $namespaces = ''): array
    {
        return explode(',', $namespaces);
    }

    public function connect(): void
    {
    }

    public function commit(string $offset = '')
    {
        $this->offsets->setOffsets($offset);
    }

    public function sync($pipelineJob)
    {
    }

    /**
     * Hack used when discovering schema. true on an array key indicates that namespace
     * has finished discovering and should consume no more data.
     * @param $namespace
     * @return mixed
     * @todo - need a better way (threading per namespace/partition? multiple container workers?)
     */
    public function ingestNamespace($namespace)
    {

        if (!isset($this->continue[$namespace])) {
            $this->continue[$namespace] = true;
        }

        return $this->continue[$namespace];
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
