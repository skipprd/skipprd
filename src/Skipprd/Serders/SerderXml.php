<?php


namespace Skipprd\Serders;

use Skipprd\Serders\Interfaces\SerderBatchInterface;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;

class SerderXml implements SerderBatchInterface
{

    public function __construct(\AvroSchema $schema = null) {

    }

    public function deserialize(string $payload): array
    {

        $array = [];
        
        $xml = simplexml_load_string($payload, null, LIBXML_NOCDATA);
        foreach ($xml as $xmlItem) {
            $array[] = json_decode(json_encode($xmlItem),TRUE);
        }

        return $array;

    }

    public function serialize(array $records, string $filename, $schema = null): void
    {

        $xml_data = new \SimpleXMLElement('<?xml version="1.0"?><data></data>');

        $this->array_to_xml($records,$xml_data);

        $result = $xml_data->asXML($filename);

    }

    public function array_to_xml( $data, &$xml_data ) {
        foreach( $data as $key => $value ) {

            if (empty($value)) {
                continue;
            }
            if( is_array($value) ) {
                if ( is_numeric($key) ){
                    $key = 'item'; //dealing with <0/>..<n/> issues
                }

                $subnode = $xml_data->addChild($key);
                $this->array_to_xml($value, $subnode);

            } else {

                if ( is_numeric($key) ){
                    $key = 'item'; //dealing with <0/>..<n/> issues
                }
                
                $xml_data->addChild("$key", htmlspecialchars("$value"));
            }
        }
    }
}