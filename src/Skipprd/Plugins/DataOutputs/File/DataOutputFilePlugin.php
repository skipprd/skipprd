<?php
namespace Skipprd\Plugins\DataOutputs\File;

use Skipprd\Helpers;
use Skipprd\Plugins\DataOutputs\DataOutputPluginBase;
use Skipprd\Buffers\BufferInterface;
use Monolog\Registry;
use Skipprd\Plugins\ValidationResponse;
use Skipprd\Traits\SkipprLogger;

class DataOutputFilePlugin extends DataOutputPluginBase
{

    protected $config = [];

    public function __construct(array $config, BufferInterface $buffer)
    {

        parent::__construct($config, $buffer);
    }

    public function connect(): void
    {
    }
    
    public function sync(string $serde = 'json')
    {

        while ($filename = $this->buffer->driver->nextFile()) {
            $path = $this->config['path'];

            $namespace = $this->buffer->decodeFileNamespace($filename);
            $partition = $this->buffer->decodeFilePartition($filename);
            $timePartition = $this->buffer->decodeChunkTime($filename);
            
            $path = (!empty($timePartition)) ? $path . '/' . $timePartition : $path;
            $path = (!empty($namespace)) ? $path . '/' . $namespace : $path;
            $path = (!empty($partition)) ? $path . '/' . $partition : $path;

            @mkdir($path, 0755, true);

            $path = $path . '/' . Helpers::randomPassword(32);

            $result = rename($filename, $path);

            if ($result) {
                SkipprLogger::info("Saved buffer file $filename to output $path.");

                @$this->buffer->driver->destroy($filename);
            } else {
                SkipprLogger::error("Could not save buffer file $filename to output $path.");
            }
        }
    }

    public function doValidateConnection(): ValidationResponse
    {

        $validationResp = new ValidationResponse('Config Succeeded');

        try {
            $path = $this->config['path'];

            mkdir($path);
        } catch (\Exception $e) {
            $validationResp->title = 'Config Failed';
            $validationResp->error = $e->getMessage();
            SkipprLogger::error("Could not configure output.");
            SkipprLogger::error($validationResp->error);
        }

        return $validationResp;
    }

    public function doValidateConfig(): ValidationResponse
    {

        $validationResp = new ValidationResponse('File output directory created');

        try {
            $path = $this->config['path'];

            mkdir($path);
        } catch (\Exception $e) {
            $result = false;
            $validationResp->title = 'Failed to create File output directory';
            $validationResp->error = $e->getMessage();
        }

        return $validationResp;
    }

    public function shutdown()
    {
    }
}
