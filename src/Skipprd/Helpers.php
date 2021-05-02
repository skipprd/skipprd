<?php


namespace Skipprd;

class Helpers
{

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
        if (array_key_first($arr) !== 0 && array() === $arr) return false;
        return array_keys($arr) === range(0, count($arr) - 1);
    }

    /**
     * Clean field name string to alpha numeric and underscores
     *
     * @param $field
     * @return string field
     */
    public static function cleanFieldName($field)
    {
        if (is_numeric($field)) {
            $field = 'A' . $field;
//            return $field;
        }

        $field = strtolower($field);

        $pattern = "/[^" . preg_quote('0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ',
                "/") . "]/";

        return preg_replace($pattern, "", $field);
    }

    static function cleanArrayFieldNames(&$array) {
        foreach ($array as $field => $value) {

            unset($array[$field]);

            $field = Helpers::cleanFieldName($field);

            if (!empty($value) && is_array($value)) {
                self::cleanArrayFieldNames($value);
            }

            $array[$field] = $value;
        }

    }

    static public function randomPassword($length = 8)
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
}


