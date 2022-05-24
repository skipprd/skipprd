<?php

namespace Skipprd\Commands;

//use App\IngestJob;
//use Illuminate\Contracts\Queue\ShouldQueue;
//use Illuminate\Queue\InteractsWithQueue;
//use Illuminate\Support\Facades\Cache;
//use Illuminate\Support\Facades\Log;
//use Iq\Events\DeadletterMetricsRequested;
//use League\StatsD\Laravel5\Facade\StatsdFacade as Statsd;
//use RdKafka\TopicPartition;

class DeadletterKafkaMetrics
{

//    use InteractsWithQueue;
//
//    public function handle(DeadletterMetricsRequested $configRequest)
//    {

//        try {
//
//            $ingestJob = $configRequest->ingestJob;
//
//            $conf = new \RdKafka\Conf();
//
//            $conf->set('metadata.broker.list', env('KAFKA_BROKERS'));
//            $conf->set('auto.offset.reset', 'smallest');
//            $conf->set('enable.auto.commit', 'false');
//            $conf->set('enable.partition.eof', 'true');
//            $conf->set('log.connection.close', 'false');
////            $conf->set('debug', 'all');
////            $conf->set('security.protocol', 'ssl');
//
//            $topic = 'raw_' . $ingestJob->tenant_id . '_' . $ingestJob->getCleanName() .'_deadletter';
//            $kafkaTopics = [$topic];
//
//            // Get offsets for consumer group.id
//            $groupId = $ingestJob->tenant_id . '.' . $ingestJob->getCleanName();
//
//            $conf->set('group.id', $groupId);
//
//            $data = [];
//            $data['consumer_lag'] = 0;
//
//            $consumer = new \RdKafka\KafkaConsumer($conf);
//
//            $consumer->subscribe($kafkaTopics);
//            $metadata = $consumer->getMetadata(true, null, 60e3);
//
//            $totalOffsets = 0;
//            $committedOffsets = 0;
//
//            foreach ($metadata->getTopics() as $topic) {
//                $topicName = $topic->getTopic();
//                if (in_array($topicName, $kafkaTopics)) {
//                    $partitions = $topic->getPartitions();
//                    foreach ($partitions as $partition) {
//                        $low = 0;
//                        $high = 0;
//                        $consumer->queryWatermarkOffsets($topicName, $partition->getId(), $low, $high, 60e3);
//
//                        $totalOffsets += ($high - $low);
//
//                        $topicPartitions[] = new TopicPartition($topicName, $partition->getId());
//
//                    }
//
//                    $topicPartitionsWithOffsets = $consumer->getCommittedOffsets($topicPartitions, 60e3);
//                    foreach ($topicPartitionsWithOffsets as $key => $topicPartitionWithOffset) {
//
//                        $offset = $topicPartitionWithOffset->getOffset();
//
//                        if ($offset > -1) {
//                            $committedOffsets += $offset;
//                        }
//                    }
//                }
//            }
//
//            $data['total_offsets'] = $totalOffsets;
//            $data['committed_offsets'] = $committedOffsets;
//            $data['consumer_lag'] = ($totalOffsets - $committedOffsets);
//
//            // Create Lag metric
//            if ($data['consumer_lag'] > -1) {
//
//                Statsd::gauge("ingest.deadletters.total.{$ingestJob->tenant_id}.{$ingestJob->getCleanName()}",  $data['consumer_lag']);
//
//                $total_lag = $data['consumer_lag'];
//
//                Log::debug("Found $total_lag lag for consumer $groupId");
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
