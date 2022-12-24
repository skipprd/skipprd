<?php

namespace Skipprd\Serders\Interfaces;

interface SerderStreamInterface
{
    const VALUE_COMPRESSION = 'value_compression';
    const FILE_COMPRESSION = 'file_compression';
    const NO_COMPRESSION = 'no_compression';

    public function __construct();


    public function deserialize(string $record) : array;

    public function openWriter(string $filename, array $schema): void;

    public function closeWriter(): void;

    public function serialize(array $record, array $schema = null): void;

    public function defaultMessage(array $schema): array;
}
