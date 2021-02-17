<?php


namespace Skipprd\BufferAdaptors;


trait BufferAdaptorsFactory
{

    static function getAdaptor(string $name, string $type, int $bytes = null) : Buffer
    {

        $adaptorName = ucfirst($type) . 'Buffer';

        $adaptorName = "Skipprd\\BufferAdaptors\\" . $adaptorName;

        $adaptor = new $adaptorName($name, $bytes);

        return $adaptor;
    }

}
