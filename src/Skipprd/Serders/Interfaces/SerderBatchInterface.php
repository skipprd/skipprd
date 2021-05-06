<?php

namespace Skipprd\Serders\Interfaces;


interface SerderBatchInterface
{

    public function __construct(\AvroSchema $schema = null);

    public function deserialize(string $record): array;

    public function serialize(array $record, string $filename): void;
}