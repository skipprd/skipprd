<?php


namespace legacy\src\Skipprd\Buffers;

class BufferAdaptorsFactory
{

    static function getAdaptor(string $bufferName, string $driverType) : BufferInterface
    {

        $driverName = ucfirst($driverType) . 'BufferDriver';
        $driverClassName = "Skipprd\\Buffers\\BufferDrivers\\$driverName";

        $driver = new $driverClassName($bufferName);

        $adaptorClassName = "Skipprd\\Buffers\\ChunkedBuffer";

        $adaptor = new $adaptorClassName($bufferName, $driver);

        return $adaptor;
    }
}
