<?php

namespace Skipprd\Buffers\BufferDrivers;

use Skipprd\Helpers;
use Skipprd\MachineToHuman\BytesToHuman;
use Skipprd\Serders\SerdersFactory;
use Skipprd\SkipprPack;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;
use Spatie\Async\Pool;

class FileBufferDriver implements BufferDriverInterface
{

    protected $skipprPack;

    /**
     * @var Spatie\Async\Pool
     */
    protected static $pool = null;

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

        $this->skipprPack = new SkipprPack();

        $this->setSerde(Config::$outputFormat);
    }

    public function setSerde(string $serde)
    {
        $this->serde = SerdersFactory::factory($serde);
    }

    protected function getPool(): void {
        if (self::$pool === null) {
            self::$pool = Pool::create()
            ->concurrency(1)
            ->timeout(600);
        }
    }

    /**
     * @return string
     */
    public function flushSerialise(
        array $memBuff,
        string $chunkName,
        string $namespace): void
    {

        SkipprLogger::info("Serializing memory buffer $chunkName to " . Config::$outputFormat . " output format");

        $this->flush($memBuff, $chunkName, $namespace);

        $file_list = glob($this->bufferDir . '/buffer=output*finalised-*');

        $this->getPool();

        if (!empty($file_list)) {
            foreach ($file_list as $bufferFile) {

                if (strpos($bufferFile, '.lock')) continue;
                if (strpos($bufferFile, '.checkpoint')) continue;

                $finalFilename = $this->bufferDir . '/' . $chunkName . '&complete=' . Helpers::randomPassword(32);

                if (FileBufferDriver::lock($finalFilename)) {

                    FileBufferDriver::lock($bufferFile);

                    $serde = $this->serde;

                    $schema = Config::$outputSchemas[$namespace];

                    self::$pool->add(function () use (
                        $bufferFile,
                        $finalFilename,
                        $namespace,
                        $serde,
                        $schema
                    ) {

                        $fpr = fopen($bufferFile, 'rb');

                        $serde->openWriter($finalFilename, $schema);

                        while (($buf = fgets($fpr)) !== false) {

                            try {
                                if ($payload = json_decode($buf, true)) {
                                    if (is_array($payload)) {
                                        $serde->serialize($payload);
                                    } else {
                                        throw new \Exception('no message in buffer');
                                    }

                                }

                            } catch (\Exception $e) {
                                // @todo !! don't long anywhere in event stream, we'll need to sample/limit these
                                // Still possible the file has been deleted just before we stat the size
//                                            SkipprLogger::error($e->getMessage());
//                                            SkipprLogger::error($e->getTraceAsString());
                            }

                        }

                        $serde->closeWriter();

                        fclose($fpr);

                        unlink($bufferFile);

                        FileBufferDriver::unlock($finalFilename);
                        FileBufferDriver::unlock($bufferFile);
                        echo "Flushed output file $finalFilename";
//                        SkipprLogger::info("Flushed output file $finalFilename");

//                        return $finalFilename;

                    });
//                    ->then(function (string $finalFilename) use ($bufferFile) {
//
//                        FileBufferDriver::unlock($finalFilename);
//                        FileBufferDriver::unlock($bufferFile);
//                        SkipprLogger::info("Flushed output file $finalFilename");
//                    })->catch(function (\Exception $e) use ($finalFilename, $bufferFile) {
//                        SkipprLogger::error($e->getMessage());
//                        FileBufferDriver::unlock($finalFilename);
//                        FileBufferDriver::unlock($bufferFile);
//                    });

                    SkipprLogger::info("Async flushing output file $bufferFile");

                    // wait for process to complete, as processInputBuffers() is
                    // holding input buffer open till it completes
                    self::$pool->wait();
                }
            }
        }
    }

    public function flush(
        array $memBuff,
        string $chunkName,
        string $namespace,
        bool $finalize = false
    ): void {

//        $schema = Config::$outputSchemas[$namespace];

//        if ($this->bufferName == 'input') {
            $filename = $this->bufferDir . '/' . $chunkName . '&finalised-' . time();
//        } else {
//            $filename = $this->bufferDir . '/' . $chunkName . '&temp_part';
//        }

        try {

//            SkipprLogger::debug("Requesting lock on $filename");

            if (FileBufferDriver::lock($filename)) { // acquire an exclusive lock

                SkipprLogger::info("Flushing memory buffer $chunkName to disk");

                $fh = fopen($filename, 'a+b');

                if (strpos($memBuff[0], "\n")) {
                    fputs($fh, implode('', $memBuff));
                } else {
                    fputs($fh, implode("\n", $memBuff) . "\n");
//                fputs($fh, $memBuff);
                }
                fflush($fh);
                fclose($fh);

                FileBufferDriver::unlock($filename);

                SkipprLogger::debug("Flushed buffer chunk $chunkName to disk");

            }
        } catch (\Exception $e) {
            echo $e->getMessage();

            echo $e->getTraceAsString();

            FileBufferDriver::unlock($filename);
            FileBufferDriver::unlock($filename);

            throw $e;
        }

    }

    public function finaliseFileBuffers(bool $force = false): void
    {
        $file_list = glob($this->bufferDir . '/buffer=' . $this->bufferName . '*&temp_part');

        if (!empty($file_list)) {
            foreach ($file_list as $filename) {

                if ($force || $this->checkFileBufferLimit($filename)) {

                    if ($this->lock($filename)) { // acquire an exclusive lock

                        $finalFilename = str_replace(
                                '&temp_part',
                                '&finalised',
                                $filename
                            ) . '=' . Helpers::randomPassword(32);

                        rename($filename, $finalFilename);

                        $this->destroy($filename);
                        $this->unlock($filename);
                    }
                }
            }
        }
    }

    public function checkFileBufferLimit(string $filename): bool
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

        clearstatcache();
        $updatedTime = filectime($filename);
//        $updatedDelta = time() - $updatedTime;
        $bytes = filesize($filename);
//        $humanSize = BytesToHuman::toHuman($bytes, true);

        $size = BytesToHuman::toHuman($bytes, true);
        $time = (time() - $updatedTime);
        $count = 'with';

        SkipprLogger::debug("Evaluating buffer file of $size, $count records and age of $time seconds: $filename");

        if ($bytes > Config::$flushBufferBytes) {
            SkipprLogger::debug("Rotating buffer file with size ". BytesToHuman::toHuman($bytes, true));
            $result = true;
        }

        if ((time() - $updatedTime) > Config::$flushBufferSeconds) {
            SkipprLogger::debug("Rotating buffer file with ttl ". (time() - $updatedTime) . " seconds");
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

            SkipprLogger::info("Finalising $this->bufferName buffer file of $size, $count records and age of $time seconds: $filename");

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
            $this->bufferDir . '/buffer=' . $this->bufferName . '*&complete=*',
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

        while (!$locked && $block) {
            if (@mkdir($chunkName . '.lock', 0777, true)) {
                $locked = true;
                SkipprLogger::debug("Created lock on file $chunkName");
            } else {
                usleep(500);
            }
        }

        return $locked;
    }

    public function unlockAll(): void
    {

        $file_list = glob($this->bufferDir . '/buffer=' . $this->bufferName . '*lock');

        if (!empty($file_list)) {
            foreach ($file_list as $lockFilename) {

                try {

                    if (strpos($lockFilename, 'complete')) {
                        $bufferFile = substr($lockFilename, 0, -5);
                        $bytes = filesize($bufferFile);
                        $humanSize = BytesToHuman::toHuman($bytes, true);
                        SkipprLogger::debug("Removing incomplete output file of sie $humanSize $bufferFile");
                        unlink($bufferFile); // remove locked file as we neven finished writing
                    }

                    SkipprLogger::debug("Unlocking file $lockFilename");
                    rmdir($lockFilename);
                } catch (\Exception $e) {
                    // Still possible the file has been deleted just before with stat the size
                    SkipprLogger::debug($e->getMessage());
                }
            }
        }
    }

    public function unlock(string $filename): bool
    {

        rmdir($filename . '.lock');

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

    public function destroyAll(): void
    {

        $file_list = glob($this->bufferDir . '/buffer=' . $this->bufferName . '*');

        SkipprLogger::info("Purging all buffer files: " . json_encode($file_list));

        if (!empty($file_list)) {
            foreach ($file_list as $filename) {

                try {
                    if (!is_dir($filename)) {
                        $this->destroy($filename);
                    }
                } catch (\Exception $e) {
                    // Still possible the file has been deleted just before with stat the size
                    SkipprLogger::debug($e->getMessage());
                }
            }
        }
    }

    public function destroy($filename): bool
    {

        try {
            SkipprLogger::debug("Destroying buffer file: " . $filename);

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

    public function readSkipprStream(string $data, string $functionName, array $metadata, callable $callback)
    {

        $current_index = 0;

        $bytes = strlen($data);

        $read = '';
        for ($i = $current_index; $i <= $bytes; $i++) {

            if ($read === '') {
                // get message framing
                $size = substr($data, $current_index, 4);
                $msgLen = @unpack('N', $size);

                if (!$msgLen) {
                    break;
                } // not sure why we sometimes get here...

                $readLen = $msgLen[1] + 4;

                if ($i !== 0) { // ensure we read whole message inc length
                    $i--;
                }

//                if ($readLen > ($bytes - $current_index)) {
//                    $remainingBytes = $bytes - $current_index;
//                    $remainingData = substr($data, $current_index);
//
//                    SkipprLogger::info("Returning partially read buffer where readlen $readLen is greater than remaining bytes $remainingBytes");
//                    SkipprLogger::info($remainingData);
//
//                    // @todo - WTF is breaking the message framing...
//                    if ($readLen > 250000) {
//                        // invalid readLen, too long
//                       return '';
//                    } else {
//                        return $remainingData;
//                    }
//                }

            }

            // `index is out of bounds` without this
            // we may have reached the end of the socket buffer
            // more data will likely arrive soon to concat onto our $read buffer
            if ($i < strlen($data)) {
                $read .= $data[$i];
            }


//            SkipprLogger::info("Read $read");

            if ($i === ($current_index + $readLen) && !empty($read)) {
//                SkipprLogger::info("Current msg start: $current_index");
//                SkipprLogger::info("Current pos: $i");
//                SkipprLogger::info("Outputting $read");


                try {

                    $this->skipprPack->create($read);

                    $current_index = $i;
                    $read = '';

                    $record = $this->skipprPack->decodeRecord();
//                    $payload = igbinary_unserialize($record);
//                    $payload = msgpack_unpack($record);
                    $payload = json_decode($record, true);

//                    SkipprLogger::info($record);

                    if (!empty($payload)) {
                        call_user_func($callback, $payload, $functionName,
                            $metadata);
                    }

                } catch (\Exception $e) {
                    $unpacked = unpack($read);
                    $unpackedStr = serialize($unpacked);
                    SkipprLogger::error("Failed to output SkipprPack received bytes: $unpackedStr");
                    SkipprLogger::error($e->getMessage());
                }


            }
        }

        if ($current_index < $bytes) {
            $remainingData = substr($data, $current_index); // remaining bytes

            return $remainingData;
        } else {
            return '';
        }
    }

    public function streamRead($fp, string $finalFilename, array $metadata, callable $callback)
    {

        while ($data = fread($fp, 8192)) {
            if ($data) {

                try {
                    $newData = $this->remainingData . $data;

                    $this->remainingData = $this->readSkipprStream($newData, $finalFilename, $metadata, $callback);

//                    if ($this->remainingData !== '') {
//                        SkipprLogger::info("Remaining data: $this->remainingData");
//                    }
                } catch (\Exception $e) {
                    SkipprLogger::error($e->getMessage());
                }
            } elseif (feof($fp)) {
                SkipprLogger::info('Client closed connection');
                fclose($fp);
            } else {
                SkipprLogger::info('No client input, sleeping');
                sleep(1);
            }
        }
    }

    /**
     * Simply closes a buffer file by renaming it with '&finalised' suffix
     * which prevents further append writes and indicates the buffer file is ready for output
     */
    public function finalise(string $filename, string $namespace, string $bucketName): void
    {

                try {

                    if (
                        file_exists($filename) // possible file removed by competing thread
                        && !strpos($filename, '.lock') // ignore locked files
                    ) {

                        if (FileBufferDriver::lock($filename, false)) { // acquire an exclusive lock
//                            $finalFilename = str_replace(
//                                    '&temp_part',
//                                    '&finalised',
//                                    $filename
//                                ) . '=' . Helpers::randomPassword(32);

                            $finalFilename = $this->bufferDir . '/' . $bucketName . '&complete=' . Helpers::randomPassword(32);

                            SkipprLogger::info("Unpacking buffer file $filename and serializing to " . Config::$outputFormat . " output format");

                            if (FileBufferDriver::lock($finalFilename)) { // acquire an exclusive lock

                                $fpr = fopen($filename, 'rb');

                                if (in_array(Config::$outputFormat,
                                        Config::$batchFormats)
                                    && Config::$enableDeadLetters) {

//                                    gc_enable();

                                    $this->serde->openWriter($finalFilename,
                                        Config::$outputSchemas[$namespace]);

                                    while (($buf = fgets($fpr)) !== false) {

                                        try {
//                                            if ($payload = igbinary_unserialize($buf)) {
//                                            if ($payload = msgpack_unpack($buf)) {
                                            if ($payload = json_decode($buf, true)) {
                                                if (is_array($payload)) {
                                                    $this->serde->serialize($payload);
                                                }

                                            }

                                        } catch (\Exception $e) {
                                            // @todo !! don't long anywhere in event stream, we'll need to sample/limit these
                                            // Still possible the file has been deleted just before we stat the size
//                                            SkipprLogger::error($e->getMessage());
//                                            SkipprLogger::error($e->getTraceAsString());
                                        }

                                    }

                                    $this->serde->closeWriter();

                                    unset($this->serde);

                                    gc_collect_cycles();
                                    gc_mem_caches();

                                    $this->setSerde(Config::$outputFormat);
//                                    gc_disable();


                                } else {

                                    $fpw = fopen($finalFilename, 'a+b');

                                    while (($buf = fgets($fpr)) !== false) {

                                        if ($payload = json_decode($buf, true)) {
                                            if (is_array($payload)) {
                                                $data = $this->serde->serialize($payload,
                                                    $finalFilename,
                                                    Config::$outputSchemas[$namespace]);
                                            }
                                        }

                                        fputs($fpw, $data . "\n");
                                    }

                                    fflush($fpw);
                                    fclose($fpw);
                                }

                                fflush($fpr);
                                fclose($fpr);

                                $updatedTime = filectime($finalFilename);
                                $updatedDelta = time() - $updatedTime;
                                $bytes = filesize($finalFilename);
                                $humanSize = BytesToHuman::toHuman($bytes,
                                    true);

                                SkipprLogger::info("Output file $finalFilename finalised at $humanSize and age of $updatedDelta seconds");

                                FileBufferDriver::unlock($finalFilename);
                            }

                            $this->destroy($filename);
                            FileBufferDriver::unlock($filename);

                        }
                    }
                } catch
                    (\Exception $e) {
                        // Still possible the file has been deleted just before we stat the size
                        SkipprLogger::error($e->getMessage());
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
