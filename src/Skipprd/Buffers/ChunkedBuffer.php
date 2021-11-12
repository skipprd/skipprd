<?php


namespace Skipprd\Buffers;

use Monolog\Registry;
use Carbon\Carbon;
use Skipprd\Buffers\BufferDrivers\BufferDriverInterface;
use Skipprd\MachineToHuman\BytesToHuman;
use Skipprd\Str;
use Skipprd\Traits\SkipprLogger;

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

    public function append(
        array $payload,
        bool $flush = false,
        int $eventTime = 0,
        string $namespace = null,
        string $partition = null
    ) : void
    {

        $timeBucket = $this->eventTimeBucket($eventTime);

        $chunkName = $this->encodeChunkName($namespace, $partition, $timeBucket);

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
                SkipprLogger::debug("Flushing buffer chunk $chunkName of size ". BytesToHuman::toHuman($this->memBuffs[$chunkName]['size'], true));

                $this->driver->flush($this->memBuffs[$chunkName]['buffer'], $chunkName, $namespace);

                unset($this->memBuffs[$chunkName]);
            }
        }
    }

    public function flushAll(bool $force = false): void
    {

        foreach ($this->memBuffs as $chunkName => $buffer) {
            if ($force
                || $buffer['size'] > $this->flushMemBytes
//                || $buffer['time'] < time() - $this->flushMemSeconds
            ) {
                $namespace = $this->decodeChunkNamespace($chunkName);

                if (!empty($this->memBuffs[$chunkName]) && !empty($this->memBuffs[$chunkName]['buffer'])) {
                    SkipprLogger::debug("Flushing buffer chunk $chunkName of size ". BytesToHuman::toHuman($this->memBuffs[$chunkName]['size'], true));

                    $this->driver->flush($this->memBuffs[$chunkName]['buffer'], $chunkName, $namespace);

                    unset($this->memBuffs[$chunkName]);
                }
            }
        }
    }

    public function eventTimeBucket(int $eventTime) : int
    {

        $bucket = $eventTime - ($eventTime % self::$eventTimeBucketDurationSeconds);

        return $bucket;
    }

    public function encodeChunkName(string $namespace = null, string $partition = null, $timeBucket = null): string
    {

        $chunkName = http_build_query([
            'buffer' => $this->bufferName,
            'time' => $timeBucket,
            'namespace' => $namespace,
            'partition' => $partition]
        );

        return $chunkName;
    }

    public function getChunkName($filename) : array
    {

        $startPos = strpos($filename, $this->bufferName) + strlen($this->bufferName);
//        $endPos = strpos($filename, '_finalised') - strlen('_finalised');
        $endPos = strrpos($filename, '_finalised', -1);
//        $encodedName = substr($filename, $startPos, -$endPos);
        $encodedName = substr($filename, $startPos, -43);
        $encodedName = trim($encodedName, '-');

        return explode('-', $encodedName);
    }

    public function decodeChunkTime($filename) : string
    {

        parse_str($filename, $array);
        $date_string = Carbon::createFromTimestamp($array['time'])->format('Y-m-d');

        SkipprLogger::debug("decoding buffer time $date_string file $filename");


        return $date_string;

//        $parts = $this->getChunkName($filename);
//
//        if ($parts[0] < 0) {
//            $timestamp = array_shift($parts);
//
//            $date_string = Carbon::createFromTimestamp($timestamp)->format('Y-m-d');
//
//            return 'dt=' . $date_string;
//        }
//
//        return '';
    }

    public function decodeFilePartition($filename) : string
    {

        parse_str($filename, $array);
        $partition = $array['partition'];

        SkipprLogger::debug("decoding buffer partition $partition file $filename");

        return $partition;

    }

    public function decodeFileNamespace($filename) : string
    {

        parse_str($filename, $array);
        $namespace = $array['namespace'];

        SkipprLogger::debug("decoding buffer namespace $namespace file $filename");

        return $namespace;

    }

    public function decodeChunkPartition(string $chunkName) : string
    {

        parse_str($chunkName, $array);
        $partition = $array['partition'];

        SkipprLogger::debug("decoding buffer partition $partition chunk $chunkName");

        return $partition;

    }

    public function decodeChunkNamespace(string $chunkName) : string
    {

        parse_str($chunkName, $array);
        $namespace = $array['namespace'];

        SkipprLogger::debug("decoding buffer namespace $namespace chunk $chunkName");

        return $namespace;
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
//                SkipprLogger::debug($e->getMessage());
//
//            }
//
//        }
//
//        return false;
//    }
}
