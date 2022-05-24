<?php

namespace Skipprd\Buffers\BufferDrivers;

use Carbon\Carbon;
use Monolog\Registry;
use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Helpers;
use Skipprd\MachineToHuman\BytesToHuman;
use Skipprd\Serders\SerdersFactory;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class FileBufferDriver implements BufferDriverInterface
{

    public $rows = 0;

    protected $bufferName;

    protected $dataFp = null;

    protected $cpFp = null;

    protected $cpLine = null;

    public $bufferDir = '';

    public $serde;

    public function __construct(string $bufferName)
    {

        $this->bufferName = $bufferName;

        $this->bufferDir = Config::$dataDir . '/buffer';

        @mkdir($this->bufferDir, 0777, true);

        $this->setSerde(Config::$outputFormat);
    }

    public function setSerde(string $serde)
    {
        $this->serde = SerdersFactory::factory($serde);
    }

    public function flush(
        array $memBuff,
        string $chunkName,
        string $namespace
    ): void {

        $schema = Config::$outputSchemas[$namespace];

        $filename = $this->bufferDir . '/' . $chunkName . '&temp_part';

        try {
            if (FileBufferDriver::lock($filename)) { // acquire an exclusive lock
                if (in_array(Config::$outputFormat, Config::$batchFormats)
                    && Config::$enableDeadLetters) {
                    $this->serde->serialize($memBuff, $filename, $schema);

                    FileBufferDriver::unlock($filename);

                    $this->finalise();
                } else {
                    $fp = fopen($filename, 'a+');

                    foreach ($memBuff as $buf) {
                        fputs(
                            $fp,
                            $this->serde->serialize($buf, $schema) . "\n"
                        );
                    }

                    fflush($fp);            // flush output before releasing the lock

                    FileBufferDriver::unlock($filename);

                    FileBufferDriver::close($fp);
                }
            }
        } catch (\Exception $e) {
            echo $e->getMessage();

            echo $e->getTraceAsString();
            fflush($fp);            // flush output before releasing the lock

            FileBufferDriver::unlock($filename);

            FileBufferDriver::close($fp);

            throw $e;
        }

        SkipprLogger::debug("Flushed buffer chunk $chunkName");

        $this->finalise();
    }

//    public function append(array $message, bool $flush = false) : void {
//
////        if (Config::$outputFormat == 'parquet') {
//            // must serialise parquet directly to file
//            // so need intermediate serialisation (json) for buffer
//            $messageStr = json_encode((array)$message);
//            $size = strlen($messageStr) * 8;
//
////        } else {
////            $message = $this->serde->serialize($message);
////            $size = mb_strlen($message, '8bit');
////        }
//
//        if (empty($this->memBuffs[$this->name])) {
//
//            $this->memBuffs[$this->name]['size'] = $size;
//            $this->memBuffs[$this->name]['time'] = time();
////            $this->memBuffs[$this->name]['buffer'] = "$message" . "\n";
//            $this->memBuffs[$this->name]['buffer'][] = $message;
//
//        } else {
//            $this->memBuffs[$this->name]['size'] += $size;
//            $this->memBuffs[$this->name]['time'] = time();
////            $this->memBuffs[$this->name]['buffer'] .= "$message" . "\n";
//            $this->memBuffs[$this->name]['buffer'][] = $message;
//
//        }
//
//        if ($flush
//            || $this->memBuffs[$this->name]['size'] > $this->flushMemBytes
////            || $this->memBuffs[$this->name]['time'] < time() - $this->flushMemSeconds
//        ) {
//
//            $this->flush($this->name);
////            $this->flushAll();
//        }
//    }

//    public function commit()
//    {
//
//        if ($this->cpFp) { // may be at end of file and already closed handle
//
//            $this->cpFp->ftruncate(0);
//            $this->cpFp->fwrite($this->cpLine);
//        }
//
//    }

    public function streamGetCurrentBufferFile()
    {

        return $this->dataFp->getBasename();
    }

    /**
     * @return bool|string
     */
    public function stream()
    {

        // stream
        if ($this->dataFp == null) {
            $filename = $this->nextFile();

            if ($filename) {
                // checkpoint
                $checkpoint_filename = $filename . '.checkpoint';

                $this->cpFp = new \SplFileObject($checkpoint_filename, "a+");
                $this->cpLine = (int) $this->cpFp->fgets();

                SkipprLogger::info("Streaming buffer file $filename from line $this->cpLine");

                $this->dataFp = new \SplFileObject($filename, "a+");

                $this->dataFp->seek($this->cpLine);
            } else {
                return false;
            }
        }

        if ($this->dataFp) {
            if (!$this->dataFp->eof()) {
                try {
                    if ($payload = $this->dataFp->current()) {
                        $this->dataFp->next();

                        $this->cpLine++;

//                        $payload = json_decode($payload, true);
//
//                        return $this->serde->serialize($payload);

//                        return $this->serde->deserialize($payload);
                        return $payload;
                    }
                } catch (\Exception $e) {
                    SkipprLogger::error($e->getMessage());

                    return false; // exit to prevent buffer destroy
                }
            }

            $this->destroy($this->dataFp->getPathname());

            $this->dataFp = null;
            $this->cpFp = null;
            $this->cpLine = 0;
        }

//        return $this->stream(); // determined by env POLL_INTERVAL_SECONDS
    }

//    /**
//     * stub, no partitioning on plain file buffer
//     *
//     * @param $filename
//     * @return string
//     */
//    public function decodeChunkTime($filename) : string {
//
//        return '';
//
//    }
//
//    /**
//     * stub, no partitioning on plain file buffer
//     *
//     * @param $filename
//     * @return string
//     */
//    public function decodeFileNamespaceAndPartition($filename) : string {
//
//        return '';
//    }

    /**
     * @return bool|string
     */
    public function nextFile()
    {

        $filenames = glob(
            $this->bufferDir . '/buffer=' . $this->bufferName . '*&finalised=*',
            GLOB_NOSORT
        );

        usort($filenames, function ($a, $b) {
            return filemtime($a) - filemtime($b);
        });

        foreach ($filenames as $filename) {
            try {
                // possible file removed by competing thread
                if (!file_exists($filename)) {
                    continue;
                }

                // ignore locked files
                if (strpos($filename, '.lock')) {
                    continue;
                }
                if (strpos($filename, '.checkpoint')) {
                    continue;
                }

                if (FileBufferDriver::lock($filename, false)) {
                    return $filename;
                }
            } catch (\Exception $e) {
                // Still possible the file has been deleted just after the file_exists check
                SkipprLogger::debug($e->getMessage());
            }
        }

        return false;
    }


    public function lock(string $chunkName, $block = true): bool
    {

        $locked = false;

        // dir is more reliable than waiting for fstat on a file
        if (@mkdir($chunkName . '.lock', 0777, true)) {
            $locked = true;
            SkipprLogger::debug("Created lock on buffer chunk $chunkName");
        }

        // @todo - check update time of lock file and force delete if it's past an expire time
        while (!$locked && $block) {
            if (@mkdir($chunkName . '.lock', 0777, true)) {
                $locked = true;
                SkipprLogger::debug("Created lock on buffer chunk $chunkName");
            } else {
                sleep(1);
            }
        }

        return $locked;
    }

    public function unlockAll(): void
    {
        $file_list = glob($this->bufferDir . '/buffer=' . $this->bufferName . '*lock');

        if (!empty($file_list)) {
            foreach ($file_list as $filename) {
                try {
                    rmdir($filename);
                } catch (\Exception $e) {
                    // Still possible the file has been deleted just before with stat the size
                    SkipprLogger::debug($e->getMessage());
                }
            }
        }
    }

    public function unlock(string $chunkName): bool
    {

        rmdir($chunkName . '.lock');

        return true;
    }

    public function close($fp): bool
    {

        if (fclose($fp)) { // release the lock)
            return true;
        } else {
            return false;
        }
    }

    public function destroy($filename): bool
    {

        try {
            SkipprLogger::debug("Destroying finished buffer file: " . $filename);

            unlink($filename);
            @unlink($filename . '.checkpoint');
            self::unlock($filename);

            return true;
        } catch (\Exception $e) {
            SkipprLogger::error("Failed to destroy buffer");
            SkipprLogger::error($e->getMessage());

            return false;
        }
    }

    /**
     * Simply closes a buffer file by renaming it with '&finalised' suffix
     * which prevents further append writes and indicates the buffer file is ready for output
     */
    public function finalise(): void
    {

        $file_list = glob($this->bufferDir . '/buffer=' . $this->bufferName . '*&temp_part*');

        if (!empty($file_list)) {
            foreach ($file_list as $filename) {
                try {
                    // possible file removed by competing thread
                    if (!file_exists($filename)) {
                        continue;
                    }

                    // ignore locked files
                    if (strpos($filename, '.lock')) {
                        continue;
                    }

                    if (FileBufferDriver::lock($filename)) { // acquire an exclusive lock
                        $finalFilename = str_replace(
                            '&temp_part',
                            '&finalised',
                            $filename
                        ) . '=' . Helpers::randomPassword(32);

                        rename(
                            $filename,
                            $finalFilename
                        );

                        $updatedTime = filectime($finalFilename);
                        $updatedDelta = time() - $updatedTime;
                        $bytes = filesize($finalFilename);
                        $humanSize = BytesToHuman::toHuman($bytes, true);

                        SkipprLogger::debug("Buffer file $filename finalised at $humanSize and age of $updatedDelta seconds");

                        FileBufferDriver::unlock($filename);
                    }
                } catch (\Exception $e) {
                    // Still possible the file has been deleted just before we stat the size
                    SkipprLogger::debug($e->getMessage());
                }
            }
        }
    }

    public function bufferGetNoFiles(): int
    {

        $file_list = glob($this->bufferDir . '/buffer=' . "$this->bufferName*");

        $i = 0;

        foreach ($file_list as $filename) {
            // possible file removed by competing thread
            if (!file_exists($filename)) {
                continue;
            }

            // ignore locked files
            if (strpos($filename, '.lock')) {
                continue;
            }

            $i++;
        }

        return $i;
    }

    public function bufferGetBytes(): int
    {

        $bytes = 0;

        $file_list = glob($this->bufferDir . '/buffer=' . "$this->bufferName*");

        if (!empty($file_list)) {
            foreach ($file_list as $filename) {
                try {
                    // possible file removed by competing thread
                    if (!file_exists($filename)) {
                        continue;
                    }

                    // ignore locked files
                    if (strpos($filename, '.lock')) {
                        continue;
                    }

                    $bytes += filesize($filename);
                } catch (\Exception $e) {
                    // Still possible the file has been deleted just before with stat the size
                    SkipprLogger::debug($e->getMessage());
                }
            }
        }

        return $bytes;
    }

    public function bufferGetNoLines(): int
    {

        $file_list = glob($this->bufferDir . '/buffer=' . "$this->bufferName*");

        $lines = 0;

        if (!empty($file_list)) {
            foreach ($file_list as $filename) {
                try {
                    // possible file removed by competing thread
                    if (!file_exists($filename)) {
                        continue;
                    }

                    // ignore locked files
                    if (strpos($filename, '.lock')) {
                        continue;
                    }

                    $fp = fopen($filename, "rb");

                    while (!feof($fp)) {
                        $lines += substr_count(fread($fp, 8192), "\n");
                    }
                } catch (\Exception $e) {
                    // Still possible the file has been deleted just before with stat the size
                    SkipprLogger::debug($e->getMessage());
                }
            }
        }

        return $lines;
    }
}
