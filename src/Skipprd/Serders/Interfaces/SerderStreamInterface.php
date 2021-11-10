<?php

namespace Skipprd\Serders\Interfaces;

interface SerderStreamInterface
{

    public function __construct();
    
    public function deserialize(string $record) : array;

    public function serialize(array $record, $schema = null) : string;
}
