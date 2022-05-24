<?php

namespace Skipprd\Serders\Interfaces;

interface SerderBatchInterface
{

    public function __construct();

    public function deserialize(string $record): array;

    public function serialize(array $record, string $filename, $schema = null): void;

    public function defaultMessage(array $schema): array;
}
