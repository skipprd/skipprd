<?php

namespace legacy\src\Skipprd\Plugins\OffsetDrivers;

interface OffsetDriverInterface
{

    public function get() : array;

    function sync(string $namespace, string $partition, string $offset) : void;

    function getOffsets(string $namespace, array $partitions): array;

    function getOffset(string $namespace, string $partition = ''): array;

    function resetSourceOffsets(): void;

    function offsetCommitAll(array $offsets): array;

//    function syncAll(array $offsets) : void;
}
