<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 20/11/2017
 * Time: 17:36
 */

namespace Skipprd\Commands;

use Skipprd\Buffers\BufferDrivers\FileBufferDriver;
use Skipprd\InternalFields;
use Skipprd\MachineToHuman\BytesToHuman;
use Skipprd\MachineToHuman\NumberToHuman;
use Skipprd\MachineToHuman\TimeToHuman;
use Skipprd\Plugins\OffsetDrivers\OffsetDriverFactory;
use Skipprd\Arr;
use Skipprd\Buffers\BufferAdaptorsFactory;
use Skipprd\Plugins\PluginFactory;
use Carbon\Carbon;
use Skipprd\Helpers;
use Skipprd\Serders\SerderAvroRecord;
use Skipprd\Serders\SerderParquet;
use Skipprd\SkipprPack;
use Skipprd\Str;
use Skipprd\Serders\SerdersFactory;
use League\StatsD\Client as Statsd;
use Skipprd\Plugins\DataSources\DataSourcePluginInterface;
use Skipprd\Plugins\DataOutputs\DataOutputPluginInterface;
use Skipprd\Traits\AnalyseSchema;
use Skipprd\Traits\Config;
use Skipprd\Traits\Ingest;
use Skipprd\Traits\IngestFast;
use Skipprd\Traits\LicenseChecker;
use Skipprd\Traits\RecordFilter;
use Skipprd\Traits\SkipprLogger;
use Skipprd\Traits\SkipprStream;
use Skipprd\Traits\TaskResponse;

class PipelineCommand
{

//    use Dispatchable, InteractsWithQueue, Queueable;
//SerializesModels;

    use AnalyseSchema;
    use IngestFast;
    use Ingest;
    use BufferAdaptorsFactory;
    use LicenseChecker;
    use SkipprLogger;
    use RecordFilter;
    use SkipprStream;
    use TaskResponse;

    /**
     * @var Statsd
     */
    protected $statsd = null;

    /**
     * @var SkipprPack
     */
    protected $skipprPack;

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
     * @var \Skipprd\Serders\Interfaces\SerderBatchInterface|\Skipprd\Serders\Interfaces\SerderStreamInterface
     */
    public $inputSerder;

    /**
     * @var int - seconds analysinc jobs has been running for
     */
//    public $startTimestamp = 0;
//
//    public $totalEntries = ['total' => 0];
//    public $currentEntries = 0;
//    public $fastPath = 0;
//    public $slowPath = 0;

    public $hashes = [];

    public $duplicateCount = 0;

    public $rejectedSrcMesgCount = 0;

//    public $deadLetters = 0;

    public $lastStatusUpdate = 0;

    public $statusUpdateIntervalSeconds = 30;

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
                $this->inputBuffer
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

//            $converterClass = 'Skipprd\Converters\AvroParquetSchemaConverter';
//            $this->converter = new $converterClass();
//            foreach(Config::$schema as $namespace => $schema) {
//                $this->converter->convert(self::$avroSchemas[$namespace]);
//            }
//            $this->testSerde = new SerderParquet();
            set_exception_handler([$this, 'exceptionHandler']);

            set_error_handler([$this, 'exceptionsErrorHandler']);

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

                $this->inputSerder = SerdersFactory::factory(Config::$sourceFormat);

                if (Config::$runMode == Config::RUN_MODE_SYNC) {

                    $this->inputPlugin->buffer->driver->unlockAll();

                    if (!Config::$analysing) {
                        $conn = $this->streamConnect();
                    }

                    $ran = false;
                    while (!$ran || !empty(Config::$pollIntervalSeconds)) {
                        $ran = true;

                        $this->inputPlugin->sync();

                        $this->scheduledStatusUpdate();

                        sleep(Config::$pollIntervalSeconds ?? 1);
                    }

                    SkipprLogger::info("Sync complete");

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

                    $this->shutdown();
                }

                // keep output alive
                // we'll probably make output sycronous
                if (!empty($this->outputPlugin)) {
                    $ran = false;

                    if (Config::$analysing) {
                        SkipprLogger::info("In analysing mode, nothing for output to do. Did you mean to run an input?");
                        $this->shutdown();
                    }

                    if (Config::$runMode == Config::RUN_MODE_SYNC) {

                        $this->host = Config::getenv('HOST', '0.0.0.0');
                        SkipprLogger::info("Listening on {$this->host}:{$this->port}");

                        $this->outputPlugin->buffer->driver->unlockAll();

                        $this->sock = $this->streamListen();

                        while (true) {

                            // @todo - schema updates still need to be recieved
//                            $this->streamRead(
//                                [$this, 'serialiseOutput'],
//                                [$this->outputPlugin, 'sync']
//                            );

                            $this->processInputBuffers();
                            $this->outputPlugin->buffer->flushFinalised();
                            $this->outputPlugin->sync();
                            $this->scheduledStatusUpdate();
                            sleep(10);
                        }

                        SkipprLogger::info("Finished reading from stream socket.");

                        $this->shutdown();
                    }

                    if (Config::$runMode == Config::RUN_MODE_VALIDATE_CONFIG) {
                        $this->outputPlugin->doValidateConfig();
                        $this->shutdown();
                    }

                    if (Config::$runMode == Config::RUN_MODE_VALIDATE_CONNECTION) {
                        $this->outputPlugin->doValidateConnection();
                        $this->shutdown();
                    }

                    if (Config::$runMode == Config::RUN_MODE_SAVE) {
                        // @todo - need this?
                        $this->outputPlugin->doSave();
                        $this->shutdown();
                    }

                    if (Config::$runMode == Config::RUN_MODE_DELETE_PLUGIN) {
                        $this->outputPlugin->deletePlugin();
                        $this->shutdown();
                    }

                    if (Config::$runMode == Config::RUN_MODE_CREATE_UPDATE_DEST_SCHEMA) {
                        foreach (Config::$schema as $namespace => $avroSchema) {
                            SkipprLogger::info("Evolving $namespace destination schema");

                            $this->outputPlugin->createOrUpdateSchema(
                                $namespace,
                                $avroSchema
                            );
                        }
                        $this->shutdown();
                    }

                    if (Config::$runMode == Config::RUN_MODE_DELETE_DEST_SCHEMA) {
                        $this->outputPlugin->deleteSchema();
                        $this->shutdown();
                    }

                    if (Config::$runMode == Config::RUN_MODE_RESET_SOURCE_OFFSETS) {
                        foreach (Config::$schema as $namespace => $avroSchema) {
                            SkipprLogger::info("Deleting destination schema for $namespace");
                            $this->outputPlugin->deleteSchema($namespace);
                        }
                        $this->shutdown();
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

    protected function scheduledStatusUpdate(bool $force = false)
    {

        if ($force || $this->lastStatusUpdate < time() - $this->statusUpdateIntervalSeconds) {

//            if (extension_loaded('newrelic')) {
//                newrelic_ignore_transaction();
//            }

            SkipprLogger::info("Memory used ". BytesToHuman::toHuman(memory_get_usage(true), true));

            if (!empty($this->inputPlugin)) {
                SkipprLogger::info("Total messages " . NumberToHuman::toHuman($this->totalEntries));
                SkipprLogger::info("Rejected source messages " . NumberToHuman::toHuman($this->rejectedSrcMesgCount));
                SkipprLogger::info("Ingested messages " . NumberToHuman::toHuman($this->currentEntries));
                SkipprLogger::info("Fast Path messages " . NumberToHuman::toHuman($this->fastPath));
                SkipprLogger::info("Slow Path messages " . NumberToHuman::toHuman($this->slowPath));
                SkipprLogger::info("Ingested Bytes " . BytesToHuman::toHuman($this->dataReadBytes,
                        true));
                SkipprLogger::info("Runtime " . TimeToHuman::toHuman(Carbon::now()->timestamp - $this->startTimestamp,
                        true));

            }

            $resp = $this->getResponse();

            Config::setStatus($resp);
            Config::setConfig();

            $this->currentEntries = 0;
            $this->deadLettersCurrent = 0;
            $this->fastPath = 0;
            $this->slowPath = 0;
            $this->dataReadBytes = 0;

            $this->lastStatusUpdate = time();
        }
    }

    /**
     * Execute the console command.
     */
    public function init()
    {

        $this->statsd = new Statsd();

        if (Config::getenv('STATSD_HOST') && Config::getenv('STATSD_PORT')) {
            // @todo - factory stats interface (statsd + skippr enterpise http endpoint)
            $this->statsd->configure([
                'host' => Config::getenv('STATSD_HOST'),
                'port' => Config::getenv('STATSD_PORT'),
//            'namespace' => 'skippr'
            ]);
        }

        $this->skipprPack = new SkipprPack();

//        if (extension_loaded('newrelic')) { // Ensure PHP agent is available
//            newrelic_ignore_transaction();
//        }

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
//        $serde = SerdersFactory::factory('parquet');

        foreach (Config::$schema as $namespace => $schema) {
            $this->defaultMsgs[$namespace] = $serde->defaultMessage($schema);
        }

        $this->startTimestamp = Carbon::now()->timestamp;

        $this->getLicense();

        if (!$this->licenseIsValid) {
            SkipprLogger::info('Please validate license to continue using Skippr');

            $this->shutdown();
        }

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

        $isValid = true;

        $source_namespace = $payload['source_namespace'] ?? $isValid = false;
        $source_partition = $payload['source_partition'] ?? $isValid = false;
        $namespace = $payload['skpr_namespace'] ?? $isValid = false;
        $partition = (string) $payload['skpr_partition'] ?? $isValid = false;


//        if (!empty($payload) && !empty($offset)) {
        if ($isValid) {

            try {
                $offset = $this->inputPlugin->offsets->getCurrentOffsets(
                    $source_namespace,
                    $source_partition
                );

//                if (empty($offset)) {
//                    SkipprLogger::info("Offset: $offset");
//                }
            } catch (\TypeError $e) {
                // Sometimes get empty messages
                SkipprLogger::error($e->getMessage());
            }

            try {
//                $record = igbinary_serialize($payload);
//                $record = msgpack_pack($payload);
                $record = json_encode($payload);
                //            $serialised = pack("c*", $payload);

//                $this->skipprPack->encode($record, $offset);
//                $record = $this->skipprPack->string();
//                $offset = $this->skipprPack->decodeOffset();
//                $sizeBytes = $this->skipprPack->length();
                $sizeBytes = strlen($record);

//                $this->streamSend($record, null);

                $this->inputPlugin->offsets->setOffsets(
                    $offset,
                    $source_namespace,
                    $source_partition
                );

                if (Config::$syncMode == 'sync') {
                    $result = $this->inputPlugin->buffer->append(
                        $record,
                        $sizeBytes,
                        0,
                        $namespace,
                        $partition
                    );

                }
//                elseif (Config::$syncMode == 'async') {
//                    $result = $this->inputPlugin->buffer->append(
//                        $record,
//                        $sizeBytes,
//                        $eventTime,
//                        $namespace,
//                        $partition
//                    );
//                }
//
                $this->dataReadBytes += $sizeBytes;

                if ($result == 2) { // buffer was flushed

                    $this->offsetCommitRoutine(
                        $source_namespace,
                        $source_partition
                    );
                }

            } catch (\Exception $e) {
                SkipprLogger::error($e->getMessage());
                SkipprLogger::error("serialised: $record offset:$offset");
//                SkipprLogger::error("Failed to output, no offset or payload");

            } catch (\TypeError $e) {
                // Sometimes get empty messages
                SkipprLogger::error($e->getMessage());
            }
        }
    }


    public function processInputBuffers(bool $force = false): void
    {
        $file_list = glob($this->outputPlugin->buffer->driver->bufferDir . '/buffer=input' . '*&temp_part');

        if (!empty($file_list)) {
            foreach ($file_list as $filename) {

                if ($force || $this->outputPlugin->buffer->driver->checkFileBufferLimit($filename)) {

                    if ($this->outputPlugin->buffer->driver->lock($filename)) { // acquire an exclusive lock

                        $fpr = fopen($filename, 'rb');

                        while (($record = fgets($fpr)) !== false) {

//                            $payload = igbinary_unserialize($record);
//                            $payload = msgpack_unpack($record);
                            $payload = json_decode($record, true);
//                $payload = unpack("c*", $record);

//                SkipprLogger::info($record);
//                SkipprLogger::info(json_encode($payload));

                            if (!empty($payload)) {
//                            if (true) {
                                $eventTime = InternalFields::parseTimeField($payload); // time field config is set on the output
//                $eventTime = $payload['skpr_event_ts'];
                                $source_namespace = $payload['source_namespace'];
                                $source_partition = $payload['source_partition'];
                                $namespace = $payload['skpr_namespace'];
                                $partition = $payload['skpr_partition'];
                                // @todo - empty() performance
//                $partition = InternalFields::parsePartitionField($payload, $source_partition);

//                $this->outputPlugin->offsets->setOffsets(
//                    $offset,
//                    $source_namespace,
//                    $source_partition
//                );

                                if (Config::$syncMode == 'sync') {
                                    $result = $this->outputPlugin->buffer->append(
                                        $record,
                                        strlen($record),
                                        $eventTime,
                                        $namespace,
                                        $partition
                                    );

                                }

//                                SkipprLogger::info("unpacking input buffer $record");
                            } else {
                                SkipprLogger::info("Problem unpacking input buffer $record");
                            }
                        }

                        fclose($fpr);

                        $this->outputPlugin->buffer->flushAll(true);

                        $this->outputPlugin->buffer->driver->destroy($filename);


//                        $finalFilename = str_replace(
//                            'buffer=input',
//                            'buffer=output',
//                            $filename
//                        );
////                        $finalFilename = str_replace(
////                                '&temp_part',
////                                '&finalised',
////                                $finalFilename
////                            ) . '=' . Helpers::randomPassword(32);
//
//                        rename(
//                            $filename,
//                            $finalFilename
//                        );
//
//                        $updatedTime = filectime($finalFilename);
//
//                        $updatedDelta = time() - $updatedTime;
//                        $bytes = filesize($finalFilename);
//                        $humanSize = BytesToHuman::toHuman($bytes, true);
//
//                        SkipprLogger::info("Rotating input buffer file $filename finalised at $humanSize and age of $updatedDelta seconds");
//
//                        $this->destroy($filename);

                        $this->outputPlugin->buffer->driver->unlock($filename);
                    }
                }
            }
        }
    }

    public function serialiseOutput(string $skipprPack): void
    {

//        $this->scheduledStatusUpdate();


//        if (extension_loaded('newrelic')) {
//            newrelic_start_transaction('skipprd');
//            newrelic_name_transaction('output');
//        }

        $record = '';

        try {
            $this->skipprPack->create($skipprPack);
            $record = $this->skipprPack->decodeRecord();
//            $offset = $this->skipprPack->decodeOffset();
            $sizeBytes = $this->skipprPack->length();
        } catch (\Exception $e) {
            SkipprLogger::error($e->getMessage());
        }

//        if (!empty($record)) { // @todo - why do we get emtpy messages over the network sometimes?
            try {
                // @todo - often get 'Warning: [msgpack] (php_msgpack_unserialize) Extra bytes' without @
//                $payload = json_decode($record, true);

                // @todo - really don't understand where the control chars are coming from
                //         They break deserialisation of the SkipprPack record
                // - pretty sure the root cause was pack()-ing offsets. 'offset 123' was interpreted as \n
                // and so we parsed half a message.

//                $record = preg_replace('/[[:cntrl:]]/', '', $record);

//                $payload = igbinary_unserialize($record);
//                $payload = msgpack_unpack($record);
                $payload = json_decode($record, true);
//                $payload = unpack("c*", $record);

//                SkipprLogger::info($payload);

                $eventTime = InternalFields::parseTimeField($payload); // time field config is set on the output
//                $eventTime = $payload['skpr_event_ts'];
                $source_namespace = $payload['source_namespace'];
                $source_partition = $payload['source_partition'];
                $namespace = $payload['skpr_namespace'];
                $partition = $payload['skpr_partition'];
                // @todo - empty() performance
//                $partition = InternalFields::parsePartitionField($payload, $source_partition);

//                $this->outputPlugin->offsets->setOffsets(
//                    $offset,
//                    $source_namespace,
//                    $source_partition
//                );

                if (Config::$syncMode == 'sync') {
                    $result = $this->outputPlugin->buffer->append(
                        $record,
                        $sizeBytes,
                        $eventTime,
                        $namespace,
                        $partition
                    );

                } elseif (Config::$syncMode == 'async') {
                    $result = $this->outputPlugin->buffer->append(
                        $record,
                        $sizeBytes,
                        $eventTime,
                        $namespace,
                        $partition
                    );
                }

                $this->dataReadBytes += $sizeBytes;

                if ($result == 2) { // buffer was flushed

                    $this->offsetCommitRoutine(
                        $source_namespace,
                        $source_partition
                    );
                }

                $this->scheduledStatusUpdate();

//                $tenantId = Config::$tenantId;
//                $pipelineName = Config::$pipelineName;
//                $this->statsd->increment("$tenantId.$pipelineName.ingest.records.current", 1);

                // Empty only after writing, will ensure still available for graceful shutdown
                $this->hashes = [];
                $this->duplicateCount = 0;

                //        if (extension_loaded('newrelic')) {
                //            newrelic_end_transaction();
                //        }

            } catch (\AvroException $e) {

                try {
                    $this->deadLetterMessage($payload);
                } catch (\Exception $e) {
                    SkipprLogger::emergency('Failed to write to dead letter queue');
                    SkipprLogger::error($e->getMessage());
                }
            } catch (\Exception $e) {
//                SkipprLogger::info($record);
//                SkipprLogger::info("Namepsace $namespace, partition $partition, bytes $sizeBytes, event time $eventTime");
//                SkipprLogger::info(serialize($payload));

                SkipprLogger::error($e->getMessage());
                SkipprLogger::error($e->getTraceAsString());

            } catch (\Error $e) {
//                SkipprLogger::info($record);
//                SkipprLogger::info("Namepsace $namespace, partition $partition, bytes $sizeBytes, event time $eventTime");
//                SkipprLogger::info(serialize($payload));

                SkipprLogger::error($e->getMessage());
                SkipprLogger::error($e->getTraceAsString());
            }
//        }
    }


    public function deadLetterMessage(array $message)
    {

        // Don't dead letter message, if running the dead letter job
        // it will be skipped and so just remain in the queue
        if (Config::$enableDeadLetters) {
            $source_namespace = $message['source_namespace'];
            $source_partition = $message['source_partition'];
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
                    strlen(serialize($message)),
                    0,
                    $namespace,
                    $partition
                );
            }
            $tenantId = Config::$tenantId;
            $pipelineName = Config::$pipelineName;

//            $this->statsd->increment("$tenantId.$pipelineName.ingest.deadletters.current", 1);

            $this->deadLetters++;
        } else {
            SkipprLogger::error('Schema not valid for events in dead letter queue');

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
        string $source_namespace,
        string $source_partition = ''
    ): void {


        if (!empty($payload) && !empty($offset)) {
            if ($this->inputPlugin->offsets->validateOffset(
                $offset,
                $source_namespace,
                $source_partition
            )) {
                $this->inputPlugin->offsets->setOffsets(
                    $offset,
                    $source_namespace,
                    $source_partition
                );
                
                if (Config::$syncMode == 'sync') {
                    if (!Config::$enableDeadLetters) {
                        // @todo - deprecate SkipprPack for Apache Arrow
                        $sp = new SkipprPack($payload);
                        $payload = $sp->decodeRecord();
//                $offset = $sp->decodeOffset();
                    }

                    $this->emitString($payload, $source_namespace, $source_partition);
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
                        strlen($payload),
                        0,
                        $source_namespace,
                        $source_partition
                    );
                }
            }
        }
    }

    public function offsetCommitRoutine(
        string $source_namespace,
        string $source_partition
    ): void {
        if (!Config::$analysing) { // should never be here on analyse schema, but just in case of code error
            $offset = $this->inputPlugin->offsets->getCurrentOffsets(
                $source_namespace,
                $source_partition
            );

            SkipprLogger::info("Committing offset for Namespace: $source_namespace Partition: $source_partition Offset: $offset");

            $this->offsetClient->sync($source_namespace, $source_partition, $offset);
        }
    }

    public function resetSourceOffsets(): void
    {
        $offsets = $this->inputPlugin->offsets->getAll();

        foreach ($offsets as $source_namespace => $partitionArr) {
            foreach ($partitionArr as $source_partition => $offset) {
                SkipprLogger::info("Resetting offset for Namespace: $source_namespace Partition: $source_partition from current offset $offset to ''");
                $this->inputPlugin->offsets->setOffsets('', $source_namespace, $source_partition);
                $this->offsetClient->sync($source_namespace, $source_partition, '');
            }
        }
    }
    public function offsetCommitAll(): void
    {
        $offsets = $this->inputPlugin->offsets->getAll();

        foreach ($offsets as $source_namespace => $partitionArr) {
            foreach ($partitionArr as $source_partition => $offset) {
                SkipprLogger::info("Committing offset for Namespace: $source_namespace Partition: $source_partition Offset: $offset");
                $this->offsetClient->sync($source_namespace, $source_partition, $offset);
            }
        }
    }

    public function readFile(
        string $filename,
        string $source_partition,
        callable $callback
    ) {

        if (preg_match("/\.gz(ip)?$|.zip/", $filename) == true) {
            // open gz file for reading
            $sfp = gzopen($filename, 'rb');

            // read and decode chunks into string stream
            while (!gzeof($sfp)) {
                $string = gzgets($sfp);

                call_user_func($callback, $string, $source_partition);
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

            call_user_func($callback, $data, $source_partition);
//            foreach ($data as $item) {
//
//                call_user_func($callback, $item);
//            }
        } else {
            $sfp = fopen($filename, 'rb');

            // read and decode chunks into string stream
            while (!feof($sfp)) {
                $string = fgets($sfp);

                call_user_func($callback, $string, $source_partition);
            }

            // remove temp file
            fclose($sfp);
        }
    }

    public function emitFile(
        string $filename,
        string $namespace,
        string $source_partition = ''
    ): void {

        $fields = [];
        $payloadString = '';

        $serde = SerdersFactory::factory(Config::$sourceFormat);

        $this->readFile(
            $filename,
            $source_partition,
            function (
                $string,
                $source_partition
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
                        $this->emitArray($msg, $namespace, $source_partition);
                    }
                }
            }
        );

        if (in_array(Config::$sourceFormat, Config::$batchFormats)) {
            $msgs = $serde->deserialize($payloadString);

            foreach ($msgs as $msg) {
//                array_push($fields, $msg);
                $this->emitArray($msg, $namespace, $source_partition);
            }
        }
    }

    public function emitString(
        string $payload,
        string $source_namespace,
        string $source_partition = ''
    ): void {

//        if (extension_loaded('newrelic')) {
//            newrelic_end_transaction(true);
//            newrelic_start_transaction('skipprd');
//            newrelic_name_transaction('input');
//        }

        if ($payload != '') {
            if (Config::$syncMode == 'sync') {
                if (!Config::$enableDeadLetters) {
                    // @todo - deprecate SkipprPack for Apache Arrow
                    $sp = new SkipprPack($payload);
                    $payload = $sp->decodeRecord();
//                $offset = $sp->decodeOffset();
                }

                $sourceMessages = $this->inputSerder->deserialize($payload);

                $this->emitArray($sourceMessages, $source_namespace, $source_partition);
            }
        }

//        if (extension_loaded('newrelic')) {
//            newrelic_end_transaction();
//        }
    }

    /**
     * All emit functions end up here after deserialising payload
     * @param array $payload - the payload to ingest
     * @param string $source_namespace - the schema namespace (table, avro namespace, event type, etc)
     * @param string $source_partition - data sources partition (shard, kafka topic, index, FS dir, etc)
     */
    public function emitArray(
        array $payload,
        string $source_namespace,
        string $source_partition = ''
    ): void
    {

        $unwrappedMessages = $this->unwrapEventPath($payload);

        // @todo - investigate why messages are always wrapped in an array... indeed, are they!?
        if ($unwrappedMessages) {
            foreach ($unwrappedMessages as $unwrappedMessage) {
                try {
                    $this->ingest($unwrappedMessage, $source_namespace, $source_partition);
                } catch (\TypeError $e) {
                    // Sometimes get empty messages
                    $this->rejectedSrcMesgCount++;
//                    SkipprLogger::error($e->getMessage());
                }
            }
        }
    }

    public function ingest(
        array $payload,
        string $source_namespace,
        string $source_partition = ''
    ): void
    {

//            $this->getIdFields($payload); // slow and we don't even use this

        // @todo - stuff like this, do an empty() once and store a bool var
        $namespace = InternalFields::parseNamespaceField($payload, $source_namespace);
        $payload['skpr_partition'] = '';
        $payload['source_namespace'] = $source_namespace;
        $payload['source_partition'] = $source_partition;

        if (Config::$flattenEvents) {
//                if (is_array($payload)) {
            $payload = Helpers::flatten($payload);
//                }
        }

        if (Config::$analysing && $this->inputPlugin->ingestNamespace($namespace) === true) {

            $this->analyse($payload, $namespace);

        }


        if (!Config::$analysing) {

            if (RecordFilter::filter($payload)) {

                /*
                 * Ingestion
                 */
                if (Config::$mutableMode !== Config::MUTABLE_MODE_STRICT) {

                    try {
                        $message = $this->fastPathIngest($payload, $namespace);

                    } catch (\Exception $e) {
//                        SkipprLogger::error($e->getMessage());
//                        SkipprLogger::error($e->getTraceAsString());
                        $message = $this->slowPathIngest($payload, $namespace);

                    } catch (\Error $e) {
//                        SkipprLogger::error($e->getMessage());
//                        SkipprLogger::error($e->getTraceAsString());
                        $message = $this->slowPathIngest($payload, $namespace);
                    }

                } else {
                    $message = $payload;
                }

                if ($message !== false) { // even slow path resolution fail can fail on breaking changes in source data

                    $this->outputEmit($message);
                }

                $this->scheduledStatusUpdate();
            }
        }
    }

    public function unwrapEventPath(array $sourceMessages)
    {

        if (Config::$eventPath) {
            if (!empty($sourceMessages) && is_array($sourceMessages)) {
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

            if (!empty($unwrappedMessages)) {
                $sourceMessages = $unwrappedMessages;
            }

//            return $sourceMessages;
//
//        } else {
//            return false;
        }

        return $sourceMessages;

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
                foreach ($offsets as $source_namespace => $offsetsParts) {
                    foreach ($offsetsParts as $source_partition => $offset) {
                        $this->inputPlugin->offsets->setOffsets(
                            $offset,
                            $source_namespace,
                            $source_partition
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
                foreach ($offsets as $source_namespace => $offsetsParts) {
                    foreach ($offsetsParts as $source_partition => $offset) {
                        $this->outputPlugin->offsets->setOffsets(
                            $offset,
                            $source_namespace,
                            $source_partition
                        );
                    }
                }
            }


            $this->outputPlugin->connect();
            $this->outputPlugin->buffer->driver->unlockAll();
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

    public function exceptionsErrorHandler(int $severity, string $message, string $filename, int $lineno): void
    {

//        if (error_reporting() & $severity) {
//            Config::$taskLogs[] = $message;
//            Config::$taskLogs[] = "In filename: $filename, line: $lineno";
//        }
//        SkipprLogger::error($message);
//        SkipprLogger::error("In filename: $filename, line: $lineno");

//        SkipprLogger::critical("Uncaught error, shutting down.");

//        $this->shutdown(137);
    }

    public function exceptionHandler(\Throwable $e): void
    {

        SkipprLogger::error($e->getMessage());
        SkipprLogger::error($e->getTraceAsString());

        SkipprLogger::critical("Uncaught exception, shutting down.");

        $this->shutdown(137);
    }


    public function shutdownSig(int $signo, $siginfo): void
    {

        SkipprLogger::info("Received SIGNAL.");

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

            if ($signo !== 1000) { // 1000 = output container gone away
                if (!Config::$analysing && Config::$runMode == Config::RUN_MODE_SYNC) {
                    // send shutdown signal to output
                    $skipprPack = new SkipprPack();
                    $skipprPack->encode('sync_complete', '');

                    $this->streamSend($skipprPack); // don't use STREAM_OOB, we want to ingest the whole TCP buffer

                    fclose($this->sock);
                }
            }
        }

//        if (Config::$syncMode == 'async') {
//            $this->inputPlugin->buffer->driver->unlockAll();
////            $this->inputPlugin->buffer->flush("input");
//            $this->inputPlugin->buffer->flushAll(true);
//
//            if ($this->threadPool !== null) {
//                foreach ($this->threadPool as $thread) {
//                    $thread->kill();
//                }
//            }
//
//            if ($this->outputThread != null) {
//                $this->outputThread->kill();
//            }
////        }
//
////        if (!Config::$analysing) {
//            $this->outputPlugin->buffer->driver->unlockAll();
////            $this->outputPlugin->buffer->flush("out");
//            $this->outputPlugin->buffer->flushAll(true);
//
//            if (!empty($this->deadletterPlugin)) {
//                $this->deadletterPlugin->buffer->driver->unlockAll();
//                $this->deadletterPlugin->buffer->flushAll(true);
//            }
//        }

        if (Config::$syncMode == 'sync') {
            if (!Config::$analysing) {
                if (!empty($this->inputPlugin)) {
                    $this->inputPlugin->buffer->driver->unlockAll();
                    $this->inputPlugin->buffer->flushAll(true);
                    $this->offsetCommitAll();
                    SkipprLogger::info("Ingested " . $this->currentEntries . " messages");
                    SkipprLogger::info("Dead Letters " . $this->deadLetters . " dead letters");
                }

                if (!empty($this->outputPlugin)) {
                    $outputPluginName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');
                    SkipprLogger::info("Flushing output buffers to $outputPluginName destination.");
                    $this->outputPlugin->buffer->driver->unlockAll();

                    $this->processInputBuffers(true);
                    $this->outputPlugin->buffer->flushFinalised(true);
                    $this->outputPlugin->sync();

                    $this->outputPlugin->shutdown();
                }

                if (!empty($this->deadletterPlugin)) {
                    $deadLetterPluginName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');
                    $this->deadletterPlugin->buffer->driver->unlockAll();
//                    $this->deadletterPlugin->buffer->flushFinalised(true);
                    SkipprLogger::info("Flushing dead letter buffers to $deadLetterPluginName destination.");
                    $this->deadletterPlugin->sync();
                    $this->deadletterPlugin->shutdown();
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
            $this->scheduledStatusUpdate(true);
        }

        // don't update final output job status when exiting due to the source container
        // sending a sync_complete event. These result in essential container tak exited in ECS (exit code 15)
        if (!empty($this->outputPlugin)) {
            Config::setStatus();
        }

//        SkipprLogger::debug("Mem used: " . BytesToHuman::toHuman(memory_get_usage(true), true, 'MB'));
//        SkipprLogger::debug("Mem limit: " . BytesToHuman::toHuman($this->flushBytes, true, 'MB'));

        // Update metadata such as field mapping
//        $configYml = $this->setConfig();
//        $configYml['update_status'] = true;
//        $configYml['status'] = false;
//        event(new WorkerConfigRequested($configYml));

        $inputName = Config::getenv('DATA_SOURCE_PLUGIN_NAME');
        $outputName = Config::getenv('DATA_OUTPUT_PLUGIN_NAME');
        
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
                                // Prefer primitive types to logical types or types
                                // that cause frequent false positives (demoted types).
                                // - if there's multiple discovered types
                                // - and the most common type is a demoted type
                                // - select the next most common, non-date type
//                                if (!in_array($dataType, $demotedTypes)) {
                                if (count($typeCount) <= 1
                                    || (count($typeCount) > 1 && !in_array(
                                            $dataType,
                                            $demotedTypes
                                        ))) {
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

    public function getIdFields(array $message)
    {

        $messageEntityNames = [];

//        if (!empty($message)) {
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
//        }

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
