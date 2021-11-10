<?php


namespace Skipprd\Traits;

use Monolog\Formatter\LineFormatter;
use Monolog\Handler\StreamHandler;
use Monolog\Logger;
use Monolog\Registry;

trait SkipprLogger
{

    public static function init()
    {

        $foo = Registry::hasLogger('skipprd');

        if (!$foo) {
            // the default date format is "Y-m-d\TH:i:sP"
            $dateFormat = "Y-m-d\TH:i:sP";
            // the default output format is "[%datetime%] %channel%.%level_name%: %message% %context% %extra%\n"
            $output = "[%datetime%] %channel%.%level_name%: %message%\n";

            $formatter = new LineFormatter($output, $dateFormat);

            // Create a handler
            $stream = new StreamHandler('php://stderr', \Monolog\Logger::DEBUG);
            $stream->setFormatter($formatter);


            $application = new Logger('skipprd');
            $application->pushHandler($stream);

            Registry::addLogger($application);
        }
    }

    public static function debug(string $message) : void
    {
        SkipprLogger::init();
        Registry::skipprd()->debug($message);
    }

    public static function info(string $message) : void
    {
        SkipprLogger::init();
        Registry::skipprd()->info($message);
    }

    public static function error(string $message) : void
    {
        SkipprLogger::init();
        Registry::skipprd()->error($message);
    }

    public static function emergency(string $message) : void
    {
        SkipprLogger::init();
        Registry::skipprd()->emergency($message);
    }

    public static function critical(string $message) : void
    {
        SkipprLogger::init();
        Registry::skipprd()->critical($message);
    }
}
