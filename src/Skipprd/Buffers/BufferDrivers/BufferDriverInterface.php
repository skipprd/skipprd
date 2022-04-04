<?php


namespace Skipprd\Buffers\BufferDrivers;

interface BufferDriverInterface
{

    /**
     * @return bool|string
     */
    public function stream();

    /**
     * @return bool|string
     */
    public function nextFile();
}
