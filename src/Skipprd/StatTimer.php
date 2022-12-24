<?php

namespace Skipprd;

class StatTimer
{

    protected static array $metrics;

    public static function initMetric(string $metric): void
    {
        if (!isset(self::$metrics[$metric])) {
//            SkipprLogger::info("Init $metric");
            self::$metrics[$metric]['elapsed'] = 0;
        }
    }

    public static function start(string $metric): void
    {
        self::initMetric($metric);

        self::$metrics[$metric]['start_time'] = self::milliseconds();
    }

    public static function stop(string $metric)
    {
        self::$metrics[$metric]['elapsed'] += self::milliseconds() - self::$metrics[$metric]['start_time'];
        self::$metrics[$metric]['start_time'] = 0;
    }

    public static function milliseconds()
    {
        [$a, $b] = explode(' ', microtime());

        return floatval(intval($b) . "" . intval($a * 1000));
    }

    public static function parse()
    {
//        self::stop(\Skipprd\Metrics::TOTAL);
        SkipprLogger::info("####### Stats #########");
        foreach (self::$metrics as $metric => $values) {
            SkipprLogger::info("$metric time " . $values['elapsed']);
        }
        SkipprLogger::info("####### Stats #########");
    }
}