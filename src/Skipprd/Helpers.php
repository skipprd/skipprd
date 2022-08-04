<?php


namespace Skipprd;

use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class Helpers
{

    protected static $cleanFieldCache = [];

    public static function explodeField($field)
    {

        // CamelCase to underscore
        $string = preg_replace('/(?<=\\w)(?=[A-Z])/', "_$1", $field);
        $string = strtolower($string);

        // dots to underscore
        $string = preg_replace('/\./', '_', $string);

        $fieldHaystack = explode('_', Str::slug($string, '_'));

        return $fieldHaystack;
    }

    public static function isSequentialArrayKeys(array $arr)
    {
        ksort($arr);
        if (array_key_first($arr) !== 0 && array() === $arr) {
            return false;
        }

        return array_keys($arr) === range(0, count($arr) - 1);
    }

    /**
     * Clean field name string to alpha numeric and underscores
     *
     * @param $field
     * @return string field
     */
    public static function cleanFieldName(string $field = ''): string
    {
        $clean = $field;

        if (!isset(self::$cleanFieldCache[$field]) || self::$cleanFieldCache[$field]) {

            if (is_numeric($field)) {
                $field = 'item_' . $field;
//            return $field;
            }

            $field = strtolower($field);

            $pattern = "/[^" . preg_quote(
                    '_0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ',
                    "/"
                ) . "]/";

            $clean = preg_replace($pattern, "_", $field);

            $clean = ltrim($clean, '0123456789');

            // '_' at the beginning is common and probably allowable
            $clean = trim($clean, '_');

            if ($clean !== $field) {
                self::$cleanFieldCache[$field] = true;
            } else {
                self::$cleanFieldCache[$field] = false;
            }
        }

        return $clean;
    }

    static function cleanArrayFieldNames(&$array)
    {
        foreach ($array as $field => $value) {
            unset($array[$field]);

            $field = Helpers::cleanFieldName($field);

            if (!empty($value) && is_array($value)) {
                self::cleanArrayFieldNames($value);
            }

            $array[$field] = $value;
        }
    }

    public static function randomPassword($length = 8)
    {
        $alphabet = 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890';
        $pass = []; //remember to declare $pass as an array
        $alphaLength = strlen($alphabet) - 1; //put the length -1 in cache
        for ($i = 0; $i < $length; $i++) {
            $n = rand(0, $alphaLength);
            $pass[] = $alphabet[$n];
        }

        return implode($pass); //turn the array into a string
    }

    public static function randomStr($length = 8)
    {
        $alphabet = 'abcdefghijklmnopqrstuvwxyz';
        $pass = []; //remember to declare $pass as an array
        $alphaLength = strlen($alphabet) - 1; //put the length -1 in cache
        for ($i = 0; $i < $length; $i++) {
            $n = rand(0, $alphaLength);
            $pass[] = $alphabet[$n];
        }

        return implode($pass); //turn the array into a string
    }

    /**
     * Flatten a multi-dimensional array into a single level.
     *
     * @param array $array
     * @param int $depth
     * @return array
     */
    public static function flatten(array $array, $delimiter = '', $prefix = '')
    {
        $result = array();
        foreach ($array as $key => $value) {
            if (is_array($value)) {
                $result = $result + self::flatten(
                    $value,
                    '_',
                    $prefix . $delimiter . $key
                );
            } else {
                $result[$prefix . $delimiter . $key] = $value;
            }
        }

        return $result;
    }

    /**
     * @return bool - true for mem allocation full, false for mem available
     *
     * clone from https://www.php.net/manual/en/function.memory-get-usage.php#120665
     */
    public static function memLimitReached(): bool
    {

        $memLimit = Config::$containerMem * 1024 * 1024 * 0.8; // allow overhead, set below memory_limit

        $memUsage = memory_get_usage(true);

        if ($memUsage >= $memLimit) {
            return true;
        }

        return false;
    }
}
