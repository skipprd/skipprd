<?php


namespace Skipprd\Traits;


use Monolog\Registry;
use Skipprd\Commands\PipelineCommand;

trait Threaded
{

    public function __construct()
    {

        if ($this->mode == 'async') {

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
//                $this->log->info("Initialising input threads");
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
////                    $this->log->debug("Starting threaded input worker");
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
//                        $this->log->info('Finished syncing data from input');
//
////                        $pipeline->shutdown();
////                        exit(0);
//
////                    }
////                }
//            });

            $ncpu = 4;
//
            if (Config::$analysing || $this->mode == 'sync') {

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

                    $this->log->info("Initialising ingest threads");

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

//                            $this->log->debug("Starting threaded ingest worker $i");

                            $pipeline->inputBuffer->finalise();

                            $pipeline->process();

                        } catch (\Exception $e) {

                            $this->log->error($e->getMessage());
                            $this->log->error($e->getTraceAsString());

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

                $this->log->info("Initialising output threads");

                $pipeline = new PipelineCommand();

                $pipeline->init();

                $pipeline->offsetChannel = $offsetsChannel;


                while (true) {

                    sleep(1);

                    try {

                        Registry::applog()
                            ->debug("Starting threaded output worker");

                        $pipeline->outputBuffer->finalise();

                        if (!empty($pipeline->outputPlugin)) {
                            $pipeline->outputPlugin->sync();

//                                $pipeline->offsetChannel->send($pipeline->outputPlugin->offset);
                        }


                    } catch (\Exception $e) {

                        $this->log->error($e->getMessage());
                        $this->log->error($e->getTraceAsString());

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
//                    $this->log->info("EVENT");
//                    $this->log->info(var_dump($event));
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
//                            $this->log->info("Consumer received offset $offset");
//
//                            if ($this->inputPlugin->validateOffset($offset)) {
//
////                                $this->inputPlugin->commit($offset);
////
////                                $this->pipelineModel->offset = $offset;
////                                $this->pipelineModel->save();
//
////                                $this->log->info("Committed offset $offset");
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
//                                $this->log->info("Thread finished");
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
////                $this->log->info("Checking elapsed time since last offsets: $elapsedTime");
////
////                if ($elapsedTime > self::$flushTimeout) {
////
////                    $this->log->info("Timeout reached waiting for new events");
////
////                    $offsetsChannel->close();
////                    $this->shutdown();
////                }
//
//            }

        }
    }
}