<?php


namespace Skipprd\Commands;

use Skipprd\Arr;
use Skipprd\Str;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class RecordFilter
{

    private static $actions = [
        'drop_field',
        'allow_field',
        'drop_record',
        'allow_record',
    ];

    public static function initFilters(&$filters): void
    {

        $envs = getenv();

        foreach ($envs as $name => $val) {
            if (Str::startsWith($name, 'FILTER_')) {
                SkipprLogger::info("Configuring filter $name with value $val");

                $parts = explode('_', $name);
                $filterName = strtolower($parts[1]);
                unset($parts[0]);
                unset($parts[1]);
                $confName = strtolower(implode('_', $parts));

                $filters[$filterName][$confName] = $val;

                // explode in array (in) and not in array (nin) comparison values
                if (isset($filters[$filterName]['operator'])
                    && isset($filters[$filterName]['comparison'])
                    && is_string($filters[$filterName]['comparison'])
                    && in_array(
                        $filters[$filterName]['operator'],
                        ['in', 'nin']
                    )
                ) {
                    $comparison = $filters[$filterName]['comparison'];

                    SkipprLogger::info("exploding comparison: $comparison");

                    $filters[$filterName]['comparison'] = explode(
                        ',',
                        $comparison
                    );
                }
            }
        }
    }

    public static function filter(&$sourceMessage): bool
    {

        if (!empty(Config::$filters)) {
            foreach (Config::$filters as $filter) {
                $value = Arr::get($sourceMessage, $filter['field_path'], false);

                if ($value
                    && RecordFilter::applyFilter(
                        $value,
                        $filter['operator'],
                        $filter['comparison'],
                        $filter['action']
                    )) {
                    Arr::set($sourceMessage, $filter['field_path'], $value);
                } else { // record drop
                    return false;
                }

                $action = $filter['action'];
                $field_path = $filter['field_path'];
                SkipprLogger::debug("Applied filter $action for field path $field_path and value $value");
            }
        }

        return true;
    }

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
