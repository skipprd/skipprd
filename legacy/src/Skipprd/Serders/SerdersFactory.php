<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 12/05/2020
 * Time: 12:05
 */

namespace legacy\src\Skipprd\Serders;

use legacy\src\Skipprd\Str;
use function Skipprd\Serders\count;

class SerdersFactory
{

    /**
     * @param string $record - string representing one or more rows
     *                         (e.g. single json, multiline json, CSV, etc)
     * @return \legacy\src\Skipprd\Serders\Interfaces\SerderBatchInterface|\legacy\src\Skipprd\Serders\Interfaces\SerderStreamInterface
     */
    public static function factory(string $serder, \AvroSchema $schema = null)
    {
        $className = "Skipprd\\Serders\\Serder" . ucfirst(Str::camel($serder));
        return new $className($schema);
    }


    public static function discover(string $record)
    {

        // attempt to discover serialisation type
        $serders = ["json" => 0, "csv" => 0, "avro" => 0];

        foreach ($serders as $serderCandidate => &$count) {
            try {
                $className = "Skipprd\\Serders\\Serder" . ucfirst(Str::camel($serderCandidate));
                $serderClass = new $className();

                $analysisMessages = @$serderClass->deserialize($record);

                // serder with most fields in each message wins
                // recursive count for nested data
                $count = count($analysisMessages, COUNT_RECURSIVE);
            } catch (\Exception $e) {
            }
        }

        if (max($serders) > 0) {
            $serder = array_search(max($serders), $serders);

            return $serder;
//            $className = "Skipprd\\Serders\\Serder" . Str::camel($serder);
//            return new $className();
        }

        return false;
    }
}
