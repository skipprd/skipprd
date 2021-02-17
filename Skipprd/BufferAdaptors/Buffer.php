<?php


namespace Skipprd\BufferAdaptors;


use App\Helpers\BytesToHuman;

interface Buffer
{

    public function __construct(string $name, int $flushBytes);

    public function flush(string $name) : void;

    public function append(string $message, bool $flush = false) : void;

    public function lockedRead();
    
    public function lock(string $name) : bool;

    public function unlockAll() : void;

    public function unlock(string $name) : bool;

    public function close($fp) : bool;

    public function destroy($fp) : bool;

    public function finalise($force = false) :void;
}
