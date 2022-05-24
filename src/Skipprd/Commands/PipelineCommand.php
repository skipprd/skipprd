<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 20/11/2017
 * Time: 17:36
 */

namespace Skipprd\Commands;

use Skipprd\Plugins\OffsetDrivers\OffsetDriverFactory;
use Skipprd\Arr;
use Skipprd\Buffers\BufferAdaptorsFactory;
use Skipprd\Plugins\PluginFactory;
use Carbon\Carbon;
use Skipprd\Helpers;
use Skipprd\SkipprPack;
use Skipprd\Str;
use Skipprd\Serders\SerdersFactory;
use League\StatsD\Client as Statsd;
use Skipprd\Plugins\DataSources\DataSourcePluginInterface;
use Skipprd\Plugins\DataOutputs\DataOutputPluginInterface;
use Segment;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;
use Skipprd\Traits\LicenseChecker;
use Skipprd\Traits\RecordFilter;
use Skipprd\Traits\SkipprLogger;
use Skipprd\Traits\SkipprStream;

class PipelineCommand
{

//    use Dispatchable, InteractsWithQueue, Queueable;
//SerializesModels;

    use AnalyseSchema;
    use Ingest;
    use BufferAdaptorsFactory;
    use LicenseChecker;
    use SkipprLogger;
    use RecordFilter;
    use SkipprStream;

    protected $statsd = null;

    public $inputThread = null;

    public $threadPool = [];

    public $outputThread = null;


    public $offsetChannel = null;


    public $host = '';
    public $port = 5544;

    /**
     * @var resource
     */
    public $sock;

    public $defaultMsgs = [];

    /**
     * @var \Skipprd\Plugins\OffsetDrivers\OffsetDriverInterface
     */
    public $offsetClient;

    /**
     * @var int - seconds analysinc jobs has been running for
     */
    public $startTimestamp = 0;

    public $totalEntries = 0;

    public $hashes = [];

    public $duplicateCount = 0;

    public $deadLetters = 0;

    public $lastStatusUpdate = 0;

    public $statusUpdateIntervalSeconds = 60;

    /**
     * @var \Skipprd\Plugins\DataSources\DataSourcePluginBase
     */
    public $inputPlugin = null;

    /**
     * @var \Skipprd\Plugins\DataOutputs\DataOutputPluginBase
     */
    public $outputPlugin = null;

    /**
     * @var \Skipprd\Plugins\DataOutputs\DataOutputPluginBase
     */
    public $deadletterPlugin = null;

    /**
     * @var \Skipprd\Buffers\BufferInterface|null
     */
    public $inputBuffer = null;

    /**
     * @var \Skipprd\Buffers\BufferInterface|null
     */
    public $outputBuffer = null;
    /**
     * @var \Skipprd\Buffers\BufferInterface|null
     */
    public $deadletterBuffer = null;

    /**
     * PipelineJob constructor.
     * @param $pluginModel DataSourcePluginInterface|DataOutputPluginInterface
     * @param array $config
     */
    public function __construct()
    {
    }

    protected function setPlugin()
    {

//        $bufferDriver = 'file';
//
//        if (!empty(Config::$timeFields) || !empty(Config::$entityNames)) {
//            $bufferDriver = 'chunked';
//        }

        // always used chunked buffer as we partition by data source partition
        // (table, topic, etc)
        $bufferDriver = 'file';

        //@todo - set $this->buffer->flushBytes in the output plugin
        $this->inputBuffer = BufferAdaptorsFactory::getAdaptor(
            'input',
            $bufferDriver
        );
        $this->outputBuffer = BufferAdaptorsFactory::getAdaptor(
            'output',
            $bufferDriver
        );
        $this->deadletterBuffer = BufferAdaptorsFactory::getAdaptor(
            'deadletter',
            $bufferDriver
        );

        /**
         * Dead Letter Plugin
         */
        $deadLetterPluginName = Config::getenv('DEAD_LETTER_PLUGIN_NAME');

        if (!empty($deadLetterPluginName)) {
            $config = [];
            $envs = getenv();

            foreach ($envs as $key => $value) {
                if (strpos($key, 'DEAD_LETTER') > -1) {
                    $config[strtolower(substr(
                        $key,
                        strlen('DEAD_LETTER_')
                    ))] = $value;
                }
            }

            $deadLetterPluginName = Str::studly(ucwords(strtolower($deadLetterPluginName)));

            $deadLetterPluginClass = "Skipprd\\Plugins\\DataOutputs" . "\\$deadLetterPluginName\\DataOutput" . "$deadLetterPluginName" . "Plugin";

            $this->deadletterPlugin = new $deadLetterPluginClass(
                $config,
                $this->deadletterBuffer
            );

            $this->deadletterPlugin->buffer->driver->setSerde('json');
        }
//        else {
//                $deadLetterPluginName = 'File';
//
//                $config['path'] = '/dead-letters';
//        }


        if (Config::getenv('JOB_NAME') == 'deadletters') {
            Config::$enableDeadLetters = false;
        }

        /**
         * Data Source Plugin
         */
        $pluginName = Config::getenv('DATA_SOURCE_PLUGIN_NAME');

        if (!empty($pluginName)) {
            $this->inputPlugin = PluginFactory::factory(
                'data_source',
                $pluginName,
                $this->outputBuffer
            );
        }
//        else {
//            $outputPluginClass = "Skipprd\\Plugins\\DataOutputs\\" . 'File' . "\\DataSource" . 'File' . "Plugin";
//
//            $this->inputPlugin = new $outputPluginClass($config, $this->outputBuffer);
//        }

        /**
         * Data Output Plugin
         */
        $pluginName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');

        if (!empty($pluginName)) {
            $this->outputPlugin = PluginFactory::factory(
                'data_output',
                $pluginName,
                $this->outputBuffer
            );
        }
//        else {
//            $outputPluginClass = "Skipprd\\Plugins\\DataOutputs\\" . 'File' . "\\DataOutput" . 'File' . "Plugin";
//
//            $config = [];
//            $config['path'] = '/';
//
//            $this->outputPlugin = new $outputPluginClass(
//                $config,
//                $this->outputBuffer
//            );
//        }
    }


    /**
     * Execute the console command.
     */
    public function handle()
    {

        try {
            $this->init();

//        set_exception_handler([$this, 'exceptionHandler']);

            // handle sigs
            // PHP 7.1 and later can handle asynchronous signals natively
            pcntl_async_signals(true);

            pcntl_signal(
                SIGINT,
                [$this, 'shutdownSig']
            ); // Call $this->shutdown() on SIGINT
            pcntl_signal(
                SIGTERM,
                [$this, 'shutdownSig']
            ); // Call $this->shutdown() on SIGTERM


//        if (Config::$analysing) {
//            Config::$mode = 'sync';
//        }


            if (!Config::$enableDeadLetters) {
                SkipprLogger::info('Reprocessing dead letters');
            }

            if (!empty($this->inputPlugin)) {
                $ran = false;


                if (Config::$runMode == Config::RUN_MODE_SYNC) {

                    $this->inputPlugin->buffer->flushAll();

                    if (!Config::$analysing) {
                        $this->streamConnect();
                    }

                    $ran = false;
                    while (!Config::$analysing && (!$ran || !empty(Config::$pollIntervalSeconds))) {
                        $ran = true;

                        $this->inputPlugin->sync();

//                        sleep(1); // Ensure outputs TCP buffer is flush

                        if (!Config::$analysing) {
                            
                            $skipprPack = new SkipprPack();
                            $skipprPack->encode('sync_complete', '');

                            $this->streamSend($skipprPack, null);

                            sleep(Config::$pollIntervalSeconds ?? 1);

                            $this->scheduledStatusUpdate();
                        }

                    }

                    fclose($this->sock);

                    $this->shutdown();

                }


                if (Config::$runMode == Config::RUN_MODE_VALIDATE_SCHEMA) {
                    $command = new \Skipprd\Commands\ValidateSchemaFile();
                    $command->handle();
                    $this->shutdown();
                }

                if (Config::$runMode == Config::RUN_MODE_VALIDATE_CONFIG) {
                    SkipprLogger::info("Validating config");

                    $this->inputPlugin->doValidateConfig();
                    $this->shutdown();
                }

                if (Config::$runMode == Config::RUN_MODE_VALIDATE_CONNECTION) {
                    SkipprLogger::info("Validating connection");
                    $this->inputPlugin->doValidateConnection();
                    $this->shutdown();
                }

                if (Config::$runMode == Config::RUN_MODE_SAVE) {
                    // @todo - need this?
                    $this->inputPlugin->doSave();
                    $this->shutdown();
                }

                if (Config::$runMode == Config::RUN_MODE_DELETE_PLUGIN) {
                    $this->inputPlugin->deletePlugin();
                    $this->shutdown();
                }

                if (Config::$runMode == Config::RUN_MODE_RESET_SOURCE_OFFSETS) {
                    SkipprLogger::info("Resetting offsets");
                    $this->resetSourceOffsets();
                    $this->shutdown();
                }

                
            } else {
                if (!empty($this->deadletterPlugin)) {
                    $ran = false;

                    while (!$ran || !empty(Config::$pollIntervalSeconds)) {
                        $ran = true;

                        if (Config::$runMode == Config::RUN_MODE_SYNC) {
                            $this->deadletterPlugin->sync();

                            $this->deadletterPlugin->buffer->flushAll();

                            sleep(Config::$pollIntervalSeconds ?? 1);
                        }

                        if (Config::$runMode == Config::RUN_MODE_VALIDATE_CONFIG) {
                            $this->deadletterPlugin->doValidateConfig();
                        }

                        if (Config::$runMode == Config::RUN_MODE_VALIDATE_CONNECTION) {
                            $this->deadletterPlugin->doValidateConnection();
                        }

                        if (Config::$runMode == Config::RUN_MODE_SAVE) {
                            // @todo - need this?
                            $this->deadletterPlugin->doSave();
                        }

                        if (Config::$runMode == Config::RUN_MODE_DELETE_PLUGIN) {
                            $this->deadletterPlugin->deletePlugin();
                        }
                    }
                }

                // keep output alive
                // we'll probably make output sycronous
                if (!empty($this->outputPlugin)) {
                    $ran = false;

                    if (Config::$runMode == Config::RUN_MODE_SYNC) {

                        $this->host = Config::getenv('HOST', '0.0.0.0');

                        SkipprLogger::info("Listening on {$this->host}:{$this->port}");

                        $this->sock = $this->streamListen();

                        $this->streamRead([$this, 'serialiseOutput'], [$this->outputPlugin, 'sync']);
                        
                    }


                    if (Config::$runMode == Config::RUN_MODE_VALIDATE_CONFIG) {
                        $this->outputPlugin->doValidateConfig();
                    }

                    if (Config::$runMode == Config::RUN_MODE_VALIDATE_CONNECTION) {
                        $this->outputPlugin->doValidateConnection();
                    }

                    if (Config::$runMode == Config::RUN_MODE_SAVE) {
                        // @todo - need this?
                        $this->outputPlugin->doSave();
                    }

                    if (Config::$runMode == Config::RUN_MODE_DELETE_PLUGIN) {
                        $this->outputPlugin->deletePlugin();
                    }

                    if (Config::$runMode == Config::RUN_MODE_CREATE_UPDATE_DEST_SCHEMA) {
                        foreach (Config::$schema as $namespace => $avroSchema) {
                            SkipprLogger::info("Evolving $namespace destination schema");

                            $this->outputPlugin->createOrUpdateSchema(
                                $namespace,
                                $avroSchema
                            );
                        }
                    }

                    if (Config::$runMode == Config::RUN_MODE_DELETE_DEST_SCHEMA) {
                        $this->outputPlugin->deleteSchema();
                    }
                }
            }



            if (Config::$analysing) { // in case we didn't see enough messages
                SkipprLogger::info('Finished analysing data');
                $this->shutdown();
            }

            if (Config::$syncMode == 'async') {
                // keep alive to control child threads
                while (true) {
                    sleep(10);
                }
            }
        } catch (\Exception $e) {
            SkipprLogger::error($e->getTraceAsString());
            SkipprLogger::error($e->getMessage());
            SkipprLogger::critical('Sorry, something failed badly. Please report this error to us with the logs above.');
            
            $this->shutdown(1);
        }
    }

    protected function scheduledStatusUpdate()
    {

        if ($this->lastStatusUpdate < time() - $this->statusUpdateIntervalSeconds) {
            Config::setStatus();
            $this->lastStatusUpdate = time();
        }
    }

    public function exceptionHandler(\Exception $e)
    {

        SkipprLogger::error($e->getMessage());

        SkipprLogger::warning("Uncaught exception, shutting down all threads.");

        $this->shutdown();
    }

    /**
     * Execute the console command.
     */
    public function init()
    {

        Segment::init(Config::$segmentKey);

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

//        $config = $this->pipelineModel->buildJobConfig();

//        $this->setConfig($config);
//        $this->config = $config;

//        $config = [];
//        $this->getConfig($config);

        $this->setPlugin();

        $serde = SerdersFactory::factory(Config::$outputFormat);

        foreach (Config::$schema as $namespace => $schema) {
            $this->defaultMsgs[$namespace] = $serde->defaultMessage($schema);
        }

        $this->startTimestamp = Carbon::now()->timestamp;

        $this->getLicense();

        if (!$this->licenseIsValid) {
            SkipprLogger::info('Please validate license to continue using Skippr');

            $this->shutdown();
        }

        Segment::identify([
            "userId" => hash('sha256', Config::$tenantId),
            "licenseKey" => $this->licenseKey,
            "traits" => [
                "pipeline_name" => hash('sha256', Config::$pipelineName),
            ]
        ]);


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

        while (($line = $this->inputPlugin->buffer->stream()) !== false) {
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

    public function outputEmit(array $payload): void
    {


        $namespace = $payload['skpr_namespace'];
        $partition = $payload['skpr_partition'];

        $offset = (string) $this->inputPlugin->offsets->getCurrentOffsets(
            $namespace,
            $partition
        );

        if (!empty($payload) && !empty($offset)) {

            $serialised = msgpack_pack($payload);
            $sp = new SkipprPack();
            $sp->encode($serialised, $offset);

            $skipprPack = $sp->string();

            $this->streamSend($skipprPack, null);


        } else {
            SkipprLogger::error("Failed to output, no offset or payload");
        }
    }

    public function serialiseOutput(SkipprPack $sp): void
    {

        $record = '';

        try {

            $record = $sp->decodeRecord();
            $offset = $sp->decodeOffset();
            
        } catch (\Exception $e) {

            SkipprLogger::error($e->getMessage());

        }

        if (!empty($record)) { // @todo - why do we get emtpy messages over the network sometimes?

            // @todo - often get 'Warning: [msgpack] (php_msgpack_unserialize) Extra bytes' without @
            $payload = @msgpack_unpack($record);
            
            try {
                $eventTime = $payload['skpr_event_ts'];
                $namespace = $payload['skpr_namespace'];
                $partition = $payload['skpr_partition'];

                $offset = $this->outputPlugin->offsets->setOffsets($offset,
                    $namespace, $partition);

                $record = $payload;
//            unset($record['skpr_event_ts']);
//            unset($record['skpr_namespace']);
//            unset($record['skpr_partition']);

//            $serialised = json_encode($record) . "\n";
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


                if (Config::$syncMode == 'sync') {
//                $this->outputPlugin->buffer->append($serialised, false, $eventTime, $partition);
                    $result = $this->outputPlugin->buffer->append(
                        $record,
                        false,
                        $eventTime,
                        $namespace,
                        $partition
                    );

//                if (!empty($this->outputPlugin)) {
//                    $this->flushBuffersRoutine();
//                }

//                    $this->inputPlugin->setOffsets($this->outputPlugin->offset);
                } elseif (Config::$syncMode == 'async') {
                    $result = $this->outputPlugin->buffer->append(
                        $record,
                        false,
                        $eventTime,
                        $namespace,
                        $partition
                    );
                }

                if ($result == 2) { // buffer was flushed
                    $this->offsetCommitRoutine($namespace, $partition);
                }

                $tenantId = Config::$tenantId;
                $pipelineName = Config::$pipelineName;

                if (!empty($this->statsd)) {
                    $this->statsd->increment(
                        "$tenantId.$pipelineName.ingest.records.current",
                        1
                    );
                }

                // Empty only after writing, will ensure still available for graceful shutdown
                $this->hashes = [];
                $this->duplicateCount = 0;


//            $flushedCount++;
            } catch (\AvroException $e) {
//            SkipprLogger::error($e->getMessage());

                try {
                    $this->deadLetterMessage($payload);
                } catch (\Exception $e) {
                    SkipprLogger::emergency('Failed to write to dead letter queue');
                    SkipprLogger::error($e->getMessage());
                }
            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
            }

        } else {
            SkipprLogger::error("SkipprPack unpacked empty record");

        }
    }


    public function deadLetterMessage(array $message)
    {

        // Don't dead letter message, if running the dead letter job
        // it will be skipped and so just remain in the queue
        if (Config::$enableDeadLetters) {
            $namespace = $message['skpr_namespace'];
            $partition = $message['skpr_partition'];

            $deadLetterTopic = 'raw_' . Config::$tenantId . '_' . Config::$pipelineName . '_deadletter';

//            $serialised = json_encode($message);

//            $sp = new SkipprPack();
//            $sp->encode($serialised, $offset);
//            $payload = $sp->string() . "\n";

            if (!empty($this->deadletterPlugin)) {
//            $this->deadletterPlugin->buffer->append($payload);
                $this->deadletterPlugin->buffer->append(
                    $message,
                    false,
                    0,
                    $namespace,
                    $partition
                );
            }
            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

            if (!empty($this->statsd)) {
                $this->statsd->increment(
                    "$tenantId.$pipelineName.ingest.deadletters.current",
                    1
                );
            }

            $this->deadLetters++;
        } else {
            SkipprLogger::critical('Schema not valid for events in dead letter queue');

//            $this->inputPlugin->buffer->unlockAll('deadletter');
//            $this->inputPlugin->buffer->flush("deadletter");
//            $this->inputPlugin->buffer->finalise("deadletter", true);

            $this->shutdown();

//            exit(0);
        }
    }

    public function emit(
        string $payload,
        string $offset,
        string $namespace,
        string $partition = '0'
    ): void {


        if (!empty($payload) && !empty($offset)) {
            if ($this->inputPlugin->offsets->validateOffset(
                $offset,
                $namespace,
                $partition
            )) {
                $this->inputPlugin->offsets->setOffsets(
                    $offset,
                    $namespace,
                    $partition
                );
                
                if (Config::$syncMode == 'sync') {
                    if (!Config::$enableDeadLetters) {
                        // @todo - deprecate SkipprPack for Apache Arrow
                        $sp = new SkipprPack($payload);
                        $payload = $sp->decodeRecord();
//                $offset = $sp->decodeOffset();
                    }

                    $this->emitString($payload, $namespace, $partition);
                } elseif (Config::$syncMode == 'async') {
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

                    $this->inputPlugin->buffer->append(
                        $payload,
                        false,
                        0,
                        $namespace,
                        $partition
                    );
                }
            }
        }
    }

    public function offsetCommitRoutine(
        string $namespace,
        string $partition
    ): void {
        if (!Config::$analysing) { // should never be here on analyse schema, but just in case of code error
            $offset = $this->outputPlugin->offsets->getCurrentOffsets(
                $namespace,
                $partition
            );

            SkipprLogger::info("Committing offset for Namespace: $namespace Partition: $partition Offset: $offset");

            $this->offsetClient->sync($namespace, $partition, $offset);
        }
    }

    public function resetSourceOffsets(): void
    {
        $offsets = $this->inputPlugin->offsets->getAll();

        foreach ($offsets as $namespace => $partitionArr) {
            foreach ($partitionArr as $partition => $offset) {
                SkipprLogger::info("Resetting offset for Namespace: $namespace Partition: $partition from current offset $offset to ''");
                $this->inputPlugin->offsets->setOffsets('', $namespace, $partition);
                $this->offsetClient->sync($namespace, $partition, '');
            }
        }
    }
    public function offsetCommitAll(): void
    {
        $offsets = $this->outputPlugin->offsets->getAll();

        foreach ($offsets as $namespace => $partitionArr) {
            foreach ($partitionArr as $partition => $offset) {
                SkipprLogger::info("Committing offset for Namespace: $namespace Partition: $partition Offset: $offset");
                $this->offsetClient->sync($namespace, $partition, $offset);
            }
        }
    }

    public function readFile(
        string $filename,
        string $partition,
        callable $callback
    ) {

        if (preg_match("/\.gz(ip)?$|.zip/", $filename) == true) {
            // open gz file for reading
            $sfp = gzopen($filename, 'rb');

            // read and decode chunks into string stream
            while (!gzeof($sfp)) {
                $string = gzgets($sfp);

                call_user_func($callback, $string, $partition);
            }

            gzclose($sfp);
        } elseif (Config::$sourceFormat == 'parquet') {
            $parquet = new \Parquet();

            $parquet->create_reader($filename, 0);

            $jsonString = '';

            $parquet->json_file($jsonString, 0);

            // @todo - implemente Parquet row reader

            $parquet->close_reader();

            $data = json_decode($jsonString, true);

            call_user_func($callback, $data, $partition);
//            foreach ($data as $item) {
//
//                call_user_func($callback, $item);
//            }
        } else {
            $sfp = fopen($filename, 'rb');

            // read and decode chunks into string stream
            while (!feof($sfp)) {
                $string = fgets($sfp);

                call_user_func($callback, $string, $partition);
            }

            // remove temp file
            fclose($sfp);
        }
    }

    public function emitFile(
        string $filename,
        string $namespace,
        string $partition = '0'
    ): void {

        $fields = [];
        $payloadString = '';

        $serde = SerdersFactory::factory(Config::$sourceFormat);

        $this->readFile(
            $filename,
            $partition,
            function (
                $string,
                $partition
            ) use (
                $serde,
                &$fields,
                &
                $payloadString,
                $namespace
            ) {

                if (in_array(Config::$sourceFormat, Config::$batchFormats)) {
                    $payloadString .= $string;
                } else {
                    $msgs = $serde->deserialize($string);

                    foreach ($msgs as $msg) {
                        //                    array_push($fields, $msg);
                        $this->emitArray($msg, $namespace, $partition);
                    }
                }
            }
        );

        if (in_array(Config::$sourceFormat, Config::$batchFormats)) {
            $msgs = $serde->deserialize($payloadString);

            foreach ($msgs as $msg) {
//                array_push($fields, $msg);
                $this->emitArray($msg, $namespace, $partition);
            }
        }
    }

    public function emitString(
        string $payload,
        string $namespace,
        string $partition = '0'
    ): void {


        if ($payload != '') {
            if (Config::$syncMode == 'sync') {
                if (!Config::$enableDeadLetters) {
                    // @todo - deprecate SkipprPack for Apache Arrow
                    $sp = new SkipprPack($payload);
                    $payload = $sp->decodeRecord();
//                $offset = $sp->decodeOffset();
                }

                $serder = SerdersFactory::factory(Config::$sourceFormat);
                $sourceMessages = $serder->deserialize($payload);

                $this->emitArray($sourceMessages, $namespace, $partition);
            }
        }
    }

    /**
     * All emit functions end up here after deserialising payload
     * @param array $payload - the payload to ingest
     * @param string $namespace - the schema namespace (table, avro namespace, event type, etc)
     * @param string $partition - data sources partition (shard, kafka topic, index, FS dir, etc)
     */
    public function emitArray(
        array $payload,
        string $namespace,
        string $partition = '0'
    ): void {

        $this->scheduledStatusUpdate();

        $unwrappedMessages = $this->unwrapEventPath($payload);

        if (empty(Config::$discoveredFieldOccurrence[$namespace])) {
            Config::$discoveredFieldOccurrence[$namespace] = [
                'enabled' => true,
                'fields' => [],
            ];
        }

        foreach ($unwrappedMessages as $unwrappedMessage) {  // outer array
            if ((Config::$analysing || empty(Config::$discoveredFieldOccurrence[$namespace]))
                && $this->inputPlugin->ingestNamespace($namespace) === true) {
                if (is_array($unwrappedMessage)) {
                    $this->getIdFields($unwrappedMessage);
                    $this->parseNamespaceField($unwrappedMessage, $namespace);
                    $unwrappedMessage['skpr_partition'] = $partition;

                    $this->analysePayload(
                        $unwrappedMessage,
                        Config::$discoveredFieldOccurrence[$namespace]['fields']
                    );
                }

                if ($this->i > Config::$minDiscoveryRecords
                    || (Carbon::now()->timestamp - $this->startTimestamp) > Config::$maxDiscoverySeconds) {
//
                    SkipprLogger::info("Finished discovering schema for $namespace record type");

                    $this->i = 0;

                    $this->inputPlugin->continue[$partition] = false;

//                    unlink("buffer.ready"); // clean up ready buffer - as we force exit here

                    // @todo - wont analyse all namespaces (tables, topics, paths, etc)
                    // if we exit here.
                    // The trouble with ->continue['part'] above is that it only exits if another
                    // record is found in the source. Else the source hangs till new data arrives.
                    // We need a way to force the source to the next namespace
                    $this->shutdown();
                }
            }


            if (!Config::$analysing && !empty(Config::$discoveredFieldOccurrence[$namespace])) {
                if (is_array($unwrappedMessage)) {
                    $this->parseTimeField($unwrappedMessage);
                    $this->parseNamespaceField($unwrappedMessage, $namespace);
                    $unwrappedMessage['skpr_partition'] = $partition;

                    try {
                        $message = false;
                        
                        if (!empty($unwrappedMessage)
                            && RecordFilter::filter($unwrappedMessage)) {
                            if (Config::$mutableMode) {
                                $message = $this->ingestPayload(
                                    $unwrappedMessage,
                                    Config::$discoveredFieldOccurrence[$namespace]['fields'],
                                    $namespace
                                );
                            } else {
                                $this->totalEntries++;
                                $message = $unwrappedMessage;
                            }
                        }
                    } catch (\Exception $e) {
                        SkipprLogger::error($e->getMessage());
                        $this->deadLetters++;
                        $message = false;
                    }

                    if ($message) {
//                        $this->serialiseOutput($message);
                        $this->outputEmit($message);
                    } else {
                        $this->deadLetterMessage($unwrappedMessage);
                    }
                } else {
                    SkipprLogger::error("found non-array message");
                    SkipprLogger::debug($unwrappedMessage);
                }
            }
        }
    }

    public function parseNamespaceField(array &$message, string $namespace)
    {

        // default to data source partition (table, topic, queue, file dir, etc)
//        $shardFieldEntityValue =  Helpers::cleanFieldName($namespace);
        $shardFieldEntityValue = $namespace;

        // optional: partition by composite key
        if (!empty(Config::$entityNames)) {
            foreach (Config::$entityNames as $entityField) {
                $shardFieldName = $entityField;
                // @todo - support entity naming
                $entityName = $entityField;

                if (!empty($message[$entityField])) {
                    if ($entityValue = $message[$entityField]) {
                        $shardFieldEntityValue .= '-' . Helpers::cleanFieldName($entityName) . '=' . Helpers::cleanFieldName($entityValue);
                    }
                }
            }
        }


//        $shardFieldEntityValue = strtolower(trim($shardFieldEntityValue, '-'));
        $shardFieldEntityValue = trim($shardFieldEntityValue, '-');

        $message['skpr_namespace'] = $shardFieldEntityValue;
    }

    public function parseTimeField(&$message)
    {

        // default to beginning of epoch.
        $message['skpr_event_ts'] = 0;

        // Support nested time fields via array dot notation
        // For user confirmed event time fields, use the first one that matches
        foreach (Config::$timeFields as $field_dot) {
            if ($time_value = Arr::get($message, $field_dot, false)) {
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

    public function unwrapEventPath($sourceMessages)
    {

        if (!empty($sourceMessages) && is_array($sourceMessages)) {
            if (!empty(Config::$eventPath)) {
                foreach ($sourceMessages as $sourceMessage) {
                    try {
                        $unwrappedMessages = Arr::get(
                            $sourceMessage,
                            Config::$eventPath
                        );
                    } catch (\Exception $e) {
                        SkipprLogger::error("Could not find field path " . Config::$eventPath . " in message.");
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

//        $this->pipelineModel->save();
        // @todo - implement state storage

//        $this->inputPlugin->commit(Config::$offsets);

        if (!empty($this->inputPlugin)) {
            $type = Config::getenv('OFFSET_DRIVER', 'skippr_file');
            $this->offsetClient = OffsetDriverFactory::factory($type);
            $offsets = $this->offsetClient->get();

            if (!empty($offsets)) {
                foreach ($offsets as $namespace => $offsetsParts) {
                    foreach ($offsetsParts as $partition => $offset) {
                        $this->inputPlugin->offsets->setOffsets(
                            $offset,
                            $namespace,
                            $partition
                        );
                    }
                }
            }

            $this->inputPlugin->connect();
            $this->inputPlugin->buffer->flushAll();
        }

        if (!empty($this->deadletterPlugin)) {
            $this->deadletterPlugin->connect();
            $this->deadletterPlugin->buffer->flushAll();
        }

        if (!empty($this->outputPlugin)) {
            $type = Config::getenv('OFFSET_DRIVER', 'skippr_file');
            $this->offsetClient = OffsetDriverFactory::factory($type);
            $offsets = $this->offsetClient->get();

            if (!empty($offsets)) {
                foreach ($offsets as $namespace => $offsetsParts) {
                    foreach ($offsetsParts as $partition => $offset) {
                        $this->outputPlugin->offsets->setOffsets(
                            $offset,
                            $namespace,
                            $partition
                        );
                    }
                }
            }


            $this->outputPlugin->connect();
            $this->outputPlugin->buffer->flushAll();
        }
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
                } elseif ($i == 0) { // last
                    Config::$discoveredFieldOccurrence[$parts[$i]]['fields'] = $metadata;
                } else {
                    $newMetadata = [];
                    $newMetadata[$parts[$i]]['fields'] = $metadata;
                    $metadata = $newMetadata;
                }
            }
        }
    }

    public function shutdownSig(int $signo, $siginfo): void
    {

        $this->shutdown($signo);
    }

    public function shutdown(int $signo = 0)
    {

        SkipprLogger::info("Gracefully shutting down");

        if (Config::$syncMode == 'async') {
            if ($this->inputThread !== null) {
                $this->inputThread->cancel();
            }
        }

        if (!empty($this->inputPlugin)) {
            $this->inputPlugin->shutdown();
        }

        if (Config::$syncMode == 'async') {
            $this->inputPlugin->buffer->driver->unlockAll();
//            $this->inputPlugin->buffer->flush("input");
            $this->inputPlugin->buffer->flushAll(true);
            $this->inputPlugin->buffer->driver->finalise();

            if ($this->threadPool !== null) {
                foreach ($this->threadPool as $thread) {
                    $thread->kill();
                }
            }

            if ($this->outputThread != null) {
                $this->outputThread->kill();
            }
//        }

//        if (!Config::$analysing) {
            $this->outputPlugin->buffer->driver->unlockAll();
//            $this->outputPlugin->buffer->flush("out");
            $this->outputPlugin->buffer->flushAll(true);
            $this->outputPlugin->buffer->driver->finalise();

            if (!empty($this->deadletterPlugin)) {
                $this->deadletterPlugin->buffer->driver->unlockAll();
                $this->deadletterPlugin->buffer->flushAll(true);
                $this->deadletterPlugin->buffer->driver->finalise();
            }
        }

        if (Config::$syncMode == 'sync') {
            if (!Config::$analysing) {
                if (!empty($this->outputPlugin)) {
                    $outputPluginName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');
                    $this->outputPlugin->buffer->driver->unlockAll();
                    $this->outputPlugin->buffer->flushAll(true);
                    $this->outputPlugin->buffer->driver->finalise();
                    SkipprLogger::info("Flushing output buffers to $outputPluginName destination.");
                    $this->outputPlugin->sync();
                    $this->offsetCommitAll();
                    $this->outputPlugin->shutdown();
                }

                if (!empty($this->deadletterPlugin)) {
                    $deadLetterPluginName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');
                    $this->deadletterPlugin->buffer->driver->unlockAll();
                    $this->deadletterPlugin->buffer->flushAll(true);
                    $this->deadletterPlugin->buffer->driver->finalise();
                    SkipprLogger::info("Flushing dead letter buffers to $deadLetterPluginName destination.");
                    $this->deadletterPlugin->sync();
                    $this->deadletterPlugin->shutdown();
                }

                if (!empty($this->inputPlugin)) {

                    SkipprLogger::info("Ingested " . $this->totalEntries . " messages");
                    SkipprLogger::info("Dead Letters " . $this->deadLetters . " dead letters");
                }
            }
        }

            // Sync all offsets having synced to destination
//            if (!empty($this->inputPlugin)) {
//                $offsets = $this->inputPlugin->offsets->getOffsets();
//
//                $type = Config::getenv('OFFSET_DRIVER', 'skippr_file');
//                $offsetClient = OffsetDriverFactory::factory($type);
//                $offsetClient->syncAll($offsets);
//
//            }


//        $this->pipelineModel->save(); // commit offsets
        // @todo - implement state storage


//        $this->updateDeadLetterQueueSize();

        Config::$exitCode = $signo;

        if (!empty($this->inputPlugin)) {
            if (!empty(Config::$discoveredFieldOccurrence)) {
                $this->finaliseFieldMapping();

                $this->writeMapping();
            } else {
                SkipprLogger::info("No fields found when analysing schema, did you send some data?");
            }
        }

        if (!empty($this->inputPlugin)) {
            Config::setConfig();
        }

        Config::setStatus();

//        SkipprLogger::debug("Mem used: " . BytesToHuman::toHuman(memory_get_usage(true), true, 'MB'));
//        SkipprLogger::debug("Mem limit: " . BytesToHuman::toHuman($this->flushBytes, true, 'MB'));

        // Update metadata such as field mapping
//        $configYml = $this->setConfig();
//        $configYml['update_status'] = true;
//        $configYml['status'] = false;
//        event(new WorkerConfigRequested($configYml));

        $inputName = Config::getenv('DATA_SOURCE_PLUGIN_NAME');
        $outputName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');

        Segment::track(array(
            "userId" => hash('sha256', Config::$tenantId),
            "licenseKey" => $this->licenseKey,
            "tenant_id" => hash('sha256', Config::$tenantId),
            "pipeline_name" => hash('sha256', Config::$pipelineName),
            'event' => 'shutdown',
            "properties" => [
                "msgs_total" => $this->totalEntries,
                "deadletters_total" => $this->deadLetters,
                "input_plugin" => $inputName,
                "output_plugin" => $outputName,
                "input_format" => Config::getenv('DATA_SOURCE_FORMAT'),
                "output_format" => Config::getenv('DATA_OUTPUT_FORMAT'),
                "skippr_version" => Config::getenv('SKIPPR_BUILD_VERSION'),
            ]
        ));
        
        SkipprLogger::info("Graceful shutdown complete, bye");

//        $this->delete();
        exit($signo);
//        return;
    }

    public function writeMapping()
    {

        $this->finaliseFieldCandidates();

//        SkipprLogger::info("Updated analysed field schema");
    }

    public function finaliseFieldCandidates()
    {

        foreach (Config::$discoveredFieldOccurrence as $namespace => $metadata) {
            Config::$discoveredFieldOccurrence[$namespace]['date_field_candidates'] = [];
            Config::$discoveredFieldOccurrence[$namespace]['enitity_field_candidates'] = [];

            $validDateFieldCandidates = [];

            foreach ($metadata['fields'] as $field => $candidateField) {
                if (!empty($candidateField['date_candidate']['field'])) {
                    $validDateFieldCandidates[$field] = $candidateField['date_candidate']['field'];
                }
            }

            Config::$discoveredFieldOccurrence[$namespace]['date_field_candidates'] = $validDateFieldCandidates;

            Config::$discoveredFieldOccurrence[$namespace]['enitity_field_candidates'] = Config::$idFields;
        }
    }

    public function finaliseFieldMapping()
    {

        foreach (Config::$discoveredFieldOccurrence as $namespace => $metadata) {
            self::determineFieldTypes(Config::$discoveredFieldOccurrence[$namespace]['fields']);
        }

        $this->findMessageIdField();

        Config::$analysing = false;
    }

    public function findMessageIdField()
    {

        $enitityFieldCandidates = [];

        if (!empty(Config::$idFields)) {
            foreach (Config::$idFields as $fieldName => $ids) {
                // 95% of this fields ID's are unique, it's probably a message ID field
                if (count(Config::$idFields[$fieldName]) / Config::$minDiscoveryRecords * 100 >= 70) {
                    unset(Config::$idFields[$fieldName]);
                } else {
                    $enitityFieldCandidates[$fieldName] = [];
                }
            }

            $numCandidates = count(Config::$idFields);

            SkipprLogger::info("Found $numCandidates ID fields");
        }

        Config::$idFields = $enitityFieldCandidates;
    }

    public static function determineFieldTypes(&$metadata, $parent_type = null)
    {

        $demotedTypes = ['boolean', 'date', 'timestamp', 'timestamp_milli'];


        foreach ($metadata as $fieldName => $field) {
            // Useful for field evolution logic for maps, which only support one sub-field type
            if ($parent_type !== null) {
                $metadata[$fieldName]['parent_type'] = $parent_type;
            }

            if (empty($metadata[$fieldName]['determined_type'])) {
                $highestType = '';
                $highestCount = 0;

                if (!empty($field['type'])) {
                    // Don't allow NULL type if we discovered any other types
                    if (count($field['type']) > 1) {
                        unset($field['type']['NULL']);
                    }

                    // force to record type over map or array if ever present
                    if (key_exists('record', $field['type'])) {
                        $metadata[$fieldName]['determined_type'] = 'record';
                    } else {
                        foreach ($field['type'] as $dataType => $dataTypeCount) {
                            if ($highestCount < $dataTypeCount) {
                                // Prefer primitive types to logical types or types
                                // that cause frequent false positives (demoted types).
                                // - if there's multiple discovered types
                                // - and the most common type is a demoted type
                                // - select the next most common, non-date type
                                if (count($field['type']) == 1
                                    || (count($field['type']) > 1 && !in_array(
                                        $dataType,
                                        $demotedTypes
                                    ))) {
                                    $highestType = $dataType;
                                    $highestCount = $dataTypeCount;
                                }
                            }
                        }

                        $metadata[$fieldName]['determined_type'] = $highestType;
                    }
                }
            }

            if (!empty($metadata[$fieldName]['determined_type'])
                && in_array(
                    $metadata[$fieldName]['determined_type'],
                    ['map', 'array', 'record']
                )) {
                if (!empty($field['fields'])) {
                    if ($metadata[$fieldName]['determined_type'] == 'array'
                        || $metadata[$fieldName]['determined_type'] == 'map'
                    ) {
//                        && (!empty($metadata[$fieldName]['fields'][0]['determined_type'])
//                            && in_array($metadata[$fieldName]['fields'][0], ['map', 'array', 'record'])) ) {

                        $metadata[$fieldName]['determined_type_values'] = null;

                        // Ignore sub-fields for Avro array, the values are just enumerated, their not fields themselves.
                        // Else we'd create a field list with string keys for each array value
                        // e.g. [1,5,3,7,4,3,5]
                        // would incorrectly become ['a0' => 1, 'a1' => 5, ...]

                        $typeCount = [];

                        // @todo - not intended to build avro type array here
                        //         however, 'array' type is a special case... how to handle?

                        // Get avro arrays items primitive data type
                        foreach ($metadata[$fieldName]['fields'] as $sub_field) {
                            foreach ($sub_field['type'] as $dataType => $dataTypeCount) {
                                if (!in_array($dataType, $demotedTypes)) {
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

//                            if (!empty($metadata[$fieldName]['determined_type_values'])
//                                && in_array($metadata[$fieldName]['determined_type_values'], ['map', 'array', 'record']) ) {
//
//                                self::determineFieldTypes($metadata[$fieldName]['fields'],
//                                    $metadata[$fieldName]['determined_type']);
//
//                            } else {

                        $metadata[$fieldName]['determined_type_values'] = $valueTypes;

                        if ($metadata[$fieldName]['determined_type'] == 'array') {
                            $metadata[$fieldName]['fields'] = [];
                        }

//                            }
                    }
                    if ($metadata[$fieldName]['determined_type'] != 'array') {
                        self::determineFieldTypes(
                            $metadata[$fieldName]['fields'],
                            $metadata[$fieldName]['determined_type']
                        );
                    }
                }
//                else {
//
//                    unset($metadata[$fieldName]);
//                }
            }
        }
    }

    public function getIdFields($message)
    {

        $messageEntityNames = [];

        if (!empty($message)) {
            foreach ($message as $fieldName => $value) {
//                $fieldName = Helpers::cleanFieldName($fieldName);

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
    private function isDuplicate(array $message): bool
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
