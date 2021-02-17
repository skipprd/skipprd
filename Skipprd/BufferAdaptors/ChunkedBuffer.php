<?php


namespace Skipprd\BufferAdaptors;


use App\Helpers\BytesToHuman;
use Carbon\Carbon;
use Illuminate\Support\Facades\Log;
use Illuminate\Support\Str;

class ChunkedBuffer extends FileBuffer
{

    protected $memBuffs = [];

    protected static $eventTimeBucketDurationSeconds = 600;
//    protected static $eventTimeBucketDurationSeconds = 3600;
//    protected static $eventTimeBucketDurationSeconds = 86400;


    public function append(string $payload, bool $flush = false, int $eventTime = 0, string $partition = null) : void {

        $timeBucket = $this->eventTimeBucket($eventTime);

        $name = $this->encodeChunkName($partition, $timeBucket);

//        if (empty($this->memBuffs[$name])) {
//
//            $this->memBuffs[$name]['size'] = 0;
//            $this->memBuffs[$name]['buffer'] = "";
//
//        }

        if (empty($this->memBuffs[$name])) {

            $this->memBuffs[$name]['size'] = mb_strlen($payload) * 8;
            $this->memBuffs[$name]['time'] = time();
            $this->memBuffs[$name]['buffer'] = "$payload";

        } else {
            $this->memBuffs[$name]['size'] += mb_strlen($payload) * 8;
            $this->memBuffs[$name]['time'] = time();
            $this->memBuffs[$name]['buffer'] .= "$payload";

        }

        if ($flush
            || $this->memBuffs[$name]['size'] > $this->flushMemBytes
//            || $this->memBuffs[$name]['time'] < time() - 30
        ) {

            $this->flush($name);
//            $this->flushAll();
        }

    }

    public function eventTimeBucket(int $eventTime) : int {

        $bucket = $eventTime - ($eventTime % self::$eventTimeBucketDurationSeconds);

        return $bucket;

    }

    public function encodeChunkName($partition, $timeBucket)
    {
        return implode('-', [$this->name, $timeBucket, $partition]);
    }

    private function getChunkName($filename) : array {

        $startPos = strpos($filename, $this->name) + strlen($this->name);
        $endPos = strpos($filename, 'finalised') - strlen('finalised');
        $encodedName = substr($filename, $startPos, -42);
        $encodedName = trim($encodedName, '-');

        return explode('-', $encodedName);

    }

    public function decodeChunkTime($filename) : string {

        $parts = $this->getChunkName($filename);

        $timestamp = array_shift($parts);

        $date_string = Carbon::createFromTimestamp($timestamp)->format('Y-m-d');

        return 'dt=' . $date_string;

    }

    public function decodeChunkPartition($filename) : string {

        $parts = $this->getChunkName($filename);

        if (is_numeric($parts[0])) {
            array_shift($parts);
        }

        return trim(implode('/', $parts), '/');
    }

    public function lockedRead()
    {

        $filenames = glob($this->tempdir . '/' . "$this->name*-finalised-*", GLOB_NOSORT);

        asort($filenames);

        foreach ($filenames as $filename) {

            try {

                // possible file removed by competing thread
                if (!file_exists($filename)) continue;

                // ignore locked files
                if (strpos($filename, '.lock')) continue;
                if (strpos($filename, '.checkpoint')) continue;

                if (FileBuffer::lock($filename, false)) {

                    return $filename;

                }

            } catch (\Exception $e) {

                // Still possible the file has been deleted just before with stat the size
                $this->log->debug($e->getMessage());

            }

        }

        return false;
    }

}
