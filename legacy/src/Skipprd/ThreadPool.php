<?php

namespace legacy\src\Skipprd;


use function Skipprd\count;

class ThreadPool
{

    /**
     * @var \parallel\Channel
     */
    protected $channel;

    public $threadPool = [];

    public function initPool(int $threadCount = 0)
    {

        $channel = \parallel\Channel::make("input.field", 100);

        if ($threadCount === 0) {

            $ncpu = 2;

            if (is_file('/proc/cpuinfo')) {
                $cpuinfo = file_get_contents('/proc/cpuinfo');
                preg_match_all('/^processor/m', $cpuinfo, $matches);
                $ncpu = count($matches[0]);
            }

            $threadCount = $ncpu * 2;
        }

        SkipprLogger::info("Starting $threadCount threads");

        for ($i = 0; $i < $threadCount; $i++) {

            $this->threadPool[$i] = new \parallel\Runtime(__DIR__ . '/../../vendor/autoload.php');
//            $this->threadPool[$i]['active'] = false;

        }

//        do {
//            usleep(1);
//            $allDone = array_reduce(
//                $this->threadPool,
//                function (bool $c, parallel\Future $future): bool {
//                    return $c && $future->done();
//                },
//                true
//            );
//        } while (false === $allDone);
    }

//    public function finish(int $i): void {
//        $this->threadPool[$i]['active'] = false;
//    }

    public function next(): \parallel\Runtime
    {
        if (!$thread = next($this->threadPool)) {
            usleep(1);
            $thread = reset($this->threadPool);
        }

//        if (current($this->threadPool)['active']) {
//            $thread = $this->next();
//        }
//
//        current($this->threadPool)['active'] = true;

        return $thread;
    }

    public function closeAll(): void
    {
        foreach ($this->threadPool as $i => $thread) {
            $this->threadPool[$i]->close();
        }
    }
//    public function run() {
//        $this->threadPool[$i]->run(function (string $dataType, string $field, $value, array $metadata = []) use ($channel) {
//
//            Ingest::fastSetValue($dataType,  $field, $value, $metadata);
//        });
//    }
}
