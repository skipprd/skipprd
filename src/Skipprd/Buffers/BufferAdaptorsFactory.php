<?php


namespace Skipprd\Buffers;

trait BufferAdaptorsFactory
{

    static function getAdaptor(string $bufferName, string $driverType, int $flushBytes = null) : BufferInterface
    {

        $driverName = ucfirst($driverType) . 'BufferDriver';
        $driverClassName = "Skipprd\\Buffers\\BufferDrivers\\$driverName";

        $driver = new $driverClassName($bufferName);

        $adaptorClassName = "Skipprd\\Buffers\\ChunkedBuffer";

        $adaptor = new $adaptorClassName($bufferName, $driver, $flushBytes);

        return $adaptor;
    }
}
