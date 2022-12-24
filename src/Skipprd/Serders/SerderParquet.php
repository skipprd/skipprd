<?php


namespace Skipprd\Serders;

use Skipprd\Converters\AvroParquetSchemaConverter;
use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\Config;
use Skipprd\SkipprLogger;

use codename\parquet\ParquetWriter;

use codename\parquet\data\Schema;
use codename\parquet\data\DataField;
use codename\parquet\data\DataColumn;


class SerderParquet implements SerderBatchInterface
{
    public $compressionType = self::VALUE_COMPRESSION;

    private $parquet = false;

    private $records = [];

    public function __construct()
    {
    }

    public function deserialize(string $payload): array
    {

        throw new \Exception("Method not implemented");
    }

    public function openWriter(string $filename, array $schema): void {

        // create file schema
//        $schema = new Schema([$idColumn->getField(), $cityColumn->getField()]);
//
//        foreach ($schema as $field) {
        $convertedSchema = $this->recruse($schema);
//            );
//        }

        $parquetSchema = new Schema($convertedSchema);

//// create file handle with w+ flag, to create a new file - if it doesn't exist yet - or truncate, if it exists
        $fileStream = fopen(__DIR__.'/test.parquet', 'w+');

        $parquetWriter = new ParquetWriter($parquetSchema, $fileStream);

        $groupWriter = $parquetWriter->CreateRowGroup();

//        if (!$this->parquet) {
//            $this->parquet = new \Parquet();
//
//            $this->parquet->create_writer($filename, $schema, 'snappy');
//        }

    }

    public function closeWriter(): void {

        // @todo - can't bulk write else get errors like
        // "Column 51 had 63784 while previous column had 1"
//        $this->parquet->write($this->records);

        $this->parquet->close_writer();

//        unset($this->records);
        unset($this->parquet);
    }

    protected function recruse(array $schema, array $parquetSchema = []): array {

        foreach ($schema as $field => $subSchema) {

            if (!empty($schema[$field]['schema']['schema'])) {
                $parquetSchema = $this->recruse($subSchema, $parquetSchema);
//                $parquetSchema[] = $dataColum->getField();
            } else {
                $dataColum = new DataColumn(
                    DataField::createFromType($field['name'],
                        $field['type'], $field['repeat']),
                    []
                );
                $parquetSchema[$field] = $dataColum->getField();
            }
        }

        return $parquetSchema;
    }

    protected function recruseSetValue(array $parquetSchema, string $fieldName, string $value): array {

        foreach ($parquetSchema as $key => $subSchema) {

            if (is_array($value)) {
                $parquetSchema = $this->recruseSetValue($parquetSchema[$fieldName], $fieldName, $value);
            } else {
                $dataColum = new DataColumn(
                    DataField::createFromType($fieldName,
                        $parquetSchema[$fieldName]['type'], $parquetSchema[$fieldName]['repeat']),
                    []
                );
                $parquetSchema[$fieldName] = $dataColum->getField();
            }
        }

        return $parquetSchema;
    }

    public function serialize(array $record): void
    {

//        foreach ($record as $fieldName => $value) {
//            $parquetSchema[$fieldName] = $value
//        }
//
//
//        $groupWriter->WriteColumn($idColumn);
//        $groupWriter->WriteColumn($cityColumn);


        ///////////////////


//    public function serialize(array $record, array $schema): void


//        foreach ($record as $field) {
//            $types[] = $this->recruse($field);
//            );
//        }

//        $fileStream = fopen(__DIR__.'/test.parquet', 'w+');
//
//        $this->parquet = new ParquetWriter($schema, $fileStream);

        try {
//            $this->records[] = $record;
            $this->parquet->write([$record]);
            unset($record);
        } catch (\Exception $exception) {

        }
    }

    public function defaultMessage(array $schema = []): array
    {

        try {
            // Init with internal special fields
            if (empty($schema)) {
                $message = Config::$specialFields;
            }

            foreach ($schema as $i => $field) {
                if (!empty($field['type'][1]['fields'])) {
                    $message[$field['name']] = $this->defaultMessage($field['type'][1]['fields']);
                } else {
                    if (!empty($field['type'][1]['type'])) {
                        if ($field['type'][1] == 'record') {
                            $message[$field['name']] = ['' => null];
                        } elseif ($field['type'][1]['type'] == 'array') {
                            $message[$field['name']] = [];
                        } elseif ($field['type'][1]['type'] == 'map') {
                            if ($field['type'][1]['values'] == 'string') {
                                $message[$field['name']] = ['' => ''];
//                                $message[$field['name']] = ['' => null];
                            }
                            if ($field['type'][1]['values'] == 'int') {
                                // @todo - I think parquet and athena might support null now?
                                $message[$field['name']] = ['' => 0];
//                                $message[$field['name']] = ['' => null];
                            }
                        }
                    } else {
                        $message[$field['name']] = null;
                    }
                }
            }
        } catch (\Exception $e) {
            SkipprLogger::error('Unable to build default message.');
            throw $e;
        }

        return $message;
    }
}
