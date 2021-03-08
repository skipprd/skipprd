<?php


namespace Skipprd;


class SkipprPack
{

    /**
     * @var int set position equal to $offset bytes
     */
    const SEEK_CUR = SEEK_CUR;
    /**
     * @var int set position to current index + $offset bytes
     */
    const SEEK_SET = SEEK_SET;
    /**
     * @var int set position to end of file + $offset bytes
     */
    const SEEK_END = SEEK_END;

    private $offset = '';

    private $payload = '';

    /**
     * @var string
     */
    private $string_buffer;
    /**
     * @var int  current position in string
     */
    private $current_index;

    public function __construct(string $skipprPack = '')
    {

        $this->string_buffer = '';
        $this->current_index = 0;

        if(is_string($skipprPack)) {
            $this->string_buffer .= $skipprPack;
        }
        else {
            throw new \Exception(sprintf('constructor argument must be a string: %s', gettype($skipprPack)));
        }


    }

    public function encode(string $payload = '', string $offset = '') : void
    {

        $this->payload = $payload;
        $this->offset = $offset;

        // write the offset length in network byte order (big end)
        $this->write(pack('N', strlen($this->offset)));

        // write the offset in network byte order (big end)
        $this->write($this->offset);

        // write the record
        $this->write($this->payload);

    }

    public function decodeRecord(): string
    {

        $this->rewind();

        $size = $this->read(4);
        $offsetSize = unpack('N', $size);
        $this->seek($offsetSize[1], SEEK_CUR);

        $record = $this->fpassthru();

        return $record;

    }

    public function decodeOffset(): string
    {

        $this->rewind();

        $size = $this->read(4);
        $offsetSize = unpack('N', $size);

        $offset = $this->read($offsetSize[1]);

        return $offset;

    }


    public function read($len) : string
    {
//        $read='';
//        for($i=$this->current_index; $i<($this->current_index+$len); $i++)
//            $read .= $this->string_buffer[$i];
//        if (strlen($read) < $len)
//            $this->current_index = $this->length();
//        else
//            $this->current_index += $len;

        $read = substr($this->string_buffer, $this->current_index, $len);

        $this->current_index += $len;

        return $read;

    }


    public function fpassthru() : string
    {

        $len = $this->length() - $this->current_index;

        $read = substr($this->string_buffer, $this->current_index, $len);

        $this->current_index += $len;

        return $read;

    }

    /**
     * @param int $offset
     * @param int $whence
     * @return bool true if successful
     */
    public function seek($offset, $whence=self::SEEK_SET) : bool
    {
        if (!is_int($offset)) {
            throw new \Exception('Seek offset must be an integer.');
        }

        // Prevent seeking before BOF
//        switch ($whence)
//        {
//            case self::SEEK_SET:
//                if (0 > $offset)
//                    throw new \Exception('Cannot seek before beginning of file.');
//                $this->current_index = $offset;
//                break;
//            case self::SEEK_CUR:
//                if (0 > $this->current_index + $whence)
//                    throw new \Exception('Cannot seek before beginning of file.');
//                $this->current_index += $offset;
//                break;
//            case self::SEEK_END:
//                if (0 > $this->length() + $offset)
//                    throw new \Exception('Cannot seek before beginning of file.');
//                $this->current_index = $this->length() + $offset;
//                break;
//            default:
//                throw new \Exception(sprintf('Invalid seek whence %d', $whence));
//        }

        return true;
    }

    public function rewind() : void
    {

        $this->seek(0, self::SEEK_SET);

    }

    /**
     * @return int
     */
    public function tell() : int
    {
        return $this->current_index;
    }

    /**
     * @return boolean
     */
    public function is_eof() : int
    {
        return ($this->current_index >= $this->length());
    }

    public function length() : int
    {
        return strlen($this->string_buffer);
    }

    /**
     * Append bytes to this buffer.
     * @param string $arg bytes to write
     * @return int count of bytes written.
     */
    public function write($arg)
    {
//        if (is_string($arg)) return $this->append_str($arg);
        return true;

    }

    /**
     * Appends bytes to this buffer.
     * @param string $str
     * @return integer count of bytes written.
     */
    private function append_str($str) : int
    {
        $this->string_buffer .= $str;
        $len = strlen($str);
        $this->current_index += $len;
        return $len;
    }

    /**
     * Truncates the truncate buffer to 0 bytes and returns the pointer
     * to the beginning of the buffer.
     * @return boolean true
     */
    public function truncate() : bool
    {
        $this->string_buffer = '';
        $this->current_index = 0;
        return true;
    }


    /**
     * @return string
     */
    public function __toString() : string
    {
        return $this->string_buffer;
    }


    /**
     * @return string
     * @uses self::__toString()
     */
    public function string() : string
    {
        return $this->__toString();
    }

}