<?php


namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class SerderXml implements SerderBatchInterface
{

    public function __construct(\AvroSchema $schema = null)
    {
    }

    public function deserialize(string $payload): array
    {

        $array = [];
        
        $xml = simplexml_load_string($payload, null, LIBXML_NOCDATA);
        foreach ($xml as $xmlItem) {
            $array[] = json_decode(json_encode($xmlItem), true);
        }

        return $array;
    }

    public function serialize(array $records, string $filename, $schema = null): void
    {

        $xml_data = new \SimpleXMLElement('<?xml version="1.0"?><data></data>');

        $this->array_to_xml($records, $xml_data);

        $result = $xml_data->asXML($filename);
    }

    public function array_to_xml($data, &$xml_data)
    {
        foreach ($data as $key => $value) {
            if (empty($value)) {
                continue;
            }
            if (is_array($value)) {
                if (is_numeric($key)) {
                    $key = 'item'; //dealing with <0/>..<n/> issues
                }

                $subnode = $xml_data->addChild($key);
                $this->array_to_xml($value, $subnode);
            } else {
                if (is_numeric($key)) {
                    $key = 'item'; //dealing with <0/>..<n/> issues
                }
                
                $xml_data->addChild("$key", htmlspecialchars("$value"));
            }
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
