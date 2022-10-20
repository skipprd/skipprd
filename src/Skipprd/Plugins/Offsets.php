<?php


namespace Skipprd\Plugins;

use Skipprd\InternalFields;
use Skipprd\Plugins\OffsetDrivers\OffsetDriverInterface;
use Skipprd\Traits\SkipprLogger;

class Offsets
{

    protected array $offsets = [];

    public OffsetDriverInterface $offsetClient;

    public function __construct(OffsetDriverInterface $client)
    {
        $this->offsetClient = $client;
    }

    public function getAll(): array
    {

        return $this->offsets;
    }

    public function getOffset(string $namespace, string $partition = ''): array
    {

        $offsets = [];

        // get latest high watermark from offset
        if (!empty($this->offsets[$namespace][$partition])) {
            $offsets = explode(' ', $this->offsets[$namespace][$partition]);

            // else get last committed offset
        } else {
            $offsets = $this->offsetClient->getOffset($namespace, $partition);
            $this->offsets[$namespace][$partition] = $offsets;
        }

        return $offsets;
    }

    public function setOffsets(string $offsets, string $namespace, string $partition = ''): void
    {

        try {
            $this->offsets[$namespace][$partition] = $offsets;
        } catch (\Exception $e) {
            SkipprLogger::error($e->getMessage());
            SkipprLogger::error("Namespace: $namespace, Partition: $partition, Offsets: $offsets");

        }
    }

    public function offsetCommitLatest(
        string $source_namespace
    ): void {

        // Input plugin buffers to namespace chunks, a buffer flush to disk always
        // flushes all offsets, up-to the current high watermark.
        SkipprLogger::info("Committing offset for Namespace: $source_namespace");

        // Commit all offsets for this namespace
        $toCommit[$source_namespace] = $this->offsets[$source_namespace];
        $this->offsets[$source_namespace] = $this->offsetClient->offsetCommitAll($toCommit);


    }

    public function getCurrentOffsets(string $namespace, string $partition = ''): string
    {
        return $this->offsets[$namespace][$partition] ?? '';
    }

    public function validateOffset(string $args, string $namespace, string $partition = '') : bool
    {

        //        $offsets = $this->getOffsets();
        //        return bccomp($args, $offsets, 5) == 1;
        
        $offsets = $this->offsetClient->getOffset($namespace, $partition);

        $args = explode(' ', $args);

        $i = 0;
        $total = count($args);

        while ($i < $total) {
            if ($args[$i] >= $offsets[$i]) {
                if ($args[$i] == $offsets[$i]) {
                    $next = $i + 1;

                    if ($total > $next) {
                        $subArgs = array_slice($args, $next);
                        $subArgs = implode(' ', $subArgs);
                        $this->validateOffset($subArgs, $namespace, $partition);
                    }
                } else {
                    return true;
                }
            } else {
                return false;
            }

            $i++;
        }

        return false;
    }
}
