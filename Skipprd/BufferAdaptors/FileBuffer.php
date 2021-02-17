<?php

namespace Skipprd\BufferAdaptors;

use Illuminate\Support\Facades\Log;
use Skipprd\Helpers;
use Skipprd\MachineToHuman\BytesToHuman;

class FileBuffer implements Buffer
{

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

    public function __construct(string $name, int $flushBytes = null)
    {

        $this->name = $name;

        if (!empty($flushBytes)) {
            $this->flushBytes = $flushBytes;
        }

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

                $fp = fopen($filename, 'a+');

                fputs($fp, $this->memBuffs[$name]['buffer']);

                fflush($fp);            // flush output before releasing the lock

                FileBuffer::unlock($filename);

                FileBuffer::close($fp);

                unset($this->memBuffs[$name]);
            }
        }

        $this->finalise();
    }

    public function append(string $message, bool $flush = false) : void {

        if (empty($this->memBuffs[$this->name])) {

            $this->memBuffs[$this->name]['size'] = mb_strlen($message) * 8;
            $this->memBuffs[$this->name]['time'] = time();
            $this->memBuffs[$this->name]['buffer'] = "$message";

        } else {
            $this->memBuffs[$this->name]['size'] += mb_strlen($message) * 8;
            $this->memBuffs[$this->name]['time'] = time();
            $this->memBuffs[$this->name]['buffer'] .= "$message";

        }

        if ($flush
            || $this->memBuffs[$this->name]['size'] > $this->flushMemBytes
//            || $this->memBuffs[$this->name]['time'] < time() - 30
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

    public function stream()
    {
        
        // stream
        if ($this->dataFp == null) {

            $filename = $this->lockedRead();

            if ($filename) {

                // checkpoint
                $checkpoint_filename = $filename . '.checkpoint';

                $this->cpFp = new \SplFileObject($checkpoint_filename, "a+");
                $this->cpLine = (int) $this->cpFp->fgets();

                $this->log->info("Streaming file $filename from line $this->cpLine");

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

                        return $payload;
                    }

                } catch (\Exception $e) {

                    $this->log->error($e->getMessage());

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

    public function lockedRead()
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

                // Still possible the file has been deleted just before with stat the size
                $this->log->debug($e->getMessage());

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
                    $this->log->debug($e->getMessage());

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

            $this->log->debug("Destroying finished buffer file: " . $filename);

            unlink($filename);
            @unlink($filename . '.checkpoint');
            self::unlock($filename);

            return true;

        } catch (\Exception $e) {
            $this->log->error("Failed to destroy buffer");
            $this->log->error($e->getMessage());

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

                       if ($bytes >= $this->flushBytes || $updatedDelta > $this->flushFileSeconds || $force) {

                           $newFilename = str_replace('buffer', 'finalised',
                               $filename);

                           rename($filename, $newFilename . '-' . Helpers::randomPassword(32));

                           if (!$force) {

                               $humanSize = BytesToHuman::toHuman($bytes);
                               $this->log->debug("Buffer file $filename rotated at $humanSize and change time delta $updatedDelta");
                           }
                       }

                       FileBuffer::unlock($filename);
                    }

               } catch (\Exception $e) {

                   // Still possible the file has been deleted just before with stat the size
                   $this->log->debug($e->getMessage());

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
                    $this->log->debug($e->getMessage());

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
                    $this->log->debug($e->getMessage());

                }
            }
        }

        return $lines;
    }
}
