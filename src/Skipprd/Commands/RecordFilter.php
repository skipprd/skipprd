<?php


namespace Skipprd\Commands;


class RecordFilter
{

    private static $actions = [
        'drop_field',
        'allow_field',
        'drop_record',
        'allow_record',
    ];

    public static function applyFilter(
        &$value,
        string $operator,
        $comparison,
        string $action
    ): bool {

        $result = self::applyComparison($value, $operator, $comparison);

        switch ($action) {

            case 'drop_field':
                $value = null;

                return $result;
            case 'allow_field':
                return $result;
            case 'drop_record':
                return false;
            case 'allow_record':
                return $result;
        }

        return true;
    }

    public static function applyComparison(
        &$value,
        $operator,
        $comparison
    ): bool {

        switch ($operator) {

            case 'eq':
                return $value == $comparison;
                break;
            case 'ne':
                return $value !== $comparison;
                break;
            case 'gt':
                return $value > $comparison;
                break;
            case 'gte':
                return $value >= $comparison;
                break;
            case 'lt':
                return $value < $comparison;
                break;
            case 'lte':
                return $value <= $comparison;
                break;
            case 'z':
                return empty($value);
                break;
            case 'n':
                return !empty($value);
                break;
            case 'in':
                return in_array($value, $comparison);
                break;
            case 'nin':
                return !in_array($value, $comparison);
                break;
            case 'default':
                new \Exception("Filter operator $operator did not match expected operators (eq, ne, gt, gte, lt, lte, z, n, in, nin)");

        }

        return false;

    }

}