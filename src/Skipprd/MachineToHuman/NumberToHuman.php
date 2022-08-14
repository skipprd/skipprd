<?php

namespace Skipprd\MachineToHuman;

class NumberToHuman
{

    /**
     * @param $number
     * @return bool|string
     */
    public static function toHuman($number)
    {

        # by james at bandit.co.nz
        # https://www.php.net/manual/en/function.number-format.php
        // first strip any formatting;
        $n = (0 + str_replace(", ", "", $number));

        // is this a number?
        if (!is_numeric($n)) {
            return false;
        }

        // now filter it;
        if ($n > 1000000000000) {
            return round(($n / 1000000000000), 1) . ' T';
        } else {
            if ($n > 1000000000) {
                return round(($n / 1000000000), 1) . ' B';
            } else {
                if ($n > 1000000) {
                    return round(($n / 1000000), 1) . ' M';
                } else {
                    if ($n > 1000) {
                        return round(($n / 1000), 1) . ' K';
                    }
                }
            }
        }

        return number_format($n);
    }
}
