<?php

namespace Skipprd\Plugins\DataSources\File;

use Skipprd\Plugins\DataSources\DataSourcePluginBase;
use Skipprd\Buffers\BufferInterface;
use Monolog\Registry;
use Skipprd\Plugins\ValidationResponse;
use Skipprd\Traits\SkipprLogger;

class DataSourceFilePlugin extends DataSourcePluginBase
{

    protected $config = [];

    public function __construct(array $config, BufferInterface $buffer)
    {

        parent::__construct($config, $buffer);
    }

    public function connect(): void
    {
    }

    public function sync($pipelineJob)
    {

        try {
            $path = $this->config['path'];

            $offsetTimestamp = 0;
            $offsetLine = 0;

            $offset = $this->offsets->getOffsets($path);

            if (isset($offset[0])) {
                $offsetTimestamp = $offset[0];
            }
            if (isset($offset[1])) {
                $offsetLine = $offset[1];
            }

            if ($offsetTimestamp && $offsetLine) {
                SkipprLogger::info("Restarting File sync from checkpoint time $offsetTimestamp line $offsetLine");
            }

            $filenames = glob($path . '/*', GLOB_NOSORT);

            usort($filenames, function ($a, $b) {
                return filemtime($a) - filemtime($b);
            });

            foreach ($filenames as $filename) {
                if ($this->ingestNamespace($path)) {
                    $timestamp = filemtime($filename);

                    if ($timestamp >= $offsetTimestamp) {
                        $line = 0;

                        if (preg_match(
                                "/\.gz(ip)?$|.zip/",
                                $filename
                            ) == true) {
                            SkipprLogger::info('Uncompressing file ' . $filename);

                            // open gz file for reading
                            $sfp = gzopen($filename, 'rb');

                            // read and decode chunks into string stream
                            while (!gzeof($sfp)) {
                                $line++;

                                $string = gzgets($sfp);

                                $offset = "$timestamp $line";

                                $pipelineJob->emit($string, $offset, $path);
                            }

                            gzclose($sfp);
                        } else {
                            $sfp = fopen($filename, 'rb');

                            // read and decode chunks into string stream
                            while (!feof($sfp)) {
                                $line++;

                                $string = fgets($sfp);

                                $offset = "$timestamp $line";

                                $pipelineJob->emit($string, $offset, $path);
                            }

                            // remove temp file
                            fclose($sfp);
                        }
                    }
                }
            }
        } catch (\Exception $e) {
            SkipprLogger::error("Error syncing data from File Plugin");
            throw $e;
        }
    }

    public function doValidateConnection(): ValidationResponse
    {

        $validationResp = new ValidationResponse('Connection Succeeded');

        try {
            $paths = $this->config['path'];
            $paths = $this->splitNamespaces($paths);
        } catch (\Exception $e) {
            $validationResp->title = "Could not connect to source data.";
            $validationResp->error = $e->getMessage();
            SkipprLogger::error($validationResp->error);
            $data = false;
        }

        return $validationResp;
    }

    public function doValidateConfig(): ValidationResponse
    {

        $validationResp = new ValidationResponse('Connection Succeeded');

        try {
            $paths = $this->config['path'];
            $paths = $this->splitNamespaces($paths);
        } catch (\Exception $e) {
            $validationResp->title = "Could not connect to source data.";
            $validationResp->error = $e->getMessage();
            SkipprLogger::error($validationResp->error);
        }


        return $validationResp;
    }

    public function shutdown()
    {

        return 'success';
    }
}
