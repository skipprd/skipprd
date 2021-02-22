<?php


namespace Skipprd\Buffers;


trait BufferAdaptorsFactory
{

    static function getAdaptor(string $name, string $type, int $bytes = null) : BufferInterface
    {

        $adaptorName = ucfirst($type) . 'Buffer';

        $adaptorName = "Skipprd\\Buffers\\" . $adaptorName;

        $adaptor = new $adaptorName($name, $bytes);

        return $adaptor;
    }

}
