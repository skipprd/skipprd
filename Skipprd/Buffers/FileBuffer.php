<?php

namespace Skipprd\Buffers;

use Carbon\Carbon;
use Monolog\Registry;
use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\Helpers;
use Skipprd\MachineToHuman\BytesToHuman;
use Skipprd\Serders\SerdersFactory;
use Skipprd\Traits\Config;

class FileBuffer implements BufferInterface
{

    public $rows = 0;

    protected $name;

    protected $dataFp = null;

    protected $cpFp = null;

    protected $cpLine = null;
    
    protected $memBuffs = [];

    public $tempdir = '/tmp';

    public $flushBytes = 1000000; # 1MB

    public $flushFileSeconds = 600;

    public $flushMemBytes = 1000000; # 1MB

    public $flushMemSeconds = 30; # seconds

    private $serde;

    public function __construct(string $name, int $flushBytes = null)
    {

        $this->name = $name;

        if (!empty($flushBytes)) {
            $this->flushBytes = $flushBytes;
        }

        if (Config::$outputFormat != 'parquet') {
            
            $this->setSerde(Config::$outputFormat);
        }

    }

    public function setSerde(string $serde)
    {
        $this->serde = SerdersFactory::factory($serde, Config::$avroSchema);
    }

    public function flushAll(bool $force = false) {

        foreach ($this->memBuffs as $name => $buffer) {

            if ($force
                || $buffer['size'] > $this->flushMemBytes
                || $buffer['time'] < time() - $this->flushMemSeconds
            ) {

                $this->flush($name);
            }

        }
    }

    public function flush(string $name) : void {

        if (!empty($this->memBuffs[$name]['buffer'])) {

            $filename = $this->tempdir . '/' . $name . '-buffer';

            if (FileBuffer::lock($filename)) { // acquire an exclusive lock

                if (Config::$outputFormat == 'parquet') {

                    $converter = new AvroParquetSchemaConverter();
                    $parquetSchema = $converter->convert(Config::$avroSchema);

                    try {
                        $writer = new \Parquet();

                        $writer->create_writer($filename, $parquetSchema, 'snappy');

                        if (!empty($this->memBuffs[$name]) && !empty($this->memBuffs[$name]['buffer'])) {

//                            foreach ($this->memBuffs[$name]['buffer'] as $line) {

                                // deserailise intermediate serialisation from buffer

                            $separator = "\r\n";
                            $line = strtok($this->memBuffs[$name]['buffer'], $separator);

                            while ($line !== false) {
                                
                                $arr[] = json_decode($line, true);

                                $reslt = $writer->write($arr);

                                $arr = [];

                                $line = strtok($separator);


                            }



//                            }
                        }

                        $writer->close_writer();

                    } catch (\Exception $exception) {

                        var_export($parquetSchema);
                        print("\n");

                        var_export($arr);
                        print("\n");

                        print($exception->getMessage());

                        exit(1);
                    }

                    FileBuffer::unlock($filename);

                } else {

                    $fp = fopen($filename, 'a+');

                    fputs($fp, $this->memBuffs[$name]['buffer']);


//                    $serde = SerdersFactory::factory('json', Config::$avroSchema);

//                    foreach ($this->memBuffs[$name]['buffer'] as $line) {

//                        if (!empty($line)) {

//                        fputs($fp, $serde->serialize($line) . "\n");
//                        fputs($fp, json_encode($line) . "\n");
//                        }

//                    }

                    fflush($fp);            // flush output before releasing the lock

                    FileBuffer::unlock($filename);

                    FileBuffer::close($fp);
                }

                unset($this->memBuffs[$name]);
            }
        }

        $this->finalise();
    }

    public function append(array $message, bool $flush = false) : void {

//        if (Config::$outputFormat == 'parquet') {
            // must serialise parquet directly to file
            // so need intermediate serialisation (json) for buffer
            $message = json_encode((array)$message);
            $size = strlen($message) * 8;

//        } else {
//            $message = $this->serde->serialize($message);
//            $size = mb_strlen($message, '8bit');
//        }

        if (empty($this->memBuffs[$this->name])) {

            $this->memBuffs[$this->name]['size'] = $size;
            $this->memBuffs[$this->name]['time'] = time();
            $this->memBuffs[$this->name]['buffer'] = "$message" . "\n";

        } else {
            $this->memBuffs[$this->name]['size'] += $size;
            $this->memBuffs[$this->name]['time'] = time();
            $this->memBuffs[$this->name]['buffer'] .= "$message" . "\n";

        }

        if ($flush
            || $this->memBuffs[$this->name]['size'] > $this->flushMemBytes
            || $this->memBuffs[$this->name]['time'] < time() - 30
        ) {

            $this->flush($this->name);
//            $this->flushAll();
        }
    }

    public function commit()
    {

        if ($this->cpFp) { // may be at end of file and already closed handle

            $this->cpFp->ftruncate(0);
            $this->cpFp->fwrite($this->cpLine);
        }

    }

    public function stream() : string
    {
        
        // stream
        if ($this->dataFp == null) {

            $filename = $this->nextFile();

            if ($filename) {

                // checkpoint
                $checkpoint_filename = $filename . '.checkpoint';

                $this->cpFp = new \SplFileObject($checkpoint_filename, "a+");
                $this->cpLine = (int) $this->cpFp->fgets();

                Registry::skipprd()->info("Streaming file $filename from line $this->cpLine");

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

                        $payload = json_decode($payload, true);

                        return $this->serde->serialize($payload);
                        
                    }

                } catch (\Exception $e) {

                    Registry::skipprd()->error($e->getMessage());

                    return false; // exit to prevent buffer destroy
                }

            }

            $this->destroy($this->dataFp->getPathname());

            $this->dataFp = null;
            $this->cpFp = null;
            $this->cpLine = 0;
        }

        return $this->stream();
        
    }

    /**
     * stub, no partitioning on plain file buffer
     * 
     * @param $filename
     * @return string
     */
    public function decodeChunkTime($filename) : string {

        return '';

    }

    /**
     * stub, no partitioning on plain file buffer
     *
     * @param $filename
     * @return string
     */
    public function decodeChunkPartition($filename) : string {

        return '';
    }

    public function nextFile()
    {

        $filenames = glob($this->tempdir . '/' . "$this->name*-finalised-*", GLOB_NOSORT);

        usort( $filenames, function( $a, $b ) { return filemtime($a) - filemtime($b); } );

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

                // Still possible the file has been deleted just after the file_exists check
                Registry::skipprd()->debug($e->getMessage());

            }

        }

        return false;
    }


    public function lock(string $name, $block = true) : bool {
        
        $locked = false;

        // dir is more reliable than waiting for fstat on a file
        if (@mkdir($name . '.lock',0777)) {
            $locked = true;
        }

        while (!$locked && $block) {

            if (@mkdir($name . '.lock',0777)) {
                $locked = true;
            } else {

                sleep(1);
            }
        }

        return $locked;
    }

    public function unlockAll() : void
    {
        $file_list = glob($this->tempdir . '/' . $this->name . '*lock');

        if (!empty($file_list)) {

            foreach ($file_list as $filename) {

                try {

                    rmdir($filename);

                } catch (\Exception $e) {

                    // Still possible the file has been deleted just before with stat the size
                    Registry::skipprd()->debug($e->getMessage());

                }

            }
        }
    }

    public function unlock(string $name) : bool {

        rmdir($name . '.lock');

        return true;
    }

    public function close($fp) : bool {

        if (fclose($fp)) { // release the lock)
            return true;
        } else {
            return false;
        }

    }

    public function destroy($filename) : bool {

        try {

            Registry::skipprd()->debug("Destroying finished buffer file: " . $filename);

            unlink($filename);
            @unlink($filename . '.checkpoint');
            self::unlock($filename);

            return true;

        } catch (\Exception $e) {
            Registry::skipprd()->error("Failed to destroy buffer");
            Registry::skipprd()->error($e->getMessage());

            return false;
        }


    }

    public function finalise($force = false) :void {

        $file_list = glob($this->tempdir . '/*' . $this->name . '*-buffer*');

        if (!empty($file_list)) {

           foreach ($file_list as $filename) {

               try {
                   // possible file removed by competing thread
                   if (!file_exists($filename)) continue;

                   // ignore locked files
                   if (strpos($filename, '.lock')) continue;

                   $updatedTime = filectime($filename);
                   $updatedDelta = time() - $updatedTime;

                   $bytes = filesize($filename);

                   if (FileBuffer::lock($filename)) { // acquire an exclusive lock

//                       Registry::skipprd()->debug("bytes: " . $bytes);
//                       Registry::skipprd()->debug("flushBytes: " . $this->flushBytes);

                       if ($bytes >= $this->flushBytes || $updatedDelta > $this->flushFileSeconds || $force) {

                           $newFilename = str_replace('buffer', 'finalised',
                               $filename);

                           rename($filename, $newFilename . '-' . Helpers::randomPassword(32));

                           if (!$force) {

                               $humanSize = BytesToHuman::toHuman($bytes);
                               Registry::skipprd()->debug("Buffer file $filename rotated at $humanSize and change time delta $updatedDelta");
                           }
                       }

                       FileBuffer::unlock($filename);
                    }

               } catch (\Exception $e) {

                   // Still possible the file has been deleted just before with stat the size
                   Registry::skipprd()->debug($e->getMessage());

               }
           }
        }
    }

    public function bufferGetNoFiles() : int
    {

        $file_list = glob($this->tempdir . '/' . "$this->name*");

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

    public function bufferGetBytes() : int
    {

        $bytes = 0;

        $file_list = glob($this->tempdir . '/' . "$this->name*");

        if (!empty($file_list)) {

            foreach ($file_list as $filename) {

                try {

                    // possible file removed by competing thread
                    if (!file_exists($filename)) continue;

                    // ignore locked files
                    if (strpos($filename, '.lock')) continue;

                    $bytes += filesize($filename);

                } catch (\Exception $e) {

                    // Still possible the file has been deleted just before with stat the size
                    Registry::skipprd()->debug($e->getMessage());

                }
            }
        }

        return $bytes;
    }

    public function bufferGetNoLines() : int
    {

        $file_list = glob($this->tempdir . '/' . "$this->name*");

        $lines = 0;

        if (!empty($file_list)) {

            foreach ($file_list as $filename) {

                try {

                    // possible file removed by competing thread
                    if (!file_exists($filename)) continue;

                    // ignore locked files
                    if (strpos($filename, '.lock')) continue;

                    $fp = fopen($filename, "rb");

                    while (!feof($fp)) {
                        $lines += substr_count(fread($fp, 8192), "\n");
                    }


                } catch (\Exception $e) {

                    // Still possible the file has been deleted just before with stat the size
                    Registry::skipprd()->debug($e->getMessage());

                }
            }
        }

        return $lines;
    }
}
