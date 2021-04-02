<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 20/11/2017
 * Time: 17:36
 */

namespace Skipprd\Commands;

use Monolog\Logger;
use Monolog\Registry;
use Skipprd\Arr;
use Skipprd\Buffers\BufferAdaptorsFactory;
use Skipprd\Plugins\PluginFactory;
use Aws\Exception\AwsException;
use Aws\Sqs\SqsClient;
use Carbon\Carbon;
use Skipprd\Buffers\FileBuffer;
use Skipprd\Helpers;
use Skipprd\Jobs\PodsStatus;
use Skipprd\SkipprPack;
use Skipprd\Str;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Ingest;
use Skipprd\Serders\SerdersFactory;
use Skipprd\Traits\Config;
use League\StatsD\Client as Statsd;
use Segment;

class PipelineCommand
{

//    use Dispatchable, InteractsWithQueue, Queueable;
//SerializesModels;

    use AnalyseSchema;
    use Ingest;
    use BufferAdaptorsFactory;
    
    protected $statsd = null;

    public $minSample = 10000;

    public $inputThread = null;

    public $threadPool = [];

    public $outputThread = null;

    public $consumerThread = null;

    public $offsetChannel = null;

    public $defaultMsg = [];

    public $log = null;

    /**
     * @var int - don't set below 20
     *    A. because that's too low for throughput
     *    B. because it won't allow time for new kafka topic creation before flush, so we loose first messages
     */
    public $flushInterval = 300;

//    public static $flushMaxMsg = 100000;
//    public static $flushMaxMsg = 10000;
    public static $flushMaxMsg = 5000;
//    public static $flushMaxMsg = 1;

    /**
     * @var int - seconds analysinc jobs has been running for
     */
    public $startTimestamp = 0;

    public $maxTime = 60;

    public $currentBytes = 0;

    public $lastFlushtimesamp = 0;

    public static $flushTimeout = 120; // shouldn't be greater than container shutdown delay
    /*
     * Source records, used to commit offsets to message bus
     */
    public $sourceRecords = [];

    public $entries = 0;

    public $totalEntries = 0;

    public $hashes = [];

    public $duplicateCount = 0;

    public $deadLetters = 0;

    /**
     * The name and signature of the subscriber command.
     *
     * @var string
     */
    protected $signature = 'iq:pipeline-command';

    /**
     * The subscriber description.
     *
     * @var string
     */
    protected $description = 'Run Pipeline Job from queue';

    protected $client;

    /**
     * @var array
     */
    protected $config = [];

    /**
     * @var \App\Plugins\DataSources\DataSourcePluginInterface
     */
    protected $inputPlugin = null;

    /**
     * @var \App\Plugins\DataOutputs\OutputPluginInterface
     */
    protected $outputPlugin = null;

    /**
     * @var \App\Plugins\DataOutputs\OutputPluginInterface
     */
    protected $deadletterPlugin = null;

    public $stream = null;

    /**
     * @var \Skipprd\BufferAdaptors\BufferInterface|null
     */
    protected $inputBuffer = null;

    /**
     * @var \Skipprd\BufferAdaptors\BufferInterface|null
     */
    protected $outputBuffer = null;
 /**
     * @var \Skipprd\BufferAdaptors\BufferInterface|null
     */
    protected $deadletterBuffer = null;

    /**
     * Create a new command instance.
     */

    /**
     * PipelineJob constructor.
     * @param $pluginModel DataSourcePluginInterface|OutputPluginInterface
     * @param array $config
     */
    public function __construct()
    {
        $application = new Logger('skipprd');
        Registry::addLogger($application);
//        Registry::skipprd()-> = new Logger(new \Monolog\Logger('Skippr Logger'));

    }

    protected function setPlugin() {

        $bufferType = 'file';

        if (!empty(Config::$timeFields) || !empty(Config::$entityNames)) {
            $bufferType = 'chunked';
        }

        //@todo - set $this->buffer->flushBytes in the output plugin
        $this->inputBuffer = BufferAdaptorsFactory::getAdaptor('input', 'file');
        $this->outputBuffer = BufferAdaptorsFactory::getAdaptor('output', $bufferType);
        $this->deadletterBuffer = BufferAdaptorsFactory::getAdaptor('deadletter', $bufferType);

        /**
         * Dead Letter Plugin
         */
        $deadLetterPluginName = Config::getenv('DEAD_LETTER_PLUGIN_NAME');

        if (!empty($deadLetterPluginName)) {

            $config = [];
            $envs = getenv();

            foreach ($envs as $key => $value) {
                if (strpos($key, 'DEAD_LETTER') > -1) {
                    $config[strtolower(substr($key,
                        strlen('DEAD_LETTER_')))] = $value;
                }
            }

            $deadLetterPluginName = Str::studly(ucwords(strtolower($deadLetterPluginName)));

        } else {

                $deadLetterPluginName = 'File';

                $config['path'] = '/dead-letters';

        }

        $deadLetterPluginClass = "Skipprd\\DataOutput" . "$deadLetterPluginName" . "\\DataOutput" . "$deadLetterPluginName" . "Plugin";

        $this->deadletterPlugin = new $deadLetterPluginClass($config, $this->deadletterBuffer);

        $this->deadletterPlugin->buffer->setSerde('json');
        

        if (Config::getenv('JOB_NAME') == 'deadletters') {

            Config::$enableDeadLetters = false;

        }

        $pluginName = Config::getenv('DATA_SOURCE_PLUGIN_NAME');

        $this->inputPlugin = PluginFactory::factory('data_source', $pluginName, $this->outputBuffer);


        $pluginName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');

        if (!empty($pluginName)) {

            $this->outputPlugin = PluginFactory::factory('data_output', $pluginName, $this->outputBuffer);

        } else {

            $this->outputPlugin = PluginFactory::factory('data_output', 'file', $this->outputBuffer);

        }

    }

    /**
     * Execute the console command.
     */
    public function handle()
    {
        
        Registry::skipprd()->info("Syncing");

        $this->init();

//        set_exception_handler([$this, 'exceptionHandler']);

        // handle sigs
        // PHP 7.1 and later can handle asynchronous signals natively
        pcntl_async_signals(true);

        pcntl_signal(SIGINT, [$this, 'shutdown']); // Call $this->shutdown() on SIGINT
        pcntl_signal(SIGTERM, [$this, 'shutdown']); // Call $this->shutdown() on SIGTERM


//        if (Config::$analysing) {
//            Config::$mode = 'sync';
//        }


        if (!Config::$enableDeadLetters) {

            Registry::skipprd()->info('Reprocessing dead letters');
        }

        $this->inputPlugin->sync($this);

        $this->inputPlugin->buffer->flushAll();

        if (Config::$analysing) { // in case we didn't see enough messages

            Registry::skipprd()->info('Finished analysing data');

        }

        if (Config::$mode == 'async') {

            // keep alive to control child threads
            while (true) {

                sleep(10);
            }
        }

        $this->shutdown();

    }

    public function exceptionHandler(\Exception $e) {

        Registry::skipprd()->error($e->getMessage());

        Registry::skipprd()->warning("Uncaught exception, shutting down all threads.");

        $this->shutdown();

    }

    /**
     * Execute the console command.
     */
    public function init()
    {

        Config::getConfig();

        Segment::init(Config::$segmentKey);

        Segment::identify([
            "userId" => hash('sha256', Config::$tenantId),
            "traits" => [
                "pipeline_name" => hash('sha256', Config::$pipelineName),
            ]
        ]);


        if (Config::getenv('STATSD_HOST') && Config::getenv('STATSD_PORT')) {

            // @todo - factory stats interface (statsd + skippr enterpise http endpoint)
            $this->statsd = new Statsd();

            $this->statsd->configure([
                'host' => Config::getenv('STATSD_HOST'),
                'port' => Config::getenv('STATSD_PORT'),
//            'namespace' => 'skippr'
            ]);
        }
        
        // setup global monolog
//        $application = new Logger('skipprd');
//        Registry::addLogger($application);

//        $this->pipelineModel = IngestJob::where('id', $this->pipelineId)->get()->first();

        // Update job status in Skippr Enterprise
        if (class_exists(PodsStatus::class)) {
            PodsStatus::dispatch();
        }

//        $config = $this->pipelineModel->buildJobConfig();

//        $this->setConfig($config);
//        $this->config = $config;

//        $config = [];
//        $this->getConfig($config);

        $this->setPlugin();

        $this->defaultMsg = $this->defaultMessage();

        $this->startTimestamp = Carbon::now()->timestamp;

        $this->connect();

//        $this->shutdown();

    }

    /**
     * @deprecated - @todo - we'll always pass the expected input format
     * Detect serialisation
     * Might be multiline json, or CSV. Pick enough rows to analyse/
     * Too any lines will cause delay and possibly OOM
     */
    public function detectSerialisation()
    {

        $lines = '';
        $i = 0;
        
        while ( ($line = $this->inputPlugin->buffer->stream() ) !== false ) {
            $i++;
            $lines .= $line;

            // grab enough lines, but not so many we get OOM
            // last one may get cut and be invalid
            if ($i == 1000) {

                break;
            }
        }
        
//        Serders::factory($lines, Config::$serder, $this->csvHeaders);
        return SerdersFactory::discover($lines);
        
    }

    public function serialiseOutput(array $payload) : void
    {

        try {

            $eventTime = $payload['skpr_event_ts'];
            $partition = $payload['skpr_partition'];

//            $serialised = json_encode($payload) . "\n";

//            $serder = SerdersFactory::factory(Config::$serder);
//            $serialised = $serder->serialize($payload) . "\n";


//            $serder = new SerderAvro($this->schema);
//            $serialised = $serder->serialize($payload);

//            $serialised = msgpack_pack($payload) . "\n";

//            $offset = (string) $offset;
//            $payload = $this->encode($serialised, $offset);

//            $offset = (string) $offset;
//            $sp = new SkipprPack();
//            $sp->encode($payload, $offset);
//            $serialised = $sp->string();


            if (Config::$mode == 'sync') {

//                $this->outputPlugin->buffer->append($serialised, false, $eventTime, $partition);
                $this->outputPlugin->buffer->append($payload, false, $eventTime, $partition);

                if (!empty($this->outputPlugin)) {

                    $this->flushBuffer();

                }

//                    $this->inputPlugin->setOffsets($this->outputPlugin->offset);

            } elseif (Config::$mode == 'async') {

                $this->outputPlugin->buffer->append($payload, false, $eventTime, $partition);

            }

            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

            if (!empty($this->statsd)) {
                $this->statsd->increment("ingest.msgs.current.$tenantId.$pipelineName", 1);
            }

//            $this->statsd->increment("ingest.msgs.current.{Config::$tenantId }.{Config::$pipelineName}", 1);

//            $flushedCount++;


        } catch (\AvroException $e) {

//            Registry::skipprd()->error($e->getMessage());

            try {

                $this->deadLetterMessage($payload);


            } catch (\Exception $e) {

                Registry::skipprd()->emergency('Failed to write to dead letter queue');
                Registry::skipprd()->error($e->getMessage());
            }

        } catch (\Exception $e) {

            Registry::skipprd()->error($e->getMessage());
        }

    }



    public function flushBuffer()
    {

        $flushBytes = $this->outputPlugin->buffer->flushBytes;

//        $timeFlush = (Carbon::now()->timestamp - $this->lastFlushtimesamp) > $this->flushInterval ? true : false;
        $timeFlush = false;
        $byteFlush = $this->currentBytes >= $flushBytes ? true : false;
//        $msgCountFlush = $this->entries >= self::$flushMaxMsg ? true : false;
        $msgCountFlush = false;

        if ($timeFlush || $byteFlush || $msgCountFlush) {

            if ($timeFlush) {
                Registry::skipprd()->debug("Flush trigger by: time interval");
            }
            if ($byteFlush) {
                Registry::skipprd()->debug("Flush trigger by: byte size");
            }
            if ($msgCountFlush) {
                Registry::skipprd()->debug("Flush trigger by: message count");
            }

            if ($this->entries == 0) {
                return false;
            }

//            Registry::skipprd()->debug("Msg Bytes Current: " . BytesToHuman::toHuman($this->currentBytes, true, 'MB'));
//            Registry::skipprd()->debug("Msg Bytes Limit: " . BytesToHuman::toHuman($this->flushBytes, true, 'MB'));

            $this->outputPlugin->buffer->unlockAll();
//            $this->outputPlugin->buffer->flush("out");
            $this->outputPlugin->buffer->flushAll();
            $this->outputPlugin->buffer->finalise();


            $this->deadletterPlugin->buffer->unlockAll();
//            $this->deadletterPlugin->buffer->flush("deadletter");
            $this->deadletterPlugin->buffer->flushAll();
            $this->deadletterPlugin->buffer->finalise();

            $this->deadletterPlugin->sync(Config::$outputFormat, Config::$avroSchema);

            $this->outputPlugin->sync(Config::$outputFormat, Config::$avroSchema);

            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

            if (!empty($this->statsd)) {
                $this->statsd->increment("flushed.msgs.current.{$pipelineName}.{$pipelineName}",
                    $this->entries);

                $this->statsd->increment("flushed.deadletters.current.{$tenantId }.{$pipelineName}",
                    $this->deadLetters);
            }

//                            $this->entries = 0;
//                            $this->deadLetters = 0;


            $flushedCount = 0;

//            $flushDocsCnt = $flushDocs->count();

            Registry::skipprd()->info("Flushing " . $this->entries . " messages");

            // Empty only after writing, will ensure still available for graceful shutdown
            $this->totalEntries += $this->entries;
            $this->entries = 0;
            $this->hashes = [];
            $this->duplicateCount = 0;
            $flushDocs = [];


            $this->lastFlushtimesamp = Carbon::now()->timestamp;
            $this->currentBytes = 0;

            return true;

        }

        return false;
    }

    public function deadLetterMessage(array $message)
    {

        // Don't dead letter message, if running the dead letter job
        // it will be skipped and so just remain in the queue
        if (Config::$enableDeadLetters) {

            $deadLetterTopic = 'raw_' . Config::$tenantId  . '_' . Config::$pipelineName .'_deadletter';

//            $serialised = json_encode($message);

//            $sp = new SkipprPack();
//            $sp->encode($serialised, $offset);
//            $payload = $sp->string() . "\n";

//            $this->deadletterPlugin->buffer->append($payload);
            $this->deadletterPlugin->buffer->append($message);

            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

            if (!empty($this->statsd)) {
                $this->statsd->increment("ingest.deadletters.current.$tenantId.$pipelineName",
                    1);

                $this->statsd->increment("ingest.deadletters.total.$tenantId.$pipelineName",
                    1);
            }

            $this->deadLetters++;

        } else {

            Registry::skipprd()->critical('Schema not valid for events in dead letter queue');

//            $this->inputPlugin->buffer->unlockAll('deadletter');
//            $this->inputPlugin->buffer->flush("deadletter");
//            $this->inputPlugin->buffer->finalise("deadletter", true);

            $this->shutdown();

//            exit(0);
        }

    }

    public function emit(string $payload) : void
    {

        if ($payload != '') {
            
            if (Config::$mode == 'sync') {

                if (!Config::$enableDeadLetters) {

                    // @todo - deprecate SkipprPack for Apache Arrow
                    $sp = new SkipprPack($payload);
                    $payload = $sp->decodeRecord();
//                $offset = $sp->decodeOffset();
                }

                $this->parseLine($payload);


            } elseif (Config::$mode == 'async') {

//                $offset = (string) $offset;
//                $sp = new SkipprPack();
//                $sp->encode($payload, $offset);
//                $payload = $sp->string();


                // Limit input buffer size to prevent flooding disk
                // and allow ingest threads to catch up
//            while($this->inputPlugin->buffer->bufferGetSize('input') > $this->inputBuffMaxBytes) {
//            while($this->inputPlugin->buffer->bufferGetSize('input') > count($this->threadPool)) {
//                sleep(1);
//            }

                $this->inputPlugin->buffer->append($payload);

            }

        }



        if (!Config::$analysing) {

//            $this->inputPlugin->commit($offset);

            if (!isset($this->j)) {
                $this->j = 0;
            } else {
                $this->j++;
            }

            if ($this->j > self::$flushMaxMsg) {

                $this->j = 0;

//                $this->pipelineModel->save();
                // @todo - implement state/mapping storage
                
            }
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

            $this->parseLine($payload, $offset);

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

    public function parseLine(string $payload) {


//                    $payload = $record;

        $this->currentBytes += strlen($payload);
//
//        if (empty(Config::$sourceFormat)) {
//
//            $this->inputPlugin->buffer->append($payload, true);
//            $this->inputPlugin->buffer->finalise(true);
//
//            Config::$sourceFormat = $this->detectSerialisation();
//
//        } else {

        // @todo - initialise in class global scope
            $serder = SerdersFactory::factory(Config::$sourceFormat);
            $sourceMessages = $serder->deserialize($payload);

            $this->parse($sourceMessages);

//        if (Config::$mode == 'sync' && !Config::$analysing) {
//            $this->statsd->increment("ingest.msgs.current.{Config::$tenantId }.{Config::$pipelineName}", 1);
//        }
//        }


    }

    public function parse(array $payload) {

        $unwrappedMessages = $this->unwrap($payload);

        foreach ($unwrappedMessages as $unwrappedMessage) {  // outer array

            if (Config::$analysing) {

                if (is_array($unwrappedMessage)) {

                    $this->getIdFields($unwrappedMessage);
                    
                    $this->analysePayload($unwrappedMessage, Config::$discoveredFieldOccurrence);
                }

                if ($this->i > $this->minSample
                    || Carbon::now()->timestamp - $this->startTimestamp > $this->maxTime) {

                    Registry::skipprd()->info('Finished analysing data');

//                    unlink("buffer.ready"); // clean up ready buffer - as we force exit here

                    $this->shutdown();
                }
            }


            if (!Config::$analysing) {

                // Ensure time for new topic creation before first flush
                if ($this->lastFlushtimesamp == 0) {
                    $this->lastFlushtimesamp = Carbon::now()->timestamp;
                }
                
                if (is_array($unwrappedMessage)) {

                    $this->parseTimeField($unwrappedMessage);
                    $this->parsePartitionField($unwrappedMessage);

                    $message = $this->ingestPayload($unwrappedMessage, Config::$discoveredFieldOccurrence);

                    if ($message) {
                        $this->serialiseOutput($message);
                    } else {
                        $this->deadLetterMessage($unwrappedMessage);
                    }
                } else {
                    Registry::skipprd()->info("found non-array message");
                    Registry::skipprd()->info($unwrappedMessage);
                }

            }
        }

    }

    public function parsePartitionField(&$message)
    {

        $shardFieldEntityValue = '';
        if (!empty(Config::$entityNames)) {
            foreach (Config::$entityNames as $entityField) {

                $shardFieldName = $entityField;
                // @todo - support entity naming
                $entityName = $entityField;

//                    if (!empty($message[$shardFieldName])) {
                if ($entityValue = array_get($message, $entityField, false) ) {

                    $shardFieldEntityValue .= '-' . str_slug($entityName, '_') . '=' . str_slug($entityValue);
                }
            }
        }


        $shardFieldEntityValue = strtolower(trim($shardFieldEntityValue, '-'));

        $message['skpr_partition'] = $shardFieldEntityValue;

    }

    public function parseTimeField(&$message)
    {

        // default to beginning of epoch.
        $message['skpr_event_ts'] = 0;

        // Support nested time fields via array dot notation
        // For user confirmed event time fields, use the first one that matches
        foreach (Config::$timeFields as $field_dot) {


            if ($time_value = Arr::get($message, $field_dot, false) ) {
                $message['skpr_event_ts'] = $time_value;
                break;
            }

        }

        // Handle millisecond timestamps
        if (strlen((string) $message['skpr_event_ts']) == 13) {
            $message['skpr_event_ts'] = floor($message['skpr_event_ts'] / 1000);
        }

        // Handle datetime strings
        if (gettype($message['skpr_event_ts']) == 'string') {
            $message['skpr_event_ts'] = Carbon::parse($message['skpr_event_ts'])->timestamp;
        }

    }

    public function unwrap($sourceMessages)
    {

        if (!empty($sourceMessages) && is_array($sourceMessages)) {

            if (!empty(Config::$eventPath)) {

                foreach ($sourceMessages as $sourceMessage) {
                    try {

                        $unwrappedMessages = Arr::get($sourceMessage, Config::$eventPath);

                    } catch (\Exception $e) {
                        Registry::skipprd()->error("Could not find field path " . Config::$eventPath . " in message.");

                    }

                }
            }
        }

        if (empty($unwrappedMessages)) {

            $unwrappedMessages = $sourceMessages;
        }
        
        return $unwrappedMessages;
    }

    public function connect()
    {
        // mocking a stream is best way to deal with new line chars
//        $this->stream = fopen("php://temp", 'w+');
//
//        $sourceStream = $this->pluginModel->sync($this->config);
//        rewind($sourceStream);
//        $bytes = stream_copy_to_stream($sourceStream, $this->stream);


        $this->inputPlugin->buffer->flushAll();
        $this->outputPlugin->buffer->flushAll();
        $this->deadletterPlugin->buffer->flushAll();

//        $this->pipelineModel->save();
        // @todo - implement state storage

//        $this->inputPlugin->commit(Config::$offsets);
        $this->inputPlugin->offsets->setOffsets(Config::$offsets);

        $this->inputPlugin->connect();
    }

    public function getUnwrappedMetadata()
    {

        $metadata = Config::$discoveredFieldOccurrence;

        if (!empty(Config::$eventPath)) {

            $parts = explode(".", Config::$eventPath);
            $lng = count($parts);

            for ($i = 0; $i <= $lng; $i++) {

                if ($i < $lng) {
                    $metadata = $metadata[$parts[$i]]['fields'];
                } else {
                    $metadata = $metadata['a0']['fields'];
                }
            }
        }

        return $metadata;

    }

    public function updateUnwrappedMetadata($metadata)
    {

        if (!empty(Config::$eventPath)) {

            $parts = array_reverse(explode(".", Config::$eventPath));
            $lng = count($parts);

            for ($i = $lng; $i <= 0; $i++) {

                if ($i == $lng) { // first
                    $newMetadata = [];
                    $newMetadata['a0']['fields'] = $metadata;
                    $metadata = $newMetadata;

                } else if ($i == 0) { // last
                    Config::$discoveredFieldOccurrence[$parts[$i]]['fields'] = $metadata;

                } else {
                    $newMetadata = [];
                    $newMetadata[$parts[$i]]['fields'] = $metadata;
                    $metadata = $newMetadata;
                }
            }
        }

    }

    public function shutdown()
    {

        Registry::skipprd()->info("Gracefully shutting down and flushing buffers");

        if (Config::$mode == 'async') {

            if ($this->inputThread !== null) {
                $this->inputThread->cancel();
            }
        }

        if ($this->inputPlugin !== null) {

            $this->inputPlugin->shutdown();
        }

        if (Config::$mode == 'async') {

            $this->inputPlugin->buffer->unlockAll();
//            $this->inputPlugin->buffer->flush("input");
            $this->inputPlugin->buffer->flushAll(true);
            $this->inputPlugin->buffer->finalise(true);

            if ($this->threadPool !== null) {

                foreach ($this->threadPool as $thread) {
                    $thread->kill();
                }
            }

            if ( $this->outputThread != null) {
                $this->outputThread->kill();
            }
        }

        if (!Config::$analysing) {
            $this->outputPlugin->buffer->unlockAll();
//            $this->outputPlugin->buffer->flush("out");
            $this->outputPlugin->buffer->flushAll(true);
            $this->outputPlugin->buffer->finalise(true);

            $this->deadletterPlugin->buffer->unlockAll();
            $this->deadletterPlugin->buffer->flushAll(true);
            $this->deadletterPlugin->buffer->finalise(true);

            $this->totalEntries += $this->entries;
            
            Config::$offsets = $this->inputPlugin->offsets->getOffsets();
        }
        
        if (Config::$mode == 'sync') {

            if (!empty($this->outputPlugin)) {
                
                Registry::skipprd()->info("Syncing remaining output buffers to destination.");

                if (!Config::$analysing) {

                    $this->outputPlugin->sync(Config::$outputFormat, Config::$avroSchema);

                    $this->deadletterPlugin->sync(Config::$outputFormat, Config::$avroSchema);
                }
                $this->outputPlugin->shutdown();
                $this->deadletterPlugin->shutdown();
            }
        }

//        $this->pipelineModel->save(); // commit offsets
          // @todo - implement state storage

        Registry::skipprd()->info("Ingested " . $this->totalEntries . " messages");
        Registry::skipprd()->info("Dead Letters " . $this->deadLetters . " dead letters");

//        $this->updateDeadLetterQueueSize();

        if (!empty(Config::$discoveredFieldOccurrence)) {

            $this->finaliseFieldMapping();

            $this->writeMapping();

        } else {
            Registry::skipprd()->info("No fields found when analysing schema, did you send some data?");
        }

//        Registry::skipprd()->debug("Mem used: " . BytesToHuman::toHuman(memory_get_usage(true), true, 'MB'));
//        Registry::skipprd()->debug("Mem limit: " . BytesToHuman::toHuman($this->flushBytes, true, 'MB'));

        // Update metadata such as field mapping
//        $configYml = $this->setConfig();
//        $configYml['update_status'] = true;
//        $configYml['status'] = false;
//        event(new WorkerConfigRequested($configYml));

        // Update Job Status
        if (class_exists(PodsStatus::class)) {
            PodsStatus::dispatch();
        }

        $inputName = Config::getenv('DATA_SOURCE_PLUGIN_NAME');
        $outputName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');

        Segment::track(array(
            "userId" => hash('sha256', Config::$tenantId),
            'event' => 'shutdown',
            "properties" => [
                "msgs_total" => $this->totalEntries,
                "deadletters_total" => $this->deadLetters,
                "input_plugin" => $inputName,
                "output_plugin" => $outputName,
                "input_format" => Config::getenv('DATA_SOURCE_FORMAT'),
                "output_format" => Config::getenv('DATA_OUTPUT_FORMAT'),
                "skippr_version" => Config::getenv('SKIPPR_BUILD_VERSION'),
                "tenant_id" => hash('sha256', Config::$tenantId),
                "pipeline_name" => hash('sha256', Config::$pipelineName),
            ]
        ));

        Registry::skipprd()->info("Graceful shutdown complete, bye");

//        $this->delete();
        exit(0);
//        return;
    }

    public function writeMapping()
    {

//        $configYml = Config::setConfig();

//        $configYml['config_updated'] = microtime(true);

        $validDateFieldCandidates = [];
        foreach (Config::$dateFieldCandidates as $field => $candidateField) {
            if (!empty($candidateField['field'])) {
                $validDateFieldCandidates[$candidateField['field']] = $candidateField;
            }
        }

        Config::$discoveredFieldOccurrence['date_field_candidates'] = $validDateFieldCandidates;

        Config::$discoveredFieldOccurrence['enitity_field_candidates'] = Config::$idFields;

//        event(new WorkerConfigRequested($configYml));

        $configYml = Config::setConfig();

        Registry::skipprd()->info("Updated analysed field schema");

    }

    public function finaliseFieldMapping()
    {

        self::determineFieldTypes(Config::$discoveredFieldOccurrence);

        $this->findMessageIdField();

        Config::$analysing = false;

    }
    
    public function findMessageIdField()
    {

        $enitityFieldCandidates = [];

        foreach (Config::$idFields as $fieldName => $ids) {

            // 95% of this fields ID's are unique, it's probably a message ID field
            if (count(Config::$idFields[$fieldName]) / $this->minSample * 100 >= 70) {
                unset(Config::$idFields[$fieldName]);

            } else {
                $enitityFieldCandidates[$fieldName] = [];
            }
        }

        Config::$idFields = $enitityFieldCandidates;

        $numCandidates = count(Config::$idFields);
        Registry::skipprd()->info("Found $numCandidates ID fields");
    }

    public static function determineFieldTypes(&$array, $parent_type = null) {

        $dateTypes = ['date', 'timestamp', 'timestamp_milli'];

        foreach ($array as $fieldName => $field) {

            // Useful for field evolution logic for maps, which only support one sub-field type
            if ($parent_type !== null) {
                $array[$fieldName]['parent_type'] = $parent_type;
            }

            if (empty($array[$fieldName]['determined_type'])) {

                $highestType = '';
                $highestCount = 0;

                if (!empty($field['type'])) {

                    foreach ($field['type'] as $dataType => $dataTypeCount) {

                        if ($highestCount < $dataTypeCount) {

                            // Prefer primitive type to date type
                            // - if there's multiple discovered types
                            // - and the most common type is a date type
                            // - select the next most common, non-date type
                            if (count($field['type']) == 1 || (count($field['type']) > 1 && !in_array($dataType, $dateTypes))) {
                                $highestType = $dataType;
                                $highestCount = $dataTypeCount;
                            }
                        }
                    }

                    $array[$fieldName]['determined_type'] = $highestType;

                }
            }

            if (!empty($array[$fieldName]['determined_type'])
                && in_array($array[$fieldName]['determined_type'], ['map', 'array', 'record'])) {

                if (!empty($field['fields'])) {

                    if ($array[$fieldName]['determined_type'] == 'array') {

//                        && (!empty($array[$fieldName]['fields'][0]['determined_type'])
//                            && in_array($array[$fieldName]['fields'][0], ['map', 'array', 'record'])) ) {

                            $array[$fieldName]['determined_type_values'] = null;

                            // Ignore sub-fields for Avro array, the values are just enumerated, their not fields themselves.
                            // Else we'd create a field list with string keys for each array value
                            // e.g. [1,5,3,7,4,3,5]
                            // would incorrectly become ['a0' => 1, 'a1' => 5, ...]

                            $typeCount = [];

                            // @todo - not intended to build avro type array here
                            //         however, 'array' type is a special case... how to handle?

                            // Get avro arrays items primitive data type
                            foreach ($array[$fieldName]['fields'] as $sub_field) {

                                foreach ($sub_field['type'] as $dataType => $dataTypeCount) {

                                    if (!in_array($dataType, $dateTypes)) {

                                        if (empty($typeCount[$dataType])) {
                                            $typeCount[$dataType] = $dataTypeCount;

                                        } else {
                                            $typeCount[$dataType] += $dataTypeCount;
                                        }
                                    }

                                }

                            }

                            arsort($typeCount);

                            $valueTypes = array_key_first($typeCount);

//                            if (!empty($array[$fieldName]['determined_type_values'])
//                                && in_array($array[$fieldName]['determined_type_values'], ['map', 'array', 'record']) ) {
//
//                                self::determineFieldTypes($array[$fieldName]['fields'],
//                                    $array[$fieldName]['determined_type']);
//
//                            } else {

                                $array[$fieldName]['determined_type_values'] = $valueTypes;

                                $array[$fieldName]['fields'] = [];
//                            }


                    } else {

                        self::determineFieldTypes($array[$fieldName]['fields'],
                            $array[$fieldName]['determined_type']);

                    }
                }
//                else {
//
//                    unset($array[$fieldName]);
//                }
            }
        }

    }

    public function getIdFields($message)
    {

        $messageEntityNames = [];

        if (!empty($message)) {

            foreach ($message as $fieldName => $value) {

                $fieldName = Helpers::cleanFieldName($fieldName);

                $fieldHaystack = Helpers::explodeField($fieldName);

                $haystack = [
                    'id',
                    'uuid',
                    'guid',
                    'uid',
                    'mac',
                    'namespace',
                    'ns'
                ];
                $matches = array_intersect($fieldHaystack, $haystack);

                if (!empty($matches)) {

                    // use field name as entity name,
                    if (count($fieldHaystack) > 1) {
                        $fieldHaystack = array_flip($fieldHaystack);
                        foreach ($matches as $match) {
                            unset($fieldHaystack[$match]);
                        }
                        $fieldHaystack = array_flip($fieldHaystack);
                    }

                    $entityName = implode('_', $fieldHaystack);

                    $messageEntityNames[$entityName] = $fieldName;

                    Config::$idFields[$fieldName][$value] = $value;

//                    Config::$entityNames[$entityName] = $fieldName;

//                    $this->mapping['properties'][$fieldName]['type'] = 'keyword';

//                    $this->mapping = $this->dataTypeMappings['parent'];
                }
            }
        }

        // @todo - return not used anymore
        return $messageEntityNames;
    }

    /**
     * @param array $message
     * @return bool
     */
    private function isDuplicate(array $message) : bool
    {

        $return = true;

        // expects string, e.g. serialised json
        $hash = md5(json_encode($message));
        if (isset($this->hashes[$hash])) {

            $this->duplicateCount++;

        } else {
            $this->hashes[$hash] = 0;

            $return = false;
        }

        return $return;
    }
}

