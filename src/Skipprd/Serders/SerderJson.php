<?php


namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderStreamInterface;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class SerderJson implements SerderStreamInterface
{

    public function __construct()
    {
    }
    
    public function deserialize(string $record): array
    {
        $message = [];
        $records = [];
        $messages = [];

        // mocking a stream is best way to deal with new line chars
        $fp = fopen("php://temp", 'r+');
        fputs($fp, $record);
        rewind($fp);

        // deserialise handling multiline json
        while (($data = fgets($fp) ) !== false) {
            $line = self::jsonDecode($data);

            $records = [];

            if (!empty($line)) { // deserialize ok
                // if array of json objects

                if (is_array($line) && AnalyseSchema::checkStringOrInt(key($line)) == 'integer') {
                    $records = $line;
                    //                foreach ($record as $item) {
                    //                    $messages[] = $this->parseRecordJson($item, 1);
                    //                }
                } else {
                    $records[] = $line;
                }
            }

            foreach ($records as $item) {
                if (is_string($item)) {
                    $message = self::jsonDecode($item);

                    $messages[] = $message;
                } else {
                    $messages[] = $item;
                }
            }
        }

        return $messages;
    }

    public function serialize(array $record, $schema = null): string
    {
        return json_encode($record);
    }

    public function jsonDecode(string $string)
    {

        $message = json_decode($string, true);

        if (json_last_error() == 4) {

            /**
             * Basic clean up
             */
            // handle escaped json
            $string = stripslashes($string);
            // and sometimes double escaped
            $string = stripslashes($string);

            // handle python unicode strings
            // @todo - better way?
            $string = str_replace("u'", '"', $string);

            // handle invalid single quotes
            $string = str_replace("'", '"', $string);

            /**
             * This will remove unwanted characters.
             * Check http://www.php.net/chr for details
             */
            for ($d = 0; $d <= 31; ++$d) {
                $string = str_replace(chr($d), "", $string);
            }
            $string = str_replace(chr(127), "", $string);

            // Some file begins with 'efbbbf' to mark the beginning of the file. (binary level)
            // here we detect it and we remove it, basically it's the first 3 characters
            // see https://en.wikipedia.org/wiki/Byte_order_mark
            if (0 === strpos(bin2hex(substr($string, 0, 6)), 'efbbbf')) {
                $string = substr($string, 3);
            }

            /**
             * Eagerly and perhaps over zealously glob any json we can find by stripping any
             * remaining non-json from beginning of source data strings.
             */
            $jsonStart = strpos($string, '["');
            if (!$jsonStart) {
                $jsonStart = strpos($string, '{"');
            }

            if ($jsonStart > 0) {
                $string = substr($string, $jsonStart);
            }

            $message = json_decode($string, true);
        }

        return $message;
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
