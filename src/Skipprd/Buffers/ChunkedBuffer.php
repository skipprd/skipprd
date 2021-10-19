<?php


namespace Skipprd\Buffers;

use Monolog\Registry;
use Carbon\Carbon;
use Skipprd\Buffers\BufferDrivers\BufferDriverInterface;
use Skipprd\Str;

class ChunkedBuffer implements BufferInterface
{

    protected $memBuffs = [];
    
    protected $bufferName = '';
    public $flushMemBytes = 1000000; # 1MB

    public $driver;

    protected static $eventTimeBucketDurationSeconds = 600;
//    protected static $eventTimeBucketDurationSeconds = 3600;
//    protected static $eventTimeBucketDurationSeconds = 86400;

    public function __construct(string $bufferName, BufferDriverInterface $bufferDriver, string $flushBytes = null)
    {
        $this->bufferName = $bufferName;

        if (!empty($flushBytes)) {
            $this->flushMemBytes = $flushBytes;
        }

        $this->driver = $bufferDriver;
    }

    public function append(array $payload, bool $flush = false, int $eventTime = 0, string $partition = null) : void {

        $timeBucket = $this->eventTimeBucket($eventTime);

        $chunkName = $this->encodeChunkName($partition, $timeBucket);

//        if (empty($this->memBuffs[$chunkName])) {
//
//            $this->memBuffs[$chunkName]['size'] = 0;
//            $this->memBuffs[$chunkName]['buffer'] = "";
//
//        }

        if (empty($this->memBuffs[$chunkName])) {

//            $this->memBuffs[$chunkName]['size'] = mb_strlen($payload) * 8;
            $this->memBuffs[$chunkName]['size'] = mb_strlen(serialize((array)$payload), '8bit');
            $this->memBuffs[$chunkName]['time'] = time();
//            $this->memBuffs[$chunkName]['buffer'] = "$payload";

        } else {
//            $this->memBuffs[$chunkName]['size'] += mb_strlen($payload) * 8;
            $this->memBuffs[$chunkName]['size'] += mb_strlen(serialize((array)$payload), '8bit');
            $this->memBuffs[$chunkName]['time'] = time();
//            $this->memBuffs[$chunkName]['buffer'] .= "$payload";

        }

        $this->memBuffs[$chunkName]['buffer'][] = $payload;

        if ($flush
            || $this->memBuffs[$chunkName]['size'] > $this->flushMemBytes
//            || $this->memBuffs[$chunkName]['time'] < time() - $this->flushMemSeconds
        ) {


            if (!empty($this->memBuffs[$chunkName]) && !empty($this->memBuffs[$chunkName]['buffer'])) {

                $this->driver->flush($this->memBuffs[$chunkName]['buffer'], $chunkName, $partition);

                unset($this->memBuffs[$chunkName]);
            }
        }

    }

    public function flushAll(bool $force = false): void {

        foreach ($this->memBuffs as $chunkName => $buffer) {

            if ($force
                || $buffer['size'] > $this->flushMemBytes
//                || $buffer['time'] < time() - $this->flushMemSeconds
            ) {

                $partition = $this->decodeChunkPartitionName($chunkName);

                if (!empty($this->memBuffs[$chunkName]) && !empty($this->memBuffs[$chunkName]['buffer'])) {

                    $this->driver->flush($this->memBuffs[$chunkName]['buffer'], $chunkName, $partition);

                    unset($this->memBuffs[$chunkName]);
                }
            }

        }
    }

    public function eventTimeBucket(int $eventTime) : int {

        $bucket = $eventTime - ($eventTime % self::$eventTimeBucketDurationSeconds);

        return $bucket;

    }

    public function encodeChunkName($partition, $timeBucket): string
    {

        $chunkName = implode('-', [$this->bufferName, $timeBucket, $partition]);
        
        return $chunkName;
    }

    public function getChunkName($filename) : array {

        $startPos = strpos($filename, $this->bufferName) + strlen($this->bufferName);
//        $endPos = strpos($filename, '_finalised') - strlen('_finalised');
        $endPos = strrpos($filename, '_finalised', -1);
//        $encodedName = substr($filename, $startPos, -$endPos);
        $encodedName = substr($filename, $startPos, -43);
        $encodedName = trim($encodedName, '-');

        return explode('-', $encodedName);

    }

    public function decodeChunkTime($filename) : string {

        $parts = $this->getChunkName($filename);

        if ($parts[0] < 0) {

            $timestamp = array_shift($parts);

            $date_string = Carbon::createFromTimestamp($timestamp)->format('Y-m-d');

            return 'dt=' . $date_string;

        }

        return '';

    }

    public function decodeChunkPartition($filename) : string {

        Registry::skipprd()->debug("decoding partitions for file $filename");

        $parts = $this->getChunkName($filename);

        // strip chunk time
        if (is_numeric($parts[0])) {
            array_shift($parts);
        }

        // reassemble chunk name
//        $nameParts = array_pop($parts);
        $partition_dir = implode('-', $parts);
//        $parts[] = $partition;

//        $partition_dir = trim(implode('/', $parts), '/');

        Registry::skipprd()->debug("decoded partition dir $partition_dir");

        return $partition_dir;
    }

    public function decodeChunkPartitionName(string $chunkName) : string {

        $parts = explode('-', $chunkName);
        unset($parts[0]); // buffer name
        unset($parts[1]); // time
        return implode('-', $parts);
    }

//    public function nextFile()
//    {
//
//        $filenames = glob($this->bufferDir . '/' . "$this->name*-finalised-*", GLOB_NOSORT);
//
//        asort($filenames);
//
//        foreach ($filenames as $filename) {
//
//            try {
//
//                // possible file removed by competing thread
//                if (!file_exists($filename)) continue;
//
//                // ignore locked files
//                if (strpos($filename, '.lock')) continue;
//                if (strpos($filename, '.checkpoint')) continue;
//
//                if (FileBufferDriver::lock($filename, false)) {
//
//                    return $filename;
//
//                }
//
//            } catch (\Exception $e) {
//
//                // Still possible the file has been deleted just before with stat the size
//                Registry::skipprd()->debug($e->getMessage());
//
//            }
//
//        }
//
//        return false;
//    }

}
