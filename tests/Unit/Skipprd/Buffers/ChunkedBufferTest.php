<?php

namespace Unit\Skipprd\Buffers;

use PHPUnit\Framework\TestCase;
use Skipprd\Buffers\BufferDrivers\BufferDriverInterface;
use Skipprd\Buffers\ChunkedBuffer;

class TestBuffer implements BufferDriverInterface {
    public function nextFile()
    {
        // TODO: Implement nextFile() method.
    }
    public function stream()
    {
        // TODO: Implement stream() method.
    }
}

class ChunkedBufferTest extends TestCase
{

    public function testEncodeChunkName()
    {

        $testBuffer = new TestBuffer();

        $chunkedBuffer = new ChunkedBuffer('foo', $testBuffer);

        $namespace = 'foo_namespace';
        $partition = 'foo_partition';
        $timeBucket = 600;

        $chunkName = $chunkedBuffer->encodeChunkName($namespace, $partition, $timeBucket);
        
        $this->assertStringContainsString($namespace, $chunkName);
        $this->assertStringContainsString($partition, $chunkName);
        $this->assertStringContainsString($timeBucket, $chunkName);
        $this->assertStringContainsString('time', $chunkName);
    }

    public function testEncodeChunkNameNoTime()
    {

        $testBuffer = new TestBuffer();

        $chunkedBuffer = new ChunkedBuffer('foo', $testBuffer);

        $namespace = 'foo_namespace';
        $partition = 'foo_partition';
        $timeBucket = 0;

        $chunkName = $chunkedBuffer->encodeChunkName($namespace, $partition, $timeBucket);

        $this->assertStringContainsString($namespace, $chunkName);
        $this->assertStringContainsString($partition, $chunkName);

        $this->assertStringNotContainsString($timeBucket, $chunkName);
        $this->assertStringNotContainsString('time', $chunkName);
    }

    public function testEncodeChunkNameNullTime()
    {

        $testBuffer = new TestBuffer();

        $chunkedBuffer = new ChunkedBuffer('foo', $testBuffer);

        $namespace = 'foo_namespace';
        $partition = 'foo_partition';
        $timeBucket = 0;

        $chunkName = $chunkedBuffer->encodeChunkName($namespace, $partition, $timeBucket);

        $this->assertStringContainsString($namespace, $chunkName);
        $this->assertStringContainsString($partition, $chunkName);

        $this->assertStringNotContainsString('time', $chunkName);
    }

    public function testDecodeChunkTime()
    {

        $testBuffer = new TestBuffer();

        $chunkedBuffer = new ChunkedBuffer('foo', $testBuffer);

        $namespace = 'foo_namespace';
        $partition = 'foo_partition';
        $timeBucket = 1653902353;

        $chunkName = $chunkedBuffer->encodeChunkName($namespace, $partition, $timeBucket);

        $timePartition = $chunkedBuffer->decodeChunkTime($chunkName);

        $this->assertEquals('2022-05-30T09:19:13+00:00', $timePartition);

    }
}
