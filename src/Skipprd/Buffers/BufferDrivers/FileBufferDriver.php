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
        string $namespace,
        bool $finalize = false
    ): void {

//        $schema = Config::$outputSchemas[$namespace];

        $filename = $this->bufferDir . '/' . $chunkName . '&temp_part';

        try {

            SkipprLogger::debug("Requesting lock on $filename");

            if (FileBufferDriver::lock($filename)) { // acquire an exclusive lock
//                if (in_array(Config::$outputFormat, Config::$batchFormats)
//                    && Config::$enableDeadLetters) {
//                    $this->serde->serialize($memBuff, $filename, $schema);
//
//                    FileBufferDriver::unlock($filename);
//
//                    $this->finalise();
//                } else {
//                    $fp = fopen($filename, 'a+');
//
//                    foreach ($memBuff as $buf) {
//                        fputs(
//                            $fp,
//                            $this->serde->serialize($buf, $schema) . "\n"
//                        );
//                    }

//                $fp = fopen($filename, 'a+');

                SkipprLogger::debug("Flushing buffer $chunkName to disk");

                $fh = fopen($filename, 'a+');
                fputs($fh, implode(PHP_EOL, $memBuff));
                fflush($fh);
                fclose($fh);

//                file_put_contents($filename, implode(PHP_EOL, $memBuff), FILE_APPEND);

//                foreach ($memBuff as $buf) {
//                    fputs($fp, "$buf\n");
//                }

//                    fflush($fp);            // flush output before releasing the lock

                    FileBufferDriver::unlock($filename);

//                    FileBufferDriver::close($fp);

                }
//            }
        } catch (\Exception $e) {
            echo $e->getMessage();

            echo $e->getTraceAsString();
//            fflush($fp);            // flush output before releasing the lock

            FileBufferDriver::unlock($filename);

//            FileBufferDriver::close($fp);

            throw $e;
        }

        SkipprLogger::debug("Flushed buffer chunk $chunkName");

        if ($finalize || $this->checkFlushLimit($filename)) {
            $this->finalise($filename, $namespace);
        }

    }

    public function checkFlushLimit(string $filename): bool
    {

        $result = false;

//        if (Helpers::memLimitReached()) {
//            SkipprLogger::info("Rotating buffer as memory limit has low headroom at ". BytesToHuman::toHuman(memory_get_usage(), true));
//            $result = true;
//        }

        // possible file removed by competing thread
        if (!file_exists($filename)) return $result;

        // ignore locked files
        if (strpos($filename, '.lock')) return $result;
        if (strpos($filename, '.checkpoint')) return $result;

        $updatedTime = filectime($filename);
//        $updatedDelta = time() - $updatedTime;
        $bytes = filesize($filename);
//        $humanSize = BytesToHuman::toHuman($bytes, true);

        if ($bytes > Config::$flushBufferBytes) {
            SkipprLogger::debug("Rotating output file with size ". BytesToHuman::toHuman($bytes, true));
            $result = true;
        }

        if ((time() - $updatedTime) > Config::$flushBufferSeconds) {
            SkipprLogger::debug("Rotating output file with ttl ". (time() - $updatedTime) . " seconds");
            $result = true;
        }

//        if ($chunk['count'] >= Config::$flushBufferRecords) {
//            SkipprLogger::debug("Rotating buffer of ". $chunk['count'] . " records");
//            $result = true;
//        }

        if ($result) {
            $size = BytesToHuman::toHuman($bytes, true);
            $time = (time() - $updatedTime);
            $count = 'with';

            SkipprLogger::info("Finalising output file of $size, $count records and age of $time seconds");

            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

//            $this->statsd->increment("{$tenantId}.{$pipelineName}.flushed.records.current", $count);
//            $this->statsd->increment("{$tenantId}.{$pipelineName}.flushed.age.current", $time);
//            $this->statsd->increment("{$tenantId}.{$pipelineName}.flushed.bytes.current", $chunk['size']);

        }

        return $result;
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

                SkipprLogger::info("Streaming output file $filename from line $this->cpLine");

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

//        return $this->stream(); // determined by env DATA_SOURCE_POLL_INTERVAL_SECONDS
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
            SkipprLogger::debug("Created lock on file $chunkName");
        }

        // @todo - check update time of lock file and force delete if it's past an expire time
        while (!$locked && $block) {
            if (@mkdir($chunkName . '.lock', 0777, true)) {
                $locked = true;
                SkipprLogger::debug("Created lock on file $chunkName");
            } else {
                sleep(1);
            }
        }

        return $locked;
    }

    public function unlockAll(): void
    {

        $file_list = glob($this->bufferDir . '/buffer=' . $this->bufferName . '*lock');

        SkipprLogger::info("Removing all buffer locks: " . json_encode($file_list));

        if (!empty($file_list)) {
            foreach ($file_list as $filename) {

                try {
                    SkipprLogger::debug("Unlocking file $filename");
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
    public function finalise(string $filename, string $namespace): void
    {

//        $file_list = glob($this->bufferDir . '/buffer=' . $this->bufferName . '*&temp_part*');

//        if (!empty($file_list)) {
//            foreach ($file_list as $filename) {
                try {

                    if (
                        file_exists($filename) // possible file removed by competing thread
                        && !strpos($filename, '.lock') // ignore locked files
                    ) {

                        SkipprLogger::info("Finalising output file $filename");

                        if (FileBufferDriver::lock($filename)) { // acquire an exclusive lock
                            $finalFilename = str_replace(
                                    '&temp_part',
                                    '&finalised',
                                    $filename
                                ) . '=' . Helpers::randomPassword(32);

//                        rename(
//                            $filename,
//                            $finalFilename
//                        );


                            $fpr = fopen($filename, 'r');

                            if (in_array(Config::$outputFormat,
                                    Config::$batchFormats)
                                && Config::$enableDeadLetters) {

                                $this->serde->openWriter($finalFilename, Config::$outputSchemas[$namespace]);

                                SkipprLogger::info("Unpacking buffer file $filename and serializing to " . Config::$outputFormat . " output format");

                                while (($buf = fgets($fpr)) !== false) {

                                    $this->serde->serialize(msgpack_unpack($buf),
                                        $finalFilename,
                                        Config::$outputSchemas[$namespace]);
                                }

                                $this->serde->closeWriter();

                            } else {

                                $fpw = fopen($finalFilename, 'a+');
                                while (($buf = fgets($fpr)) !== false) {

                                    fputs(
                                        $fpw,
                                        $this->serde->serialize(msgpack_unpack($buf),
                                            $finalFilename,
                                            Config::$outputSchemas[$namespace]) . "\n"
                                    );
                                }

                                fflush($fpw);
                                fclose($fpw);
                            }

                            fflush($fpr);
                            fclose($fpr);

                            $updatedTime = filectime($finalFilename);
                            $updatedDelta = time() - $updatedTime;
                            $bytes = filesize($finalFilename);
                            $humanSize = BytesToHuman::toHuman($bytes, true);

                            SkipprLogger::debug("Output file $filename finalised at $humanSize and age of $updatedDelta seconds");

                            $this->destroy($filename);
                            FileBufferDriver::unlock($filename);
                        }
                    }
                } catch
                    (\Exception $e) {
                        // Still possible the file has been deleted just before we stat the size
                        SkipprLogger::debug($e->getMessage());
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

                    fclose($fp);

                } catch (\Exception $e) {
                    // Still possible the file has been deleted just before with stat the size
                    SkipprLogger::debug($e->getMessage());
                }
            }
        }

        return $lines;
    }
}
