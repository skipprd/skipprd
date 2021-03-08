<?php


namespace Skipprd\Serders;

use Skipprd\Traits\AnalyseSchema;

class SerderCsv implements SerderInterface
{

    private static $csvHeaders = [];

    public function __construct(\AvroSchema $schema = null) {

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

        self::$csvHeaders = [];
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
        while ( ($data = fgetcsv($fp, null, $delimiter) ) !== FALSE ) {

            $is_header_row = false;
            $headers = self::$csvHeaders;

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
                    self::$csvHeaders = $headers;
                }
            }

            $fieldCount = max($fieldCount, count($data));

            // don't add header row to data
            if (!$is_header_row && $fieldCount > 1) {

                // has header column names
                // field count is greater than 1
                if (!empty(self::$csvHeaders)) {

                    $keyedRow = [];

                    foreach ($data as $key => $item) {
                        $keyedRow[self::$csvHeaders[$key]] = trim($item);
                    }
                    $messages[] = $keyedRow;


                } else { // no header column names, int key index only
                    $messages[] = $data;
                }
            }
        }

        // Ensure all fields present in each row
        $messages = array_filter($messages, function($line) use ($fieldCount) {
            return $fieldCount == count($line);
        });

        return $messages;
    }

    public function serialize(array $record): string
    {

        return 'TODO';
    }
}