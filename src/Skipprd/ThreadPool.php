<?php

namespace Skipprd;


use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;

class ThreadPool
{

    /**
     * @var \parallel\Channel
     */
    protected $channel;

    public $threadPool = [];

    public function initPool()
    {

        $channel = \parallel\Channel::make("input.field", 100);

        $ncpu = 4;

        if (is_file('/proc/cpuinfo')) {
            $cpuinfo = file_get_contents('/proc/cpuinfo');
            preg_match_all('/^processor/m', $cpuinfo, $matches);
            $ncpu = count($matches[0]);
        }

        for ($i = 0; $i < $ncpu; $i++) {

            $this->threadPool[$i] = new \parallel\Runtime(__DIR__ . '/../../../bootstrap/autoload.php');

        }

        do {
            usleep(1);
            $allDone = array_reduce(
                $this->threadPool,
                function (bool $c, parallel\Future $future): bool {
                    return $c && $future->done();
                },
                true
            );
        } while (false === $allDone);
    }

    public function run() {
        $this->threadPool[$i]->run(function (string $dataType, string $field, $value, array $metadata = []) use ($channel) {

            Ingest::fastSetValue($dataType,  $field, $value, $metadata);
        });
    }
}
