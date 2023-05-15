<?php

namespace legacy\src\Skipprd\Commands;

//use App\IngestJob;
//use App\Services\KubeClient;
//use Illuminate\Bus\Queueable;
//use Illuminate\Contracts\Queue\ShouldQueue;
//use Illuminate\Foundation\Bus\Dispatchable;
//use Illuminate\Queue\InteractsWithQueue;
//use Illuminate\Queue\SerializesModels;
//use Illuminate\Support\Facades\Cache;
//use Illuminate\Support\Facades\Log;
//use Iq\Events\PodsStatusRequested;
//use League\StatsD\Laravel5\Facade\StatsdFacade as Statsd;
//use RdKafka\TopicPartition;

class PodsStatus
{

//    private static $statusInt = [
//        'Unknown' => 0,
//        'Pending' => 1,
//        'Running' => 2,
//        'Succeeded' => 3,
//        'Failed' => 4,
//    ];

    private static $statusInt = [
        'Unknown' => 0,
        'Running' => 2,
        'Complete' => 3,
        'Failed' => 4,
    ];

//    use InteractsWithQueue;
//
//    public function handle(PodsStatusRequested $configRequest)
//    use Dispatchable, InteractsWithQueue, Queueable, SerializesModels;
//
//
//    /**
//     * Create a new job instance.
//     *
//     * @return void
//     */
//    public function __construct()
//    {
//    }
//
//    /**
//     * Execute the job.
//     *
//     * @return void
//     */
//    public function handle()
//    {
//
//        $ingestJobs = IngestJob::get()->all();
//
//        foreach ($ingestJobs as $ingestJob) {
//
//            try {
//
//                foreach ($ingestJob->getEcsTasks() as $jobName => $args) {
//
//                    $statusInt = 0;
//
//                    $config = $ingestJob->getJobConfig($jobName);
//
//                    $kubeClient = new KubeClient($config);
//
//                    $status = $kubeClient->getStatus();
//
//                    $phase = 'Unknown';
//
//                    if (!empty($status)) {
//
//                        $phase = $this->parseStatusCondition($status);
//
////                        if ($ingestJob->status <= self::$statusInt[$phase]) {
//
//                            $statusInt = self::$statusInt[$phase];
////                        }
//                    }
//
//                    Log::info("$jobName status is $phase");
//
//                    $ingestJob->status = $statusInt;
//                    $ingestJob->save();
//
//                }
//
//                Statsd::gauge("ingest.pipelines.total.{$ingestJob->tenant_id}", 1);
//
//                Statsd::increment("ingest.pipelines.current.{$ingestJob->tenant_id}", 1);
//
//            } catch (\Exception $e) {
////                Log::error('Caught exception: ' . $e->getMessage());
////                Log::error('On line: ' . $e->getLine());
////                Log::error('Of file: ' . $e->getFile());
//
//            }
//        }
//
//    }
//
//    public function parseStatusCondition(object $statusArr) : string {
//
//
//        $data = $statusArr->getData();
//
//        if (!empty($data['conditions'][0]['type'])) {
//
//            if ($data['conditions'][0]['status'] = true) {
//                return $data['conditions'][0]['type'];
//            }
//
//        }
//
//        if (!empty($data['startTime'])
//            && empty($data['completionTime']) ) {
//
//            return 'Running';
//
//        }
//
//        return 'Unknown';
//    }
}
