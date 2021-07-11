<?php

namespace Skipprd\Plugins\DataSources\OffsetDrivers;

interface OffsetDriverInterface
{

    public function get() : array ;

    public function sync(string $partition, string $offset) : void;

    public function syncAll(array $offsets) : void;

}