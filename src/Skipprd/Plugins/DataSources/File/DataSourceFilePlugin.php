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

    public function connect()
    {
        return true;
    }
    
    public function sync($pipelineJob)
    {

        try {
            $paths = $this->config['path'];

            foreach ($this->splitPartitions($paths) as $path) {
                $offsetTimestamp = 0;
                $offsetLine = 0;

                $offset = $this->offsets->parseOffsets($path);

                if (isset($offset[0])) {
                    $offsetTimestamp = $offset[0];
                }
                if (isset($offset[1])) {
                    $offsetLine = $offset[1];
                }

                if ($offsetTimestamp && $offsetLine) {
                    Registry::skipprd()
                        ->info("Restarting File sync from checkpoint time $offsetTimestamp line $offsetLine");
                }

                $filenames = glob($path . '/*', GLOB_NOSORT);

                usort($filenames, function ($a, $b) {
                    return filemtime($a) - filemtime($b);
                });

                foreach ($filenames as $filename) {
                    if ($this->ingestPartition($path)) {
                        $timestamp = filemtime($filename);

                        if ($timestamp >= $offsetTimestamp) {
                            $line = 0;

                            if (preg_match(
                                "/\.gz(ip)?$|.zip/",
                                $filename
                            ) == true) {
                                Registry::skipprd()
                                    ->info('Uncompressing file ' . $filename);

                                // open gz file for reading
                                $sfp = gzopen($filename, 'rb');

                                // read and decode chunks into string stream
                                while (!gzeof($sfp)) {
                                    $line++;

                                    $string = gzgets($sfp);

                                    if ($this->offsets->validateOffset(
                                        $path,
                                        "$timestamp $line"
                                    )) {
                                        $offset = "$timestamp $line";

                                        $pipelineJob->emit($string, $path);

                                        $this->offsets->setOffsets(
                                            $path,
                                            $offset
                                        );
                                    }
                                }

                                gzclose($sfp);
                            } else {
                                $sfp = fopen($filename, 'rb');

                                // read and decode chunks into string stream
                                while (!feof($sfp)) {
                                    $line++;

                                    $string = fgets($sfp);

                                    if ($this->offsets->validateOffset(
                                        $path,
                                        "$timestamp $line"
                                    )) {
                                        $offset = "$timestamp $line";

                                        $pipelineJob->emit($string, $path);

                                        $this->offsets->setOffsets(
                                            $path,
                                            $offset
                                        );
                                    }
                                }

                                // remove temp file
                                fclose($sfp);
                            }
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
            $paths = $this->splitPartitions($paths);
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
            $paths = $this->splitPartitions($paths);
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
