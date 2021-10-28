<?php


namespace Skipprd\Traits;


use Monolog\Registry;
use Skipprd\Commands\PipelineCommand;
use Skipprd\SkipprPack;

trait Threaded
{

    public function __construct()
    {

        if (Config::$mode == 'async') {

            $offsetsChannel = \parallel\Channel::make("input.offset", 100);
//            $this->offsetChannel = $offsetsChannel;

//            $inputThread = new \parallel\Runtime(__DIR__.'/../../../bootstrap/autoload.php');
//
//            $this->inputThread = $inputThread->run( function () use ($offsetsChannel) {
//
//                // Bootstrap thread
//                require __DIR__. '/../../../vendor/autoload.php';
//                $app = require_once __DIR__.'/../../../bootstrap/app.php';
//                $app->make(Kernel::class)->bootstrap();
//
//                SkipprLogger::info("Initialising input threads");
//
//                $pipeline = new PipelineCommand();
//
//                $pipeline->init();
//
//                // handle sigs
//                // PHP 7.1 and later can handle asynchronous signals natively
//                pcntl_async_signals(true);
//
//                pcntl_signal(SIGINT, [$pipeline, 'commit']); // Call $this->shutdown() on SIGINT
//                pcntl_signal(SIGTERM, [$pipeline, 'commit']); // Call $this->shutdown() on SIGTERM
//                pcntl_signal(SIGHUP, [$pipeline, 'commit']); // Call $this->shutdown() on SIGTERM
//
////                $this->offsetChannel = $offsetsChannel;
//
////                while (true) {
//
////                    SkipprLogger::debug("Starting threaded input worker");
//
//                    $pipeline->inputPlugin->sync($pipeline);
//
////                    $pipeline->inputBuffer->unlockAll('input');
//                    $pipeline->inputBuffer->flush("input");
////                    $pipeline->inputBuffer->finalise("input", true);
//
//                    $pipeline->pipelineModel->save();
//
////                    if ($pipeline->analysing) { // in case we didn't see enough messages
//
//                        SkipprLogger::info('Finished syncing data from input');
//
////                        $pipeline->shutdown();
////                        exit(0);
//
////                    }
////                }
//            });

            $ncpu = 4;
//
            if (Config::$analysing || Config::$mode == 'sync') {

                $ncpu = 1;

            } else {
                if (is_file('/proc/cpuinfo')) {

                    $cpuinfo = file_get_contents('/proc/cpuinfo');
                    preg_match_all('/^processor/m', $cpuinfo, $matches);
                    $ncpu = count($matches[0]);
                }
            }
//
            for ($i = 0; $i < $ncpu; $i++) {

                $this->threadPool[$i] = new \parallel\Runtime(__DIR__ . '/../../../bootstrap/autoload.php');

                $this->threadPool[$i]->run(function () use (
                    $i,
                    $offsetsChannel
                ) {

                    // Bootstrap thread
                    require __DIR__ . '/../../../vendor/autoload.php';
                    $app = require_once __DIR__ . '/../../../bootstrap/app.php';
                    $app->make(Kernel::class)->bootstrap();

                    SkipprLogger::info("Initialising ingest threads");

                    $pipeline = new PipelineCommand();

                    $pipeline->init();

                    // handle sigs
                    // PHP 7.1 and later can handle asynchronous signals natively
                    pcntl_async_signals(true);

                    pcntl_signal(SIGINT,
                        [
                            $pipeline,
                            'commit'
                        ]); // Call $this->shutdown() on SIGINT
                    pcntl_signal(SIGTERM,
                        [
                            $pipeline,
                            'commit'
                        ]); // Call $this->shutdown() on SIGTERM

                    $pipeline->offsetChannel = $offsetsChannel;

                    while (true) {

                        sleep(1);

                        try {

//                            SkipprLogger::debug("Starting threaded ingest worker $i");

                            $pipeline->inputBuffer->finalise();

                            $pipeline->process();

                        } catch (\Exception $e) {

                            SkipprLogger::error($e->getMessage());
                            SkipprLogger::error($e->getTraceAsString());

                        }
                    }

                });
            }

            $this->outputThread = new \parallel\Runtime(__DIR__ . '/../../../bootstrap/autoload.php');

            $this->outputThread->run(function () use ($offsetsChannel) {

                // Bootstrap thread
                require __DIR__ . '/../../../vendor/autoload.php';
                $app = require_once __DIR__ . '/../../../bootstrap/app.php';
                $app->make(Kernel::class)->bootstrap();

                SkipprLogger::info("Initialising output threads");

                $pipeline = new PipelineCommand();

                $pipeline->init();

                $pipeline->offsetChannel = $offsetsChannel;


                while (true) {

                    sleep(1);

                    try {

                        SkipprLogger::debug("Starting threaded output worker");

                        $pipeline->outputBuffer->finalise();

                        if (!empty($pipeline->outputPlugin)) {
                            $pipeline->outputPlugin->sync();

//                                $pipeline->offsetChannel->send($pipeline->outputPlugin->offset);
                        }


                    } catch (\Exception $e) {

                        SkipprLogger::error($e->getMessage());
                        SkipprLogger::error($e->getTraceAsString());

                    }
                }

            });

            /**
             * Manage threads - so to speak
             * - watch for offset events on channel and shutdown when quite
             * - @todo - emit some more concrete progress events and use them
             */

//            $lastEvent = time();
//
//            $events = new \parallel\Events();
//            $events->addChannel($offsetsChannel);
//            $events->setBlocking(false);
//
//            while (true) {
//
//                sleep(5);
//
//                if (($event = $events->poll()) != null) {
//
//                    SkipprLogger::info("EVENT");
//                    SkipprLogger::info(var_dump($event));
//
//                    // something happened, let's figure out what it was. First, we check the source.
//                    if ($event->object == $offsetsChannel) {
//                        // OK, now we must add our request to the queue, or return an output if we're already processing it
//                        if ($event->type == \Parallel\Events\Event\Type::Read) {
//
//                            $lastEvent = time();
//
//                            $offset = $event->value;
//
//                            SkipprLogger::info("Consumer received offset $offset");
//
//                            if ($this->inputPlugin->validateOffset($offset)) {
//
////                                $this->inputPlugin->commit($offset);
////
////                                $this->pipelineModel->offset = $offset;
////                                $this->pipelineModel->save();
//
////                                SkipprLogger::info("Committed offset $offset");
//                            }
//                        }
//
//                        $events->addChannel($offsetsChannel); // gets removed, so we put it back in.
//
//                    } else {
//                        // @todo - not currently implemented, the threads never terminate
//                        if ($event->object instanceof \parallel\Future) {
//
//                            if ($event->type == \Parallel\Events\Event\Type::Read) { // our task finished!
//
//                                SkipprLogger::info("Thread finished");
//
//                                $lastEvent = time();
//
//                            }
//                        }
//                    }
//                }
//
////                $elapsedTime = time() - $lastEvent;
////
////                SkipprLogger::info("Checking elapsed time since last offsets: $elapsedTime");
////
////                if ($elapsedTime > self::$flushTimeout) {
////
////                    SkipprLogger::info("Timeout reached waiting for new events");
////
////                    $offsetsChannel->close();
////                    $this->shutdown();
////                }
//
//            }

        }
    }

    public function process() : void
    {

        // @todo - decide if we'll support multi-threading and if so for which plugins???

        $offset = '';

        $bufferName = Config::$enableDeadLetters ? 'input' : 'deadletter';

//        while ($line = $this->inputPlugin->buffer->nextFile($bufferName)) {

        $i = 0;

        if (empty(Config::$sourceFormat)) {
            Config::$sourceFormat = $this->detectSerialisation();
        }

        while ($line = $this->inputPlugin->buffer->stream()) {

            $sp = new SkipprPack($line);
            $payload = $sp->decodeRecord();
            $offset = $sp->decodeOffset();

            $this->emitString($payload, $offset);

            $this->inputPlugin->buffer->commit();

//            $sourceMessages['skpr_event_ts'] = 0;
//            $sourceMessages['skpr_partition'] = '';
//            $this->serialiseOutput($sourceMessages, $offset);

            $i++;
        }

        if (!Config::$analysing) {


            $this->outputPlugin->buffer->flushAll();
            $this->deadletterPlugin->buffer->flushAll();

        }

    }

}