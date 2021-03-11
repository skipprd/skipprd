<?php


namespace Skipprd\Buffers;

interface BufferInterface
{

    public function __construct(string $name, int $flushBytes);

    public function flush(string $name) : void;

    public function append(array $message, bool $flush = false) : void;

    public function nextFile();
    
    public function lock(string $name) : bool;

    public function unlockAll() : void;

    public function unlock(string $name) : bool;

    public function close($fp) : bool;

    public function destroy($fp) : bool;

    public function finalise($force = false) :void;
}
