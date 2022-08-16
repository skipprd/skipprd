<?php


namespace Skipprd\Traits;

use Skipprd\SkipprPack;

trait SkipprStream
{

    protected $skipprPack;

    protected $remainingData = '';

    protected $connAttempts = 0;

    protected $connAttemptsMax = 6;

    protected $tcpBufferSize = 1000000;


    public function streamSend(string $message, $flags = null)
    {

        $attemptCount = 0;

        try {
            $attemptCount++;

            $exitCode = @stream_socket_sendto($this->sock, $message, $flags);
            // Don't set STREAM_OOB flag, it fills buffer

            $bytes = strlen($message);

            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

//            $this->statsd->increment("{$tenantId}.{$pipelineName}.ingest.bytes.current", $bytes);

            // sometimes bytes written, sometime success exit code
            // might be platform based, someone else suggest due to blockking/non-blocking
            $success = ($exitCode === 0 || $exitCode === $bytes);

            if (!$success) {
//                SkipprLogger::debug("Stream return code: $exitCode");

                // @codeCoverageIgnoreStart
//                if (\function_exists('socket_import_stream')) {
//                    // actual socket errno and errstr can be retrieved with ext-sockets on PHP 5.4+
//                    $socket = \socket_import_stream($this->sock);
//                    $errno = \socket_get_option($socket, \SOL_SOCKET,
//                        \SO_ERROR);
//                    $errstr = \socket_strerror($errno);
//                } elseif (\PHP_OS === 'Linux') {
                // Linux reports socket errno and errstr again when trying to write to the dead socket.
                // Suppress error reporting to get error message below and close dead socket before rejecting.

                // @todo - don't do this, we end up with a packet that we can't SkipprPack decode at the outptu
                // This is only known to work on Linux, Mac and Windows are known to not support this.
//                        @\fwrite($this->conn, \PHP_EOL);
//                        $error = \error_get_last();
//
//                        // fwrite(): send of 2 bytes failed with errno=111 Connection refused
//                        \preg_match('/errno=(\d+) (.+)/', $error['message'], $m);
//                        $errno = isset($m[1]) ? (int) $m[1] : 0;
//                        $errstr = isset($m[2]) ? $m[2] : $error['message'];


//                        \fclose($this->conn);

//                }

//                SkipprLogger::debug("Stream error: $errstr code: $errno");

                throw new \Exception('Stream to output failed');
            }
        } catch (\Exception $e) {
            if (extension_loaded('newrelic')) {
                newrelic_ignore_transaction();
            }

            try { // possibly a socket to nowhere

                if ($this->connAttempts < $this->connAttemptsMax) {
                    sleep($this->connAttempts);

                    SkipprLogger::info("Output stream not available, reconnecting and retransmitting");

                    $this->streamConnect();

                    $this->streamSend($message, $flags);
                }

            } catch (\Exception $e) {

                SkipprLogger::error("Output stream not available, shutting down");

                $this->shutdown(1000);
            }
        }
    }

    /**
     * @param callable $emitMessageCallback
     * @param callable $postReadCallback
     */
    public function streamRead(
        callable $emitMessageCallback,
        callable $postReadCallback
    ) {
        try {

            $this->skipprPack = new SkipprPack();

            while ($conn = @stream_socket_accept($this->sock, 60)) {
//                stream_set_blocking($conn, true);
//                stream_set_timeout($conn, 120);

                SkipprLogger::info("New client stream connection");

                while ($data = fread($conn, 8192)) {
                    if ($data) {

                        try {
                            $newData = $this->remainingData . $data;

                            $this->remainingData = $this->readSkipprStream(
                                $newData,
                                $emitMessageCallback,
                                $postReadCallback
                            );
                        } catch (\Exception $e) {
                            SkipprLogger::error($e->getMessage());
                        }
                    } elseif (feof($conn)) {
                        SkipprLogger::info('Client closed connection');
                        fclose($conn);
                    } else {
                        SkipprLogger::info('No client input, sleeping');
                        sleep(1);
                    }
                }
            }
        } catch (\Exception $exception) {
            SkipprLogger::error("Failed to accept connection");
        }
    }

    /**
     * @param string $data
     * @param callable $emitMessageCallback
     * @param callable $postReadCallback
     * @return false|string
     */
    public function readSkipprStream(
        string $data,
        callable $emitMessageCallback,
        callable $postReadCallback
    ) {

        $current_index = 0;

        $bytes = strlen($data);

        $read = '';
        for ($i = $current_index; $i <= $bytes; $i++) {

            if ($read == '') {
                // get message framing
                $size = substr($data, $current_index, 4);
                $msgLen = @unpack('N', $size);

                if (!$msgLen) {
                    break;
                } // not sure why we sometimes get here...

                $readLen = $msgLen[1] + 4;

                if ($i !== 0) { // ensure we read whole message inc length
//                    $i--;
                }

//                if ($readLen > ($bytes - $current_index)) {
//                    $remainingBytes = $bytes - $current_index;
//                    $remainingData = substr($data, $current_index);
//
////                    SkipprLogger::info("Returning socket partially read buffer where readlen $readLen is greater than remaining bytes $remainingBytes");
//                    SkipprLogger::info($remainingData);
//
//                    // @todo - WTF is breaking the message framing...
////                    if ($readLen > 250000) {
//                        // invalid readLen, too long
////                        return '';
////                    } else {
//                        return $remainingData;
////                    }
//                }

//                SkipprLogger::info("ReadLen: $readLen");
            }

            // `index is out of bounds` without this
            // we may have reached the end of the socket buffer
            // more data will likely arrive soon to concat onto our $read buffer
//            if ($i < strlen($data)) {
                $read .= $data[$i];
//            }

//            SkipprLogger::info("Read $read");

            if ($i === ($current_index + $readLen)) {
//                SkipprLogger::info("Current msg start: $current_index");
//                SkipprLogger::info("Current pos: $i");
//                SkipprLogger::info("Outputting $read");


                try {

                    $this->skipprPack->create($read);

                    $record = $this->skipprPack->decodeRecord();

                    $current_index = $i;
                    $read = '';

                    switch ($record) {
                        case 'sync_complete':
                            SkipprLogger::info("Received sync complete event from source stream");
                            $this->shutdown(0);
                            break;

                        case 'schema_update':
                            SkipprLogger::info("Received schema update event from source stream");

                            Config::getConfig();

//                            $this->connect();

                            //                        $this->shutdown(0);

//                            return '';
//                            break;

                        case 'input_buffer_flush':
                            SkipprLogger::info("Received input buffer flush from source stream");
                            call_user_func($emitMessageCallback, $this->skipprPack);
                            break;

//                        default:
//                            call_user_func($emitMessageCallback, $this->skipprPack);
//                            break;
                    }



//                    $this->serialiseOutput($this->skipprPack);

//                    $remainingData = substr($remainingData, $readLen);
                } catch (\Exception $e) {
                    SkipprLogger::error("Failed to output SkipprPack received bytes: $read");
                    SkipprLogger::error($e->getMessage());
                }


            }
        }


//        $remainingData = substr($data, $current_index);

//        if (!empty($remainingData)) {
//            SkipprLogger::debug("Remaining data $remainingData");
//        }

//        call_user_func($postReadCallback);

//        $this->outputPlugin->sync();

        if ($current_index < $bytes) {
            $remainingData = substr($data, $current_index); // remaining bytes

            SkipprLogger::info("Returning remaining data");
            SkipprLogger::info($remainingData);
            return $remainingData;
        } else {
            return '';
        }

//        return $remainingData;
    }

    public function streamConnect()
    {

        $this->connAttempts++;

        SkipprLogger::info("Connecting to stream...");

        $this->host = Config::getenv('HOST', '127.0.0.1');

        $this->sock = stream_socket_client(
            "{$this->host}:{$this->port}",
            $errNo,
            $errorMsg,
            0, // not applicable when using async
            STREAM_CLIENT_CONNECT | STREAM_CLIENT_ASYNC_CONNECT | STREAM_CLIENT_PERSISTENT
//            60, // not applicable when using async
//            STREAM_CLIENT_CONNECT | STREAM_CLIENT_PERSISTENT
        );
        //        stream_set_timeout($this->sock, 600);
        stream_set_blocking($this->sock, false); // wait for data on read or we fill the buffer quickly
        stream_set_chunk_size($this->sock, $this->tcpBufferSize);
        stream_set_write_buffer($this->sock, $this->tcpBufferSize);

        if (!$this->sock && !$errNo) {
            if ($this->connAttempts < $this->connAttemptsMax) {
                sleep($this->connAttempts);

                SkipprLogger::info("Stream connecting...");

                $this->streamConnect();
            } else {
                return false;
            }
        }

        SkipprLogger::info("Stream connected");

        return $this->sock;
    }

    public function streamListen()
    {
        try {
            $this->sock = @\stream_socket_server(
                "tcp://{$this->host}:{$this->port}",
                $errno,
                $errstr,
                STREAM_SERVER_BIND | STREAM_SERVER_LISTEN
                //                            stream_context_create(array('socket' => $context + array('backlog' => 511)))
            );

//            stream_set_timeout($this->sock, 600);
            stream_set_blocking($this->sock, false);
            stream_set_chunk_size($this->sock, $this->tcpBufferSize);
            stream_set_read_buffer($this->sock, $this->tcpBufferSize);

            if (false === $this->sock) {
                if ($errno === 0) {
                    // PHP does not seem to report errno, so match errno from errstr
                    // @link https://3v4l.org/3qOBl

                    $errno = self::errno($errstr);
                }

                throw new \RuntimeException(
                    'Failed to listen on "' . "{$this->host}:{$this->port}" . '": ' . $errstr . self::errconst($errno),
                    $errno
                );
            }

        } catch (\Exception $e) {
            SkipprLogger::error("Failed to listen to socket on tcp://{$this->host}:{$this->port}");
            SkipprLogger::error($e->getMessage());
        }

        return $this->sock;
    }

    public static function errno($errstr)
    {
        if (\function_exists('socket_strerror')) {
            foreach (\get_defined_constants(false) as $name => $value) {
                if (\strpos(
                    $name,
                    'SOCKET_E'
                ) === 0 && \socket_strerror($value) === $errstr) {
                    return $value;
                }
            }
        }

        return 0;
    }


    public static function errconst($errno)
    {
        if (\function_exists('socket_strerror')) {
            foreach (\get_defined_constants(false) as $name => $value) {
                if ($value === $errno && \strpos($name, 'SOCKET_E') === 0) {
                    return ' (' . \substr($name, 7) . ')';
                }
            }
        }

        return '';
    }
}
