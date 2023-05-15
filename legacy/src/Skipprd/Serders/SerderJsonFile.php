<?php


namespace legacy\src\Skipprd\Serders;

use legacy\src\Skipprd\Serders\Interfaces\SerderBatchInterface;

class SerderJsonFile implements SerderBatchInterface
{

    public $supportedCompressionTypes = [
        self::FILE_COMPRESSION,
        self::NO_COMPRESSION
    ];

    public $compressonType = '';

    protected $fh;
    protected $records;

    protected SerderJson $jsonSerde;

    public function __construct()
    {

        $this->jsonSerde = new SerderJson();

        $this->compressonType = self::FILE_COMPRESSION;

    }

    public function openWriter(string $filename, array $schema): void {

        if ($this->compressonType === self::NO_COMPRESSION
        ) {
            $this->fh = fopen($filename, 'a+');
        } else {
            $this->fh = gzopen($filename . '.gz', 'w6');
        }
    }

    public function closeWriter(): void {

        foreach ($this->records as $data) {

            if ($this->compressonType === self::NO_COMPRESSION) {
                fputs($this->fh, $data);
            } else {
                gzwrite($this->fh, $data);
            }
        }

        if ($this->compressonType === self::NO_COMPRESSION
        ) {
            fclose($this->fh);
        } else {

            gzclose($this->fh);
        }

    }

    public function deserialize(string $record): array
    {
        return $this->jsonSerde->deserialize($record);
    }


    public function serialize(array $record, array $schema = null): void
    {
        $this->records[] = json_encode($record);
    }

    public function jsonDecode(string $string) : array
    {

        return $this->jsonSerde->jsonDecode($string);
    }

    public function defaultMessage(array $schema = []): array
    {

        return $this->jsonSerde->defaultMessage($schema);
    }

}
