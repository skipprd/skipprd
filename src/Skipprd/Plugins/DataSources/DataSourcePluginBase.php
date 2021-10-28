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

    public $flushBytes = 10000000;
    
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
        $this->buffer->flushBytes = $this->flushBytes;
        $this->config = $config;
    }


    public function splitPartitions(string $partitionField = '') : array
    {
        return explode(',', $partitionField);
    }

    public function connect()
    { 
    }

    public function commit(string $offset = '')
    {
        $this->offsets->setOffsets($offset);
    }
    
    public function sync($pipelineJob)
    {
    }

    public function ingestPartition($partition)
    {

        if (!isset($this->continue[$partition])) {
            $this->continue[$partition] = true;
        }

        //        SkipprLogger::info(var_dump($this->continue));
        
        return $this->continue[$partition];
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
