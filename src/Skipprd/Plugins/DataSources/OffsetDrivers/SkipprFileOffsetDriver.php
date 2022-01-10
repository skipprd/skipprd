<?php

namespace Skipprd\Plugins\DataSources\OffsetDrivers;

use Monolog\Registry;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class SkipprFileOffsetDriver implements OffsetDriverInterface
{

    /**
     * Offsets currently committed to the backend. These will be at or behind the offsets during ingestion.
     * @var array
     */
    private $committedOffsets = [];

    protected $pipelineName = '';

    public function __construct()
    {
        $this->pipelineName = Config::getPipelineName();
    }

    public function get(): array
    {

        $this->committedOffsets = $this->client();

        return $this->committedOffsets;
    }

    public function sync(string $namespace, string $partition, string $offset): void
    {

        $this->committedOffsets[$namespace][$partition] = $offset;

        $this->client('PUT', $this->committedOffsets);
    }

//    public function syncAll(array $offsets): void
//    {
//
//        $this->committedOffsets = $offsets;
//
//        $this->client('PUT', $this->committedOffsets);
//    }

    protected function client(string $method = 'GET', array $data = [])
    {

        try {
            switch ($method) {
                case 'PUT':
                    try {
                        $state[Config::$pipelineName]['offsets'] = $data;

                        file_put_contents(
                            Config::$dataDir . '/skippr-offsets.json',
                            json_encode($state),
                            LOCK_EX
                        );



                        SkipprLogger::debug('Written offsets to ' . Config::$dataDir . '/skippr-offsets.json');
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
                                SkipprLogger::info('Loading offsets for pipeline ' . Config::$pipelineName);

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
