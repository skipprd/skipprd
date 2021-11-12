<?php


namespace Skipprd\Buffers;

interface BufferInterface
{

    public function append(
        array $payload, 
        bool $flush = false,
        int $eventTime = 0,
        string $namespace = null,
        string $partition = null
    ) : void;

    public function flushAll(bool $force = false): void;

    public function eventTimeBucket(int $eventTime) : int;

    public function encodeChunkName(string $namespace, string $partition, $timeBucket): string;

    public function getChunkName($filename) : array;

    public function decodeChunkTime($filename) : string;

    public function decodeFileNamespace($filename) : string;

    public function decodeChunkNamespace(string $chunkName) : string;

    public function decodeChunkPartition(string $chunkName) : string;
}
