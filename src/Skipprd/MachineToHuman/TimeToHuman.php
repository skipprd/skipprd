<?php

namespace Skipprd\MachineToHuman;

class TimeToHuman
{

    protected static $units = [
        'second',
        'minute',
        'hour',
        'day',
        'week',
        'month',
        'year',
    ];

    protected static $unitsFormats = [
        'second' => '%s',
        'minute' => '%i',
        'hour' => '%h',
        'day' => '%d',
        'week' => '%w',
        'month' => '%m',
        'year' => '%y',
    ];

    protected static $unitTime = [
        'second' => 60,
        'minute' => 3600,
        'hour' => 86400,
        'day' => 604800,
        'week' => 2592000,
        'month' => 31536000,
        'year' => 99999999999999,
    ];

    /**
     * @param $time
     * @param string $format
     * @return bool|string
     */
    public static function minsToHoursMins($time, $format = '%02d:%02d')
    {
        if ($time < 1) {
            return false;
        }
        $hours = floor($time / 60);
        $minutes = ($time % 60);

        return sprintf($format, $hours, $minutes);
    }

    /**
     * @param $seconds
     * @param bool $withUnits
     * @param bool $forceUnit
     * @return float|int|string
     */
    public static function toHuman($seconds, $withUnits = true, $forceUnit = false)
    {

        $return = 0;
        $i = 0;

        // Convert to logical units
        if (!$forceUnit) {
            foreach (self::$unitTime as $unit) {
                $i++;

                if ($seconds > $unit) {
                    continue;
                }
                $return = round($seconds, 2);
                if ($withUnits) {
                    $return = $return . ' ' . self::$units[$i];
                }
            }
        } else {
            $unitTimeFraction = round($seconds / self::$unitTime[$forceUnit], 2);

            $return = $unitTimeFraction;

            if ($withUnits) {
                $return = $unitTimeFraction . ' ' . $forceUnit;
            }
        }

        return $return;
    }

    /**
     * @param $seconds
     * @param string $format
     * @return bool|string
     */
    public static function auto($seconds, $format = '%02d:%02d')
    {
        if ($seconds < 1) {
            return false;
        }
        if ($seconds >= 3600) {
            $hours = floor($seconds / 60);
            $minutes = ($seconds % 60);

            return sprintf($format, $hours, $minutes);
        } elseif ($seconds >= 60) {
            $minutes = ($seconds % 60);
            return sprintf('%02d mins', $minutes);
        } else {
            $seconds = round($seconds, 2);
            return sprintf('%02d seconds', $seconds);
        }
    }

    public static function suggestUnit($seconds)
    {

        foreach (self::$units as $unit) {
            if ($seconds > self::$unitTime[$unit]) {
                $suggestedUnit = $unit;
                continue;
            }
            return $suggestedUnit;
        }
    }
}
