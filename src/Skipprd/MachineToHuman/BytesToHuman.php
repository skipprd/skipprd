<?php

namespace Skipprd\MachineToHuman;

class BytesToHuman
{

    protected static $units = [
        'B',
        'KB',
        'MB',
        'GB',
        'TB',
        'PB',
    ];

    /**
     * @param $size
     * @param bool $withUnits - append units (MB, GB, etc)
     * @param int $precision
     * @return string
     */
    public static function toHuman($size, $withUnits, $precision = 2)
    {
        static $units = array('B','KB','MB','GB','TB','PB','EB','ZB','YB');
        $step = 1024;
        $i = 0;
        while (($size / $step) > 0.9) {
            $size = $size / $step;
            $i++;
        }

        if ($withUnits) {
            return round($size, $precision).$units[$i];
        } else {
            return round($size, $precision);
        }
    }


    /**
     * @param $bytes
     * @return string
     */
    public static function suggestUnit($bytes)
    {

        $suggestedUnit = reset(self::$units);

        // Ensure conversion to unit
        foreach (self::$units as $i => $unit) {
            if ($bytes <= 1024) {
                return $suggestedUnit;
            }

            $suggestedUnit = self::$units[$i + 1];

            $bytes /= 1024;
        }

        return $suggestedUnit;
    }
}
