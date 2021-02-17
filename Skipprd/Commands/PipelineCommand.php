<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 20/11/2017
 * Time: 17:36
 */

namespace Skipprd\Commands;

use Illuminate\Log\Logger;
use Skipprd\Plugins\PluginFactory;
use Aws\Exception\AwsException;
use Aws\Sqs\SqsClient;
use Carbon\Carbon;
use Skipprd\BufferAdaptors\BufferAdaptorsFactory;
use Skipprd\Helpers;
use Skipprd\Jobs\PodsStatus;
use Skipprd\SkipprPack;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Ingest;
use Skipprd\Serders\SerdersFactory;
use Skipprd\Traits\Config;
use League\StatsD\Laravel5\Facade\StatsdFacade as Statsd;

class PipelineCommand
{

//    use Dispatchable, InteractsWithQueue, Queueable;
//SerializesModels;

    use AnalyseSchema;
    use Ingest;
    use BufferAdaptorsFactory;

    protected $pipelineModel;

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
     *    B. because it won't allow time for new topic creation before flush, so we loose first messages
     */
    // @todo - find where performance drops of for number of cached entries
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
     * @var \Skipprd\BufferAdaptors\Buffer|null
     */
    protected $inputBuffer = null;

    /**
     * @var \Skipprd\BufferAdaptors\Buffer|null
     */
    protected $outputBuffer = null;
 /**
     * @var \Skipprd\BufferAdaptors\Buffer|null
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

        $this->log = new Logger(new \Monolog\Logger('Skippr Logger'));

    }

    protected function setPlugin() {

        $path = 'raw_' . Config::$tenantId . '_' . Config::$pipelineName .'_deadletter';

        $config = [];
        $config['data_source_s3_bucket'] = 'skpr-deadletters';
        $config['data_source_s3_region'] = getenv('AWS_REGION');
        $config['data_source_aws_access_id'] = getenv('AWS_ACCESS_KEY_ID');
        $config['data_source_aws_secret_key'] = getenv('AWS_SECRET_ACCESS_KEY');
        $config['data_source_s3_prefix'] = $path;

        $this->deadletterPlugin = PluginFactory::factory('data_output', 's3_bucket', $config, $this->deadletterBuffer);

        if (getenv('JOB_NAME') == 'deadletters') {

            $this->inputPlugin = PluginFactory::factory('data_source', 's3', $config, $this->inputBuffer);

            Config::$enableDeadLetters = false;

        }
        else {

            $pluginName = getenv('DATA_SOURCE_PLUGIN_NAME');

            $config = [];

            foreach (getenv() as $name => $value) {
                if (strpos($name, 'DATA_SOURCE') > -1) {
                    $config[strtolower(substr($name, strlen('DATA_SOURCE_')))] = $value;
                }
            }

            $this->inputPlugin = PluginFactory::factory('data_source', $pluginName, $config, $this->outputBuffer);
        }

        $pluginName = getenv('DATA_OUTPUT_PLUGIN_NAME');

        if (!empty($pluginName)) {

            $config = [];

            foreach (getenv() as $name => $value) {
                if (strpos($name, 'DATA_OUTPUT') > -1) {
                    $config[strtolower(substr($name, strlen('DATA_OUTPUT_')))] = $value;
                }
            }

            $this->outputPlugin = PluginFactory::factory('data_output', $pluginName, $config, $this->outputBuffer);
        }

    }

    /**
     * Execute the console command.
     */
    public function handle()
    {
        
        $this->log->info("Syncing");

        $this->init();

//        set_exception_handler([$this, 'exceptionHandler']);

        // handle sigs
        // PHP 7.1 and later can handle asynchronous signals natively
        pcntl_async_signals(true);

        pcntl_signal(SIGINT, [$this, 'shutdown']); // Call $this->shutdown() on SIGINT
        pcntl_signal(SIGTERM, [$this, 'shutdown']); // Call $this->shutdown() on SIGTERM


//        if (Config::$analysing) {
//            $this->mode = 'sync';
//        }


        if (!Config::$enableDeadLetters) {

            $this->log->info('Reprocessing dead letters');
        }

        $this->inputPlugin->sync($this);

        $this->inputBuffer->flushAll();

        if (Config::$analysing) { // in case we didn't see enough messages

            $this->log->info('Finished analysing data');

        }

        if ($this->mode == 'async') {

            // keep alive to control child threads
            while (true) {

                sleep(10);
            }
        }

        $this->shutdown();

    }

    public function exceptionHandler(\Exception $e) {

        $this->log->error($e->getMessage());

        $this->log->warning("Uncaught exception, shutting down all threads.");

        $this->shutdown();

    }

    /**
     * Execute the console command.
     */
    public function init()
    {

        Config::getConfig();

        // setup global monolog
//        $application = new Logger('applog');
//        Registry::addLogger($application);

//        $this->pipelineModel = IngestJob::where('id', $this->pipelineId)->get()->first();

        // Update Job Status
        if (class_exists(PodsStatus::class)) {
            PodsStatus::dispatch();
        }

//        $config = $this->pipelineModel->buildJobConfig();

//        $this->setConfig($config);
//        $this->config = $config;

//        $config = [];
//        $this->getConfig($config);

        $bufferType = 'file';

        if (!empty(Config::$timeFields) || !empty(Config::$entityNames)) {

            $bufferType = 'chunked';
        }

        //@todo - set $this->buffer->flushBytes in the output plugin
        $this->inputBuffer = BufferAdaptorsFactory::getAdaptor('input', 'file');
        $this->outputBuffer = BufferAdaptorsFactory::getAdaptor('output', $bufferType);
        $this->deadletterBuffer = BufferAdaptorsFactory::getAdaptor('deadletter', $bufferType);

        $this->setPlugin();

        $this->defaultMsg = $this->defaultMessage();

        $this->startTimestamp = Carbon::now()->timestamp;

        $this->connect();

//        $this->shutdown();

    }

    /**
     * Detect serialisation
     * Might be multiline json, or CSV. Pick enough rows to analyse/
     * Too any lines will cause delay and possibly OOM
     */
    public function detectSerialisation()
    {

        $lines = '';
        $i = 0;
        
        while ( ($line = $this->inputBuffer->stream() ) !== false ) {
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

    public function serialiseOutput(array $payload, string $offset) : void
    {

        try {

            $eventTime = $payload['skpr_event_ts'];
            $partition = $payload['skpr_partition'];

            $serialised = json_encode($payload) . "\n";

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


            if ($this->mode == 'sync') {

                $this->outputBuffer->append($serialised, false, $eventTime, $partition);

                if (!empty($this->outputPlugin)) {

                    $this->flushBuffer();

                    if (!Config::$enableDeadLetters) {
                        $this->pipelineModel->dl_offset = $offset;

                    } else {
                        $this->pipelineModel->offset = $offset;

                    }

                }

//                    $this->inputPlugin->commit($this->outputPlugin->offset);

            } elseif ($this->mode == 'async') {

                $this->outputBuffer->append($serialised, false, $eventTime, $partition);

            }

            Statsd::increment("ingest.msgs.current.{Config::$tenantId }.{Config::$pipelineName}", 1);


//            Statsd::increment("ingest.msgs.current.{Config::$tenantId }.{Config::$pipelineName}", 1);

//            $flushedCount++;


        } catch (\AvroException $e) {

//            $this->log->error($e->getMessage());

            try {

                $this->deadLetterMessage($payload, $offset);


            } catch (\Exception $e) {

                $this->log->emergency('Failed to write to dead letter queue');
                $this->log->error($e->getMessage());
            }

        } catch (\Exception $e) {

            $this->log->error($e->getMessage());
        }

    }



    public function flushBuffer()
    {

        $flushBytes = $this->outputPlugin->buffer->flushBytes;

        $timeFlush = (Carbon::now()->timestamp - $this->lastFlushtimesamp) > $this->flushInterval ? true : false;
        $byteFlush = $this->currentBytes >= $flushBytes ? true : false;
//        $msgCountFlush = $this->entries >= self::$flushMaxMsg ? true : false;
        $msgCountFlush = false;

        if ($timeFlush || $byteFlush || $msgCountFlush) {

            if ($timeFlush) {
                $this->log->debug("Flush trigger by: time interval");
            }
            if ($byteFlush) {
                $this->log->debug("Flush trigger by: byte size");
            }
            if ($msgCountFlush) {
                $this->log->debug("Flush trigger by: message count");
            }

            if ($this->entries == 0) {
                return false;
            }

//            $this->log->debug("Msg Bytes Current: " . BytesToHuman::toHuman($this->currentBytes, true, 'MB'));
//            $this->log->debug("Msg Bytes Limit: " . BytesToHuman::toHuman($this->flushBytes, true, 'MB'));

            $this->outputBuffer->unlockAll();
//            $this->outputBuffer->flush("out");
            $this->outputBuffer->flushAll();
            $this->outputBuffer->finalise();


            $this->deadletterBuffer->unlockAll();
//            $this->deadletterBuffer->flush("deadletter");
            $this->deadletterBuffer->flushAll();
            $this->deadletterBuffer->finalise();

            $this->deadletterPlugin->sync();

            $this->outputPlugin->sync(Config::$serder, Config::$schema);

            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

            Statsd::increment("flushed.msgs.current.{$pipelineName}.{$pipelineName}", $this->entries);

            Statsd::increment("flushed.deadletters.current.{$tenantId }.{$pipelineName}", $this->deadLetters);

//                            $this->entries = 0;
//                            $this->deadLetters = 0;


            $flushedCount = 0;

//            $flushDocsCnt = $flushDocs->count();

            $this->log->info("Flushing " . $this->entries . " messages");

            // Empty only after writing, will ensure still available for graceful shutdown
//            $this->entries = 0;
            $this->hashes = [];
            $this->duplicateCount = 0;
            $flushDocs = [];


            $this->lastFlushtimesamp = Carbon::now()->timestamp;
            $this->currentBytes = 0;

            return true;

        }

        return false;
    }

    public function deadLetterMessage(string $message, string $offset)
    {

        // Don't dead letter message, if running the dead letter job
        if (Config::$enableDeadLetters) {

            $deadLetterTopic = 'raw_' . Config::$tenantId  . '_' . Config::$pipelineName .'_deadletter';

//            $serialised = json_encode($message);

            $sp = new SkipprPack();
            $sp->encode($message, $offset);
            $payload = $sp->string() . "\n";

            $this->deadletterBuffer->append($payload);

            Statsd::increment("ingest.deadletters.current.{Config::$tenantId }.{Config::$pipelineName}", 1);

            Statsd::increment("ingest.deadletters.total.{Config::$tenantId }.{Config::$pipelineName}", 1);

            $this->deadLetters++;

        } else {

            $this->log->critical('Schema not valid for events in dead letter queue');

//            $this->inputBuffer->unlockAll('deadletter');
//            $this->inputBuffer->flush("deadletter");
//            $this->inputBuffer->finalise("deadletter", true);

            $this->shutdown();

//            exit(0);
        }

    }

    public function emit(string $payload, $offset = null) : void
    {

        if ($payload != '') {

            if ($this->mode == 'sync') {

                if (!Config::$enableDeadLetters) {

                    $sp = new SkipprPack($payload);
                    $payload = $sp->decodeRecord();
//                $offset = $sp->decodeOffset();
                }

                $this->parseLine($payload, $offset);


            } elseif ($this->mode == 'async') {

                $offset = (string) $offset;
                $sp = new SkipprPack();
                $sp->encode($payload, $offset);
                $payload = $sp->string();


                // Limit input buffer size to prevent flooding disk
                // and allow ingest threads to catch up
//            while($this->inputBuffer->bufferGetSize('input') > $this->inputBuffMaxBytes) {
//            while($this->inputBuffer->bufferGetSize('input') > count($this->threadPool)) {
//                sleep(1);
//            }

                $this->inputBuffer->append("input", $payload);

            }

        }



        if (!Config::$analysing) {

            $this->inputPlugin->commit($offset);

            if (!isset($this->j)) {
                $this->j = 0;
            } else {
                $this->j++;
            }

            if ($this->j > self::$flushMaxMsg) {

                $this->j = 0;

                $this->pipelineModel->save();
                
            }
        }


    }

    public function process() : void
    {

        $offset = '';

        $bufferName = Config::$enableDeadLetters ? 'input' : 'deadletter';

//        while ($line = $this->inputBuffer->lockedRead($bufferName)) {

        $i = 0;

        if (empty(Config::$serder)) {
            Config::$serder = $this->detectSerialisation();
        }

        while ($line = $this->inputBuffer->stream()) {

            $sp = new SkipprPack($line);
            $payload = $sp->decodeRecord();
            $offset = $sp->decodeOffset();

            $this->parseLine($payload, $offset);

            $this->inputBuffer->commit();

//            $sourceMessages['skpr_event_ts'] = 0;
//            $sourceMessages['skpr_partition'] = '';
//            $this->serialiseOutput($sourceMessages, $offset);

            $i++;
        }

        if (!Config::$analysing) {

//            Statsd::increment("ingest.msgs.current.{Config::$tenantId }.{Config::$pipelineName}", $i);

//            $this->outputBuffer->flush('out');
            $this->outputBuffer->flushAll();
            $this->deadletterBuffer->flushAll();

//            $this->updateDeadLetterQueueSize();
        }

    }

    public function parseLine(string $payload, string $offset) {


//                    $payload = $record;

        $this->currentBytes += strlen($payload);
//
        if (empty(Config::$serder)) {
            
            $this->inputBuffer->append($payload, true);
            $this->inputBuffer->finalise(true);

            Config::$serder = $this->detectSerialisation();

        } else {

            $serder = SerdersFactory::factory(Config::$serder);
            $sourceMessages = $serder->deserialize($payload);

            $this->parse($sourceMessages, $offset);

//        if ($this->mode == 'sync' && !Config::$analysing) {
//            Statsd::increment("ingest.msgs.current.{Config::$tenantId }.{Config::$pipelineName}", 1);
//        }
        }


    }

    public function parse(array $payload, string $offset) {

        $unwrappedMessages = $this->unwrap($payload); //

        foreach ($unwrappedMessages as $unwrappedMessage) {  // outer array

            if (Config::$analysing) {

                if (is_array($unwrappedMessage)) {

                    $this->getIdFields($unwrappedMessage);
                    
                    $this->analysePayload($unwrappedMessage, Config::$discoveredFieldOccurrence);
                }

                if ($this->i > $this->minSample
                    || Carbon::now()->timestamp - $this->startTimestamp > $this->maxTime) {

                    $this->log->info('Finished analysing data');

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

                    $message = $this->ingestPayload($unwrappedMessage, $offset, Config::$discoveredFieldOccurrence);

                    if ($message) {
                        $this->serialiseOutput($message, $offset);
                    } else {
                        $this->deadLetterMessage($payload, $offset);
                    }
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


            if ($time_value = array_get($message, $field_dot, false) ) {
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

                        $unwrappedMessages = array_get($sourceMessage, Config::$eventPath);

                    } catch (\Exception $e) {
                        $this->log->error("Could not find field path " . Config::$eventPath . " in message.");

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


        if (!Config::$enableDeadLetters) {
            $this->inputPlugin->commit($this->pipelineModel->dl_offset);

        } else {
            $this->inputPlugin->commit($this->pipelineModel->offset);

        }

        $this->inputBuffer->flushAll();
        $this->outputBuffer->flushAll();
        $this->deadletterBuffer->flushAll();

        $this->pipelineModel->save();
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

        $this->log->info("Gracefully shutting down and flushing buffers");

        if ($this->mode == 'async') {

            if ($this->inputThread !== null) {
                $this->inputThread->cancel();
            }
        }

        if ($this->inputPlugin !== null) {

            $this->inputPlugin->shutdown();
        }

        if ($this->mode == 'async') {

            $this->inputBuffer->unlockAll();
//            $this->inputBuffer->flush("input");
            $this->inputBuffer->flushAll(true);
            $this->inputBuffer->finalise(true);

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
            $this->outputBuffer->unlockAll();
//            $this->outputBuffer->flush("out");
            $this->outputBuffer->flushAll(true);
            $this->outputBuffer->finalise(true);

            $this->deadletterBuffer->unlockAll();
            $this->deadletterBuffer->flushAll(true);
            $this->deadletterBuffer->finalise(true);
        }
        
        if ($this->mode == 'sync') {

            if (!empty($this->outputPlugin)) {
                
                $this->log->info("Syncing remaining output buffers to destination.");

                if (!Config::$analysing) {

                    $this->outputPlugin->sync(Config::$serder, Config::$schema);

                    $this->deadletterPlugin->sync();
                }
                $this->outputPlugin->shutdown();
                $this->deadletterPlugin->shutdown();
            }
        }

        $this->pipelineModel->save(); // commit offsets

        $this->log->info("Ingested " . $this->entries . " messages");
        $this->log->info("Queued " . $this->deadLetters . " dead letters");
        
//        $this->updateDeadLetterQueueSize();

        if (!empty(Config::$discoveredFieldOccurrence)) {

            $this->finaliseFieldMapping();

            $this->writeMapping();

        } else {
            $this->log->info("No fields found when analysing schema, did you send some data?");
        }

//        $this->log->debug("Mem used: " . BytesToHuman::toHuman(memory_get_usage(true), true, 'MB'));
//        $this->log->debug("Mem limit: " . BytesToHuman::toHuman($this->flushBytes, true, 'MB'));

        // Update metadata such as field mapping
//        $configYml = $this->setConfig();
//        $configYml['update_status'] = true;
//        $configYml['status'] = false;
//        event(new WorkerConfigRequested($configYml));

        // Update Job Status
        if (class_exists(PodsStatus::class)) {
            PodsStatus::dispatch();
        }

        $this->log->info("Graceful shutdown complete, bye");

//        $this->delete();
        exit(0);
//        return;
    }

    public function writeMapping()
    {

        $configYml = $this->setConfig();

        $configYml['config_updated'] = microtime(true);

        $validDateFieldCandidates = [];
        foreach ($this->dateFieldCandidates as $field => $candidateField) {
            if (!empty($candidateField['field'])) {
                $validDateFieldCandidates[$candidateField['field']] = $candidateField;
            }
        }
        $configYml['field_yml']['date_field_candidates'] = $validDateFieldCandidates;

        $configYml['field_yml']['enitity_field_candidates'] = $this->idFields;

        event(new WorkerConfigRequested($configYml));

        $this->log->info("Updated analysed field schema");

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

        foreach ($this->idFields as $fieldName => $ids) {

            // 95% of this fields ID's are unique, it's probably a message ID field
            if (count($this->idFields[$fieldName]) / $this->minSample * 100 >= 70) {
                unset($this->idFields[$fieldName]);

            } else {
                $enitityFieldCandidates[$fieldName] = [];
            }
        }

        $this->idFields = $enitityFieldCandidates;

        $numCandidates = count($this->idFields);
        $this->log->info("Found $numCandidates ID fields");
    }

    public static function determineFieldTypes(&$array, $parent_type = null) {

        foreach ($array as $fieldName => $field) {

            // Useful for field evolution logic for maps, which only support one sub-field type
            if ($parent_type !== null) {
                $array[$fieldName]['parent_type'] = $parent_type;
            }

            if (empty($array[$fieldName]['determined_type'])) {

                $highestType = '';
                $highestCount = 0;

                if (!empty($field['type'])) {

                    $dateTypes = ['date', 'timestamp', 'timestamp_milli'];

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

                        asort($typeCount);

                        $array[$fieldName]['determined_type_values'] = array_key_first($typeCount);

                        $array[$fieldName]['fields'] = [];

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

                    $this->idFields[$fieldName][$value] = $value;

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

