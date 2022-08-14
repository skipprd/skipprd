<?php

namespace Skipprd\Traits;


use Carbon\Carbon;

trait TaskResponse
{
    public int $totalEntries = 0;
    public array $incrementNamespacesCount = [];
    public int $currentEntries = 0;
    public int $dataReadBytes = 0;
    public int $deadLetters = 0;
    public int $deadLettersCurrent = 0;
    public int $startTimestamp = 0;
    public int $fastPath = 0;
    public int $slowPath = 0;


    public function getResponse(): array {

        $resp = [
            "msgs_total" => $this->totalEntries,
            "msgs_current" => $this->currentEntries,
            "bytes_total" => $this->dataReadBytes,
            "deadletters_total" => $this->deadLetters,
            "deadletters_current" => $this->deadLettersCurrent,
            "run_time_seconds" => Carbon::now()->timestamp - $this->startTimestamp,
        ];

        foreach ($this->incrementNamespacesCount as $name => $count) {
            $resp['msgs_' . $name] = $count;
            $this->incrementNamespacesCount[$name] = 0;
        }

        return $resp;
    }

    public function incrementNamespacesCount(string $namespace) {
        if (!isset($this->incrementNamespacesCount[$namespace])) {
            $this->incrementNamespacesCount[$namespace] = 1;
        } else {
            $this->incrementNamespacesCount[$namespace]++;
        }
    }
}