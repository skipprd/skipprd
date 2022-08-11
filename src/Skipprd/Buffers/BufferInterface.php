<?php


namespace Skipprd\Buffers;

interface BufferInterface
{

    public function append(
        string $payload,
        int $bytes,
        int $eventTime = 0,
        string $namespace = null,
        string $partition = null
    ) : int;

    public function flushAll(bool $finalize = false): void;

    public function eventTimeBucket(int $eventTime) : int;

    public function encodeChunkName(string $namespace, string $partition, int $timeBucket = 0): string;

    public function getChunkName($filename) : array;

    public function decodeChunkTime($filename) : string;

    public function decodeFileNamespace($filename) : string;

    public function decodeChunkNamespace(string $chunkName) : string;

    public function decodeChunkPartition(string $chunkName) : string;
}
