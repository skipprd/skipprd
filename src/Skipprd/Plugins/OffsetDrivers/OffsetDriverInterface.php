<?php

namespace Skipprd\Plugins\OffsetDrivers;

interface OffsetDriverInterface
{

    public function get() : array;

    public function sync(string $namespace, string $partition, string $offset) : void;

    public function getOffset(string $namespace, string $partition = ''): array;

    public function resetSourceOffsets(): void;

    public function offsetCommitAll(array $offsets): void;

//    public function syncAll(array $offsets) : void;
}
