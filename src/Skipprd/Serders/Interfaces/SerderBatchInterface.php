<?php

namespace Skipprd\Serders\Interfaces;

interface SerderBatchInterface
{

    public function __construct();

    public function deserialize(string $record): array;

    public function openWriter(string $filename, array $schema): void;

    public function closeWriter(): void;

    public function serialize(array $record): void;

    public function defaultMessage(array $schema): array;
}
