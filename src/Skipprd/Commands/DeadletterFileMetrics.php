<?php

namespace Skipprd\Commands;

//use App\IngestJob;
//use Illuminate\Contracts\Queue\ShouldQueue;
//use Illuminate\Queue\InteractsWithQueue;
//use Illuminate\Support\Facades\Cache;
//use Illuminate\Support\Facades\Log;
//use League\StatsD\Laravel5\Facade\StatsdFacade as Statsd;
//use RdKafka\TopicPartition;

class DeadletterFileMetrics
{

//    use InteractsWithQueue;
//
//    public function handle(DeadletterMetricsRequested $configRequest)
//    {
//
//        try {
//
//            $ingestJob = $configRequest->ingestJob;
//
//            $bufferName = 'deadletter-raw_' . $ingestJob->tenant_id . '_' . $ingestJob->getCleanName() .'_deadletter';
//
//            $data = [];
//            $data['consumer_lag'] = 0;
//
//            $fileBuffer = new FileBuffer();
//
////            $identifier = $ingestJob->tenant_id . '-' . $ingestJob->getCleanName() . '-skipprd-skipprd';
////            $fileBuffer->tempdir = '/data/' . $identifier;
//
//            $lines = $fileBuffer->bufferGetNoLines($bufferName);
//
//            $data['consumer_lag'] = $lines;
//
//            // Create Lag metric
//            if ($data['consumer_lag'] > -1) {
//
//                Statsd::gauge("ingest.deadletters.total.{$ingestJob->tenant_id}.{$ingestJob->getCleanName()}",  $data['consumer_lag']);
//
//                $total_lag = $data['consumer_lag'];
//
//                Log::debug("Found $total_lag lag for consumer {$ingestJob->tenant_id} {$ingestJob->getCleanName()}");
//
//            }
//
//        } catch (\Exception $e) {
//            Log::error('Caught exception: ' . $e->getMessage());
//            Log::error('On line: ' . $e->getLine());
//            Log::error('Of file: ' . $e->getFile());
//
//        }
//
//    }
}
