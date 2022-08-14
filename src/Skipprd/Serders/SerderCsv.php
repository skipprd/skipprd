<?php


namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;
use Skipprd\Traits\SkipprLogger;

class SerderCsv implements SerderBatchInterface
{

    public $csvHeaders = [];
    
    public function __construct()
    {
    }
    
    /**
     * @todo - Remove and handle in connector.
     *         We should not receive multi record serialisation here.
     *
     * @param $record
     * @return mixed
     */
    public function deserialize(string $record): array
    {

        $messages = [];

        // fix newline encodings
        // fgetcsv doesn't recognise with \r for instance
        $record = preg_replace("/\r\n|\n\r|\n|\r/", "\n", $record);

        // mocking a stream is best way to deal with new line chars
        $fp = fopen("php://temp", 'r+');
        fputs($fp, $record);
        rewind($fp);


        // Determine delimiter
        $firstLine = fgets($fp);

        $delimiters = [";" => 0, "," => 0, "\t" => 0, "|" => 0];

        foreach ($delimiters as $delimiter => &$count) {
            $count = count(str_getcsv($firstLine, $delimiter));
        }

        $delimiter = array_search(max($delimiters), $delimiters);

        rewind($fp);

        /**
         * deserialise handling multilines in csv
         */
        while (($data = fgetcsv($fp, null, $delimiter) ) !== false) {
            $is_header_row = false;
            $headers = $this->csvHeaders;

            // track fields in a CSV row
            // Used to remove rows with too few fields, typically indicates
            // non CSV and causes incorrect serder discovery
            $fieldCount = 0;

            // check for header row
            if (empty($headers)) {
                $is_header_row = true;

                foreach ($data as $item) {
                    if (AnalyseSchema::checkStringOrInt($item) != 'string') {
                        $is_header_row = false;
                    }
                }

                if ($is_header_row) {
                    $headers = $data;
                    $this->csvHeaders = $headers;
                }
            }

            $fieldCount = max($fieldCount, count($data));

            // don't add header row to data
            if (!$is_header_row && $fieldCount > 1) {
                // has header column names
                // field count is greater than 1
                if (!empty($this->csvHeaders)) {
                    $keyedRow = [];

                    foreach ($data as $key => $item) {
                        $keyedRow[$this->csvHeaders[$key]] = trim($item);
                    }
                    $messages[] = $keyedRow;
                } else { // no header column names, int key index only
                    $messages[] = $data;
                }
            }
        }

        // Ensure all fields present in each row
        $messages = array_filter($messages, function ($line) use ($fieldCount) {
            return $fieldCount == count($line);
        });

        fclose($fp);

        return $messages;
    }

    public function serialize(array $record, string $filename, $schema = null): void
    {

        $fh = fopen($filename, 'a+');

        $i = 0;

        # write out the data
        foreach ($record as $row) {
            if ($i === 0) {
                # write out the headers
                fputcsv($fh, array_keys(current($record)));

                $i++;
            }

            foreach ($row as $field => $item) {
                if (is_array($item)) {
                    $data[$field] = json_encode($item);
                } else {
                    $data[$field] = $item;
                }
            }

//            $data = json_encode($row, 0, 2);

            fputcsv($fh, $data);
        }

        fclose($fh);
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
//                                $message[$field['name']] = ['' => 0];
                                $message[$field['name']] = ['' => null];
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
