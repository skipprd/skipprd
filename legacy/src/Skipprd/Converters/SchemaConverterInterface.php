<?php


namespace legacy\src\Skipprd\Converters;

interface SchemaConverterInterface
{

    public function convert($schema) : array;
}
