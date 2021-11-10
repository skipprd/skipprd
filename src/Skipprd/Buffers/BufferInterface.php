<?php


namespace Skipprd\Buffers;

interface BufferInterface
{

    public function append(array $payload, bool $flush = false, int $eventTime = 0, string $partition = null) : void;

    public function flushAll(bool $force = false): void;

    public function eventTimeBucket(int $eventTime) : int;

    public function encodeChunkName($partition, $timeBucket): string;

    public function getChunkName($filename) : array;

    public function decodeChunkTime($filename) : string;

    public function decodeChunkPartition($filename) : string;

    public function decodeChunkPartitionName(string $chunkName) : string;
}
