<?php


namespace Skipprd\Traits;


use Skipprd\SkipprPack;

trait SkipprStream
{

    public function streamSend(string $message, $flags = null)
    {

        try {
            
            $exitCode = @stream_socket_sendto($this->sock, $message, $flags);
            // Don't set STREAM_OOB flag, it fills buffer

            $len = strlen($message);

            // sometimes bytes written, sometime success exit code
            // might be platform based, someone else suggest due to blockking/non-blocking
            $success = ($exitCode === 0 || $exitCode === $len);

            if (!$success) {

                SkipprLogger::debug("Stream return code: $exitCode");

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

            SkipprLogger::info("Output stream not available, reconnecting and retransmitting");

            sleep(1);

            $this->streamConnect();

            $this->streamSend($message, $flags);
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
            while ($conn = @stream_socket_accept($this->sock)) {

                stream_set_blocking($conn, true);
                stream_set_timeout($conn, 120);

                SkipprLogger::info("New client stream connection");

                $remainingData = '';

                while ($data = fread($conn, 8192)) {

                    if ($data) {

                        try {

                            $newData = $remainingData . $data;

                            $remainingData = $this->readSkipprStream($newData,
                                $emitMessageCallback, $postReadCallback);

                        } catch (\Exception $e) {

                            SkipprLogger::error("Failed to read stream: $data");
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

        $read = '';
        for ($i = $current_index; $i <= strlen($data); $i++) {

            if ($read == '') {

                // get message framing
                $size = substr($data, $current_index, 4);
                $msgLen = @unpack('N', $size);

                if (!$msgLen) break; // not sure why we sometimes get here...

                $readLen = $msgLen[1] + 4;

                if ($i !== 0) { // ensure we read whole message inc length
                    $i--;
                }

//                SkipprLogger::info("ReadLen: $readLen");

            }

            if ($i < strlen($data)) { // index is out of bounds without this, not sure why
                $read .= $data[$i];
            }

//            SkipprLogger::info("Read $read");

            if ($i === ($current_index + $readLen)) {


//                SkipprLogger::info("Current msg start: $current_index");
//                SkipprLogger::info("Current pos: $i");
//                SkipprLogger::info("Outputting $read");


                try {
                    $sp = new SkipprPack($read);


                    if ($sp->decodeRecord() == 'sync_complete') {

                        SkipprLogger::info("Received sync complete event from source stream");
                        $this->shutdown(0);

                    }

                    call_user_func($emitMessageCallback, $sp);
//                    $this->serialiseOutput($sp);

//                    $remainingData = substr($remainingData, $readLen);


                } catch (\Exception $e) {

                    SkipprLogger::error("Failed to output SkipprPack received bytes: $read");
                    SkipprLogger::error($e->getMessage());
                }

                $current_index = $i;
                $read = '';


            }

        }


        $remainingData = substr($data, $current_index);

//        if (!empty($remainingData)) {
//            SkipprLogger::debug("Remaining data $remainingData");
//        }

        call_user_func($postReadCallback);

//        $this->outputPlugin->sync();

        return $remainingData;

    }

    public function streamConnect()
    {

        SkipprLogger::info("Connecting to stream...");

        $this->host = Config::getenv('HOST', '127.0.0.1');

        $this->sock = stream_socket_client(
            "{$this->host}:{$this->port}",
            $errNo,
            $errorMsg,
            0, // not applicable when using async
            STREAM_CLIENT_CONNECT | STREAM_CLIENT_ASYNC_CONNECT | STREAM_CLIENT_PERSISTENT
        );


        if (!$this->sock) {
            sleep(1);
            $this->streamConnect();
        }

//        stream_set_timeout($this->sock, 600);
        stream_set_blocking($this->sock,
            true); // wait for data on read or we fill the buffer quickly


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

//                        stream_set_timeout($this->sock, 600);
            stream_set_blocking($this->sock, true);

        } catch (\Exception $exception) {

            SkipprLogger::error("Failed to listen to socket on tcp://{$this->host}:{$this->port}");
        }

        return $this->sock;

    }

    public static function errno($errstr)
    {
        if (\function_exists('socket_strerror')) {
            foreach (\get_defined_constants(false) as $name => $value) {
                if (\strpos($name, 'SOCKET_E') === 0 && \socket_strerror($value) === $errstr) {
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

