<?php


namespace Skipprd\Converters;

interface SchemaConverterInterface
{

    public function convert($schema) : array;
}
