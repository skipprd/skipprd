<?php


namespace Skipprd\Traits;

use Carbon\Carbon;
use Monolog\Formatter\JsonFormatter;
use Monolog\Formatter\LineFormatter;
use Monolog\Handler\StreamHandler;
use Monolog\Logger;
use Monolog\Registry;

trait SkipprLogger
{

    private static $jsonFormatter;

    public static function init()
    {

        $skipprLogger = Registry::hasLogger('skipprd');

        if (!$skipprLogger) {
            // the default date format is "Y-m-d\TH:i:sP"
            $dateFormat = "Y-m-d\TH:i:sP";
            // the default output format is "[%datetime%] %channel%.%level_name%: %message% %context% %extra%\n"
            $output = "[%datetime%] %channel%.%level_name%: %message%\n";

            $formatter = new LineFormatter($output, $dateFormat);
            self::$jsonFormatter = new JsonFormatter($output, $dateFormat);

            // Create a handler
            $stream = new StreamHandler('php://stderr', \Monolog\Logger::DEBUG);
            $stream->setFormatter($formatter);
            $stream->setLevel(Config::$logLevel);

            $application = new Logger('skipprd');
            $application->pushHandler($stream);

            Registry::addLogger($application);
        }
    }

    public static function debug(string $message) : void
    {

//        Config::$taskLogs[] = $message;

        SkipprLogger::init();
        Registry::skipprd()->debug($message);
    }

    public static function info(string $message) : void
    {
        SkipprLogger::init();

        $datetime = Carbon::now()->toIso8601String();
        Config::$taskLogs[] = "$datetime skipprd.INFO $message";
        Registry::skipprd()->info($message);
    }

    public static function error(string $message) : void
    {

        SkipprLogger::init();

        $datetime = Carbon::now()->toIso8601String();
        Config::$taskLogs[] = "$datetime skipprd.INFO $message";
        Registry::skipprd()->error($message);
    }

    public static function emergency(string $message) : void
    {
        SkipprLogger::init();

        $datetime = Carbon::now()->toIso8601String();
        Config::$taskLogs[] = "$datetime skipprd.INFO $message";
        Registry::skipprd()->emergency($message);
    }

    public static function critical(string $message) : void
    {
        SkipprLogger::init();

        $datetime = Carbon::now()->toIso8601String();
        Config::$taskLogs[] = "$datetime skipprd.INFO $message";
        Registry::skipprd()->critical($message);
    }
}
