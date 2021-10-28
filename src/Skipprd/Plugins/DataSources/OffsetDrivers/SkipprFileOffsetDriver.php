<?php

namespace Skipprd\Plugins\DataSources\OffsetDrivers;

use Monolog\Registry;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class SkipprFileOffsetDriver implements OffsetDriverInterface
{

    protected $pipelineName = '';

    public function __construct()
    {
        $this->pipelineName = Config::getPipelineName();

    }

    public function get() : array
    {

        $offsets = $this->client();

        return $offsets;

    }

    public function sync(string $partition, string $offset) : void
    {

        $this->client('PUT', [$partition => $offset]);

    }

    public function syncAll(array $offsets) : void
    {

        $this->client('PUT', $offsets);

    }

    protected function client(string $method = 'GET', array $data = [])
    {

        $state[Config::$pipelineName]['offsets'] = Config::$offsets;

        try {

            switch ($method) {
            case 'PUT':

                try {

                    file_put_contents(Config::$dataDir . '/skippr-offsets.json', json_encode($state));

                    SkipprLogger::info('Written state to ' . Config::$dataDir . '/skippr-offsets.json');


                } catch (\Exception $e) {
                    SkipprLogger::error($e->getMessage());
                }

                break;

            case 'GET':

                $offsets = [];

                if (file_exists(Config::$dataDir . '/skippr-offsets.json')) {

                    try {

                        SkipprLogger::info('Found existing ' . Config::$dataDir . '/skippr-offsets.json');

                        $state = json_decode(
                            file_get_contents(Config::$dataDir . '/skippr-offsets.json'),
                            true
                        );

                        if (!empty($state[Config::$pipelineName])) {

                            SkipprLogger::info('Loading state for pipeline ' . Config::$pipelineName);

                            $offsets = (!empty($state[Config::$pipelineName]['offsets']) ? $state[Config::$pipelineName]['offsets'] : []);

                        }

                    } catch (\Exception $e) {
                        SkipprLogger::error($e->getMessage());
                    }

                }

                return $offsets;
            }

        } catch (\Exception $e) {

            SkipprLogger::error($e->getMessage());
        }
    }

}
