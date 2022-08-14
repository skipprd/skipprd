<?php


namespace Skipprd\Buffers;

use Monolog\Registry;
use Carbon\Carbon;
use Skipprd\Buffers\BufferDrivers\BufferDriverInterface;
use Skipprd\Buffers\BufferDrivers\FileBufferDriver;
use Skipprd\Helpers;
use Skipprd\InternalFields;
use Skipprd\MachineToHuman\BytesToHuman;
use Skipprd\SkipprPack;
use Skipprd\Str;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class ChunkedBuffer implements BufferInterface
{

    protected const BUFFER_APPENDED=1;
    protected const BUFFER_APPENDED_FLUSHED=2;

    protected $memBuffs = [];

    protected $bufferName = '';

    /**
     * @var \Skipprd\Buffers\BufferDrivers\FileBufferDriver
     */
    public $driver;

    public function __construct(string $bufferName, BufferDriverInterface $bufferDriver)
    {
        $this->bufferName = $bufferName;

        $this->driver = $bufferDriver;
    }

    /**
     * @param array $payload
     * @param int $bytes
     * @param bool $flush
     * @param int $eventTime
     * @param string|null $namespace
     * @param string|null $partition
     * @return int - status flag, BUFFER_APPENDED for record appened to buffer memory.
     * BUFFER_APPENDED_FLUSHED for record appended and all buffer memory flushed to persistent buffer driver.
     */
    public function append(
        string $payload,
        int $bytes,
        int $eventTime = 0,
        string $namespace = '',
        string $partition = ''
    ) : int {

        $return = self::BUFFER_APPENDED;

        $timeBucket = $this->eventTimeBucket($eventTime);

        $chunkName = $this->encodeChunkName($namespace, $partition, $timeBucket);

        if (empty($this->memBuffs[$chunkName])) {
//            $this->memBuffs[$chunkName]['size'] = mb_strlen($payload) * 8;
            $this->memBuffs[$chunkName]['size'] = $bytes;
            $this->memBuffs[$chunkName]['time'] = time();
            $this->memBuffs[$chunkName]['count'] = 1;
//            $this->memBuffs[$chunkName]['buffer'] = '';
        } else {
//            $this->memBuffs[$chunkName]['size'] += mb_strlen($payload) * 8;
            $this->memBuffs[$chunkName]['size'] += $bytes;
//            $this->memBuffs[$chunkName]['time'] = time();
            $this->memBuffs[$chunkName]['count']++;
//            $this->memBuffs[$chunkName]['buffer'] .= "$payload";
        }

        $this->memBuffs[$chunkName]['buffer'][] = $payload;
//        $this->memBuffs[$chunkName]['buffer'] .= $payload;

        if ($this->checkFlushLimit($chunkName, $this->memBuffs[$chunkName])) {
            if (!empty($this->memBuffs[$chunkName]) && !empty($this->memBuffs[$chunkName]['buffer'])) {
                $this->driver->flush($this->memBuffs[$chunkName]['buffer'], $chunkName, $namespace);

                $return = self::BUFFER_APPENDED_FLUSHED;

//                $this->memBuffs[$chunkName] = [];
                unset($this->memBuffs[$chunkName]);

//                gc_collect_cycles();
            }
        }

//        if (Helpers::memLimitReached()) {
//
//            SkipprLogger::info("Rotating buffer as memory limit has low headroom at ". BytesToHuman::toHuman(memory_get_usage(true), true));
//
//            $carrySize = 0;
//
//            // ensure we flush at least the largest file
//            foreach ($this->memBuffs as $name => $chunk) {
//                if ($chunk['size'] > $carrySize) {
//                    $carrySize = $chunk['size'];
//                    $flushChunkName = $name;
//                }
//            }
//
//            $this->driver->flush($this->memBuffs[$flushChunkName]['buffer'], $flushChunkName, $namespace);
//
//            unset($this->memBuffs[$flushChunkName]);
//
//            $return = self::BUFFER_APPENDED_FLUSHED;
//        }

        return $return;
    }

    public function flushAll(bool $finalize = false): void
    {

        foreach ($this->memBuffs as $chunkName => $buffer) {
            if ($finalize || $this->checkFlushLimit($chunkName, $this->memBuffs[$chunkName])
            ) {
                $namespace = $this->decodeChunkNamespace($chunkName);

                if (!empty($this->memBuffs[$chunkName]) && !empty($this->memBuffs[$chunkName]['buffer'])) {

                    $this->driver->flush($this->memBuffs[$chunkName]['buffer'], $chunkName, $namespace, $finalize);

//                    $this->memBuffs[$chunkName] = [];

                    unset($this->memBuffs[$chunkName]);

//                    gc_collect_cycles();
                }
            }

        }

//        $this->flushFinalised();
    }

    public function flushFinalised(bool $finalize = false): void
    {
        $file_list = glob($this->driver->bufferDir . '/buffer=' . $this->bufferName . '*&temp_part*');

        foreach ($file_list as $filename) {

            if ($finalize || $this->driver->checkFileBufferLimit($filename)
            ) {

                $namespace = $this->decodeFileNamespace($filename);
                $partition = $this->decodeFilePartition($filename);
                $timeBucket = $this->getFileChunkTime($filename);

                $bucketName = $this->encodeChunkName($namespace, $partition, $timeBucket);

                $this->driver->finalise($filename, $namespace, $bucketName);
            }
        }
    }

    public function checkFlushLimit(string $chunkName, array $chunk): bool
    {

        $result = false;

//        if (Helpers::memLimitReached()) {
//            SkipprLogger::info("Rotating buffer as memory limit has low headroom at ". BytesToHuman::toHuman(memory_get_usage(), true));
//            $result = true;
//        }

        if ($chunk['size'] > Config::$flushMemBufferBytes) {
            SkipprLogger::debug("Rotating memory buffer with size ". BytesToHuman::toHuman($chunk['size'], true));
            $result = true;
        }

        if ((time() - $chunk['time']) > Config::$flushMemBufferSeconds) {
            SkipprLogger::debug("Rotating memory buffer with ttl ". (time() - $chunk['time']) . " seconds");
            $result = true;
        }

        if ($chunk['count'] >= Config::$flushMemBufferRecords) {
            SkipprLogger::debug("Rotating memory buffer of ". $chunk['count'] . " records");
            $result = true;
        }

        if ($result) {
            $size = BytesToHuman::toHuman($chunk['size'], true);
            $time = (time() - $chunk['time']);
            $count = $chunk['count'];

            SkipprLogger::debug("Flushing memory buffer $chunkName of $size, $count records and age of $time seconds to disk");

            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

//            $this->statsd->increment("{$tenantId}.{$pipelineName}.flushed.records.current", $count);
//            $this->statsd->increment("{$tenantId}.{$pipelineName}.flushed.age.current", $time);
//            $this->statsd->increment("{$tenantId}.{$pipelineName}.flushed.bytes.current", $chunk['size']);

        }

        return $result;
    }
    

    public function eventTimeBucket(int $eventTime) : int
    {

        $bucketSeconds = false;

        if (Config::$eventTimeBucketDurationSeconds) {

            switch (Config::$eventTimeBucketDurationSeconds) {
                case 'year':
                    $bucketSeconds = 30240000;
                    break;
                case 'month':
                    $bucketSeconds = 259200; // 30 days
                    break;
                case 'day':
                    $bucketSeconds = 86400;
                    break;
                case 'hour':
                    $bucketSeconds = 3600;
                    break;
                case 'minute':
                    $bucketSeconds = 60;
                    break;
                default:
                    $bucketSeconds = false;
            }
        }

        if ($bucketSeconds) {

            $bucket = $eventTime - ($eventTime % $bucketSeconds);

        } else {

            $bucket = false;
        }

        return $bucket;
    }

    public function encodeChunkName(string $namespace = null, string $partition = null, int $timeBucket = 0): string
    {

        $chunks = [
            'buffer' => $this->bufferName,
            'namespace' => $namespace,
            'partition' => $partition
        ];

        if ($timeBucket) {
            $chunks['time'] = $timeBucket;
        }

        $chunkName = http_build_query($chunks);

        return $chunkName;
    }

    public function getChunkName($filename) : array
    {

        $startPos = strpos($filename, $this->bufferName) + strlen($this->bufferName);
//        $endPos = strpos($filename, '_finalised') - strlen('_finalised');
        $endPos = strrpos($filename, '&finalised', -1);
//        $encodedName = substr($filename, $startPos, -$endPos);
        $encodedName = substr($filename, $startPos, -43);
        $encodedName = trim($encodedName, '-');

        return explode('-', $encodedName);
    }

    public function getFileChunkTime($filename) : int
    {
        parse_str($filename, $array);

        if (isset($array['time'])) {
            return $array['time'];
        } else {
            return 0;
        }
    }

    public function decodeChunkTime($filename) : string
    {

        parse_str($filename, $array);

        if (isset($array['time'])) {
            $date_string = Carbon::createFromTimestamp($array['time'])->toIso8601String();

            SkipprLogger::debug("decoding buffer time $date_string file $filename");
        } else {
            $date_string = '';
        }



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
        $namespace = $array['namespace'] ?? '';

        SkipprLogger::debug("decoding buffer namespace $namespace from file $filename");

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
