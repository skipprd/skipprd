use crate::buffer::{BufferChunker};
use crate::discover::{AnalyseSchema, Metadata, NUM_ANALYSED_RECORDS, OutputMetadata, PipelineMetadata};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::{OffsetKey, Offsets, OffsetTypes};
use crate::helpers::Helpers;
use crate::ingest::ingest::ingest;
use crate::serdes::json::SerdeJson;
use crate::{ARROW_SCHEMA, METADATA, METRICS, RUNNING};


use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};


use std::io::{BufWriter, Write};
use std::ops::Deref;

use std::process::exit;
use std::string::ToString;
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime, Duration};
use threadpool::ThreadPool;
use std::sync::mpsc::channel;
extern crate num_cpus;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicUsize, Ordering, AtomicU64};
use std::sync::atomic::Ordering::AcqRel;
use dashmap::{DashMap};

use crate::ingest::fast_ingest::{create_default_nested_message, DEFAULT_NESTED_MESSAGE, fast_path_ingest};

use crate::helpers::timed_rwlock::TimedRwLock;
use crate::buffer::ingest_buffer::{Buffers, IngestBufferBatch, IngestRecord};
use crate::serdes::csv::SerderCsv;

use crate::serdes::xml::SerdeXml;
// use crate::converters::skippr_avro::convert_skippr_to_avro_field_types;

use arrow::error::ArrowError;
use arrow::datatypes;
use arrow_schema::SchemaRef;
use serde_derive::{Deserialize, Serialize};
use tokio::runtime;
use crate::cli::{CLI_MODE, Mode};
use crate::converters::skippr_arrow::convert_skippr_to_arrow;
use crate::plugins::DataOutputPlugin;
use std::collections::VecDeque;
use libc::rand;
use rand::{random, Rng};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deadletter {
    pub(crate) namespace: String,
    pub(crate) partition: String,
    pub(crate) time: u64,
    pub(crate) error: String,
    pub(crate) records: String,
}

#[derive(Clone, Debug)]
pub struct IngestBatch {
    pub(crate) offset_key: OffsetKey,
    pub(crate) data: String,
    pub(crate) bytes: usize,
}

// Bare metal platforms usually have very small amounts of RAM
// (in the order of hundreds of KB)
pub const _WRITE_BUF_SIZE: usize = if cfg!(target_os = "espidf") {
    512
} else {
    512 * 1024
};

thread_local! {
    pub static PARSE_NAMESPACE_CACHE: Lazy<RwLock<HashMap<String, String>>> = Lazy::new(|| RwLock::new(HashMap::new()));

    pub static PARTITION_ALLOWED_VALUES_CACHE: Lazy<RwLock<HashSet<String>>> = Lazy::new(|| RwLock::new(HashSet::new()));
}

pub static DEADLETTER_FILE_NAME: Lazy<String> = Lazy::new(|| BufferChunker::encode_chunk_name(
    "deadletters",
    Some(Config::get_pipeline_name().as_str()),
    None,
    None,
    None
));

pub static DEADLETTER_FILE: Lazy<Arc<TimedRwLock<BufWriter<File>>>> = Lazy::new(|| {
    let data_dir = Config::get_data_dir();
    let deadletter_dir = format!("{}/deadletter_buffer", data_dir);
    let output_file = format!("{}/{}", deadletter_dir.clone(), &DEADLETTER_FILE_NAME.as_str());
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_file)
        .unwrap();

    let writer = BufWriter::new(file);

    Arc::new(TimedRwLock::new("deadletter_file".to_string(), writer))
});

#[derive(Clone, Debug)]
struct SchemaHash {
    schema: SchemaRef,
    hash: String,
}

#[derive(Clone)]
struct IngestTask {
    datas: Arc<Vec<IngestBatch>>,
    offset_db: Arc<Offsets>,
    shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>
}

/// Return type for ingest_file that includes throughput metrics
#[derive(Clone, Debug)]
pub struct ThroughputMetrics {
    pub bytes_per_second: u64,
    pub active_cores: usize,
    pub queue_length: usize,
    pub optimal_chunk_size: usize,
}

/// Main struct for managing ingestion of data
/// Handles queueing, processing, and distribution of tasks to worker threads
pub struct Ingest {
    thread_pool: Arc<ThreadPool>,
    num_cpus: usize,
    tx: Sender<u64>,
    active_count: Arc<AtomicUsize>, // Track total number of active threads
    queue_length: Arc<AtomicUsize>,  // Track total number of queued tasks
    schema_hashes: DashMap<String, SchemaHash>,
    analyse_schema: AnalyseSchema,
    throughput_window: Arc<RwLock<VecDeque<(Instant, u64)>>>,
    throughput_lock: Arc<RwLock<()>>,
    window_size: Duration,
    task_queue: Arc<RwLock<VecDeque<IngestTask>>>,
    queue_lock: Arc<RwLock<()>>,
    is_shutting_down: Arc<AtomicUsize>, // Flag to indicate shutdown in progress
    max_queue_length: usize, // Maximum number of tasks to queue
    optimal_chunk_size: Arc<AtomicUsize>,
    throughput_history: Arc<RwLock<VecDeque<(Instant, u64)>>>, // Track throughput over time
    // last_adjustment: Arc<RwLock<Instant>>, // Track when we last adjusted chunk size
    // adjustment_cooldown: Duration, // Minimum time between adjustments
}

impl Drop for Ingest {
    fn drop(&mut self) {
        println!("Completing, waiting for {} ingest tasks to finish", self.active_count.load(Ordering::SeqCst));

        self.wait_for_completion();
    }
}

impl Ingest {
    pub fn new() -> Ingest {
        // We always want to use all cores       
        let num_cpus = match CLI_MODE.read().clone() {
            Mode::Sync(_) => {
                num_cpus::get()
            },
            _ => 1
        };
        
        println!("Starting with {} optimized threads for ingest", num_cpus);

        let (tx, rx) = channel();
        let tx_clone = tx.clone();
        let active_count = Arc::new(AtomicUsize::new(0));
        let active_count_clone = active_count.clone();
        let queue_length = Arc::new(AtomicUsize::new(0));
        let optimal_chunk_size = Arc::new(AtomicUsize::new(5_000_000));
        let queue_length_clone = queue_length.clone();
        let task_queue: Arc<RwLock<VecDeque<IngestTask>>> = Arc::new(RwLock::new(VecDeque::new()));
        let task_queue_clone = task_queue.clone();
        let queue_lock = Arc::new(RwLock::new(()));
        let queue_lock_clone = queue_lock.clone();
        let is_shutting_down = Arc::new(AtomicUsize::new(0));
        let is_shutting_down_clone = is_shutting_down.clone();

        let thread_pool = Arc::new(ThreadPool::new(num_cpus));
        let thread_pool_clone = thread_pool.clone();
        
        let max_queue_length = num_cpus * 2;
        
        // This monitoring thread tracks task completion and processes queued tasks
        thread_pool.execute(move || {
            while let Ok(_) = rx.recv() {
                // Check if we're shutting down
                // if is_shutting_down_clone.load(Ordering::SeqCst) > 0 {
                //     self.wait_for_completion();
                //     println!("Shutting down monitoring thread");
                //     break;
                // }


                active_count_clone.fetch_sub(1, AcqRel);
                queue_length_clone.fetch_sub(1, AcqRel);

                let current_queue_length = queue_length_clone.load(Ordering::Acquire);
                let current_active_threads = active_count_clone.load(Ordering::Acquire);

                // println!("Task completed ({} tasks in queue, {}/{} active threads)",
                //          current_queue_length,
                //          current_active_threads,
                //          num_cpus
                // );

                // Process any queued tasks if we have capacity
                if current_active_threads < num_cpus {
                    let _lock = match queue_lock_clone.write() {
                        Ok(lock) => lock,
                        Err(e) => {
                            println!("Failed to acquire queue lock: {:?}", e);
                            continue;
                        }
                    };
                    
                    let mut task_queue = match task_queue_clone.write() {
                        Ok(queue) => queue,
                        Err(e) => {
                            println!("Failed to acquire task queue: {:?}", e);
                            continue;
                        }
                    };
                    
                    // Process tasks from the queue while we have capacity
                    while let Some(ingest_task) = task_queue.pop_front() {
                        if active_count_clone.load(Ordering::Acquire) >= num_cpus {
                            // Put the task back if we're at capacity
                            task_queue.push_front(ingest_task);
                            break;
                        }

                        // Process the queued task
                        let tx = tx_clone.clone();
                        let offset_db_clone = ingest_task.offset_db.clone();
                        let datas_clone = ingest_task.datas.clone();
                        let mut schema_hashes = DashMap::new();
                        
                        let shared_output_clone = ingest_task.shared_output.clone();

                        // Increment active count before spawning
                        active_count_clone.fetch_add(1, AcqRel);
                        // Note: We don't increment queue_length here since we're processing from the queue,
                        // and the task was already counted in queue_length when it was added to the queue

                        thread_pool_clone.execute(move || {
                            // Create a new runtime for this thread
                            let rt = tokio::runtime::Runtime::new().unwrap();
                            let handle = rt.handle().clone();
                            
                            // Process the batch
                            Ingest::process_batch(&datas_clone, &offset_db_clone, &mut schema_hashes, handle, shared_output_clone);

                            // Don't panic if sending fails (channel might be closed during shutdown)
                            let _ = tx.send(0);
                        });
                    }
                }
            }
            println!("Monitoring thread exited");
        });

        // Schema hashes
        let schema_hashes = DashMap::new();

        {
            let schemas = ARROW_SCHEMA.read();
            for (namespace, schema) in schemas.iter() {
                let schema_hash = SchemaHash {
                    schema: Arc::clone(schema),
                    hash: format!("{:?}", md5::compute(format!("{:?}", Arc::clone(schema).deref())))
                };
                schema_hashes.insert(namespace.clone(), schema_hash);
            }
        }

        let analyse_schema: AnalyseSchema = AnalyseSchema { i: 0 };
        let throughput_window = Arc::new(RwLock::new(VecDeque::with_capacity(100)));
        let throughput_lock = Arc::new(RwLock::new(()));
        let window_size = Duration::from_secs(5); // 5 second window for throughput calculation

        let throughput_history = Arc::new(RwLock::new(VecDeque::with_capacity(100)));
        let last_adjustment = Arc::new(RwLock::new(Instant::now()));
        // let adjustment_cooldown = Duration::from_secs(10); // 10 second cooldown between adjustments

        Ingest {
            num_cpus,
            thread_pool,
            tx,
            active_count,
            queue_length,
            schema_hashes,
            analyse_schema,
            throughput_window,
            throughput_lock,
            window_size,
            task_queue,
            queue_lock,
            is_shutting_down,
            max_queue_length,
            optimal_chunk_size,
            throughput_history,
            // last_adjustment,
            // adjustment_cooldown,
        }
    }

    pub fn wait_for_completion(&self) {
        // Signal that we're shutting down
        self.is_shutting_down.store(1, Ordering::SeqCst);
        
        let mut current_active_count = self.active_count.load(Ordering::SeqCst);
        let mut last_report_time = Instant::now();

        println!("Waiting for {} ingest tasks to finish, signaling shutdown...", current_active_count);

        // Give tasks a chance to complete gracefully
        let timeout = Instant::now() + Duration::from_secs(60); // 1 minute timeout
        
        while self.active_count.load(Ordering::SeqCst) > 0 && Instant::now() < timeout {
            if current_active_count != self.active_count.load(Ordering::SeqCst) || last_report_time.elapsed() > Duration::from_secs(5) {
                println!("Waiting for {} ingest tasks to finish", self.active_count.load(Ordering::SeqCst));
                current_active_count = self.active_count.load(Ordering::SeqCst);
                last_report_time = Instant::now();
            }
            
            // Use exponential backoff to avoid excessive CPU usage when waiting
            if current_active_count > 10 {
                std::thread::sleep(std::time::Duration::from_millis(500));
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        // If we still have active threads after timeout, force decrement them
        if self.active_count.load(Ordering::SeqCst) > 0 {
            println!("Forcing completion of {} remaining tasks after timeout", self.active_count.load(Ordering::SeqCst));
            self.active_count.store(0, Ordering::SeqCst);
            self.queue_length.store(0, Ordering::SeqCst);
            // Clear the task queue
            if let Ok(mut task_queue) = self.task_queue.write() {
                task_queue.clear();
            }
        }

        println!("All ingest tasks finished or timed out");
    }

    fn update_throughput(&self, bytes: u64) {
        let now = Instant::now();
        let _lock = self.throughput_lock.write().unwrap();
        let mut window = self.throughput_window.write().unwrap();
        
        // Add new measurement
        window.push_back((now, bytes));
        
        // Remove old measurements outside window
        while let Some((time, _)) = window.front() {
            if now.duration_since(*time) > self.window_size {
                window.pop_front();
            } else {
                break;
            }
        }
    }

    fn get_current_throughput(&self) -> u64 {
        let now = Instant::now();
        let _lock = self.throughput_lock.read().unwrap();
        let window = self.throughput_window.read().unwrap();
        
        if window.is_empty() {
            return 0;
        }

        let oldest_time = window.front().unwrap().0;
        let total_bytes: u64 = window.iter().map(|(_, bytes)| bytes).sum();
        let duration = now.duration_since(oldest_time).as_secs_f64();
        
        if duration == 0.0 {
            return 0;
        }

        (total_bytes as f64 / duration) as u64
    }

    /**
     * Optimise the IngestTask chunk size to keep all CPU cores busy and ensure we're queueing the right amount of tasks.
     * Reduce the chunk size if we have lest active_tasks than cores or the queue isn't full.
     * Increase the chunk size if we have more active_tasks than cores and a full queue.
     */
    fn get_optmial_chunk_size(&self) {

        let now= Instant::now();

        let active_cores = self.active_count.load(Ordering::Acquire);
        let queue_length = self.queue_length.load(Ordering::Acquire);
        let current_throughput = self.get_current_throughput();
        let current_chunk_size = self.optimal_chunk_size.load(Ordering::Acquire);
        let optimal_chunk_size_min = 1_000_000;

        // Update throughput history
        {
            let mut history = self.throughput_history.write().unwrap();
            history.push_back((now, current_throughput));
            
            // Keep only last 10 measurements
            while history.len() > 10 {
                history.pop_front();
            }
        }

        // Calculate throughput trend
        let throughput_trend = {
            let history = self.throughput_history.read().unwrap();
            if history.len() < 2 {
                0.0
            } else {
                let oldest = history.front().unwrap();
                let newest = history.back().unwrap();
                let time_diff = newest.0.duration_since(oldest.0).as_secs_f64();
                if time_diff == 0.0 {
                    0.0
                } else {
                    (newest.1 as f64 - oldest.1 as f64) / time_diff
                }
            }
        };

        let mut adjustment_factor = 1.0; // Default to no change

        // Only adjust if we're at capacity
        if active_cores >= self.num_cpus && queue_length >= self.max_queue_length {

            // Randomly check throughput trend to avoid oscillation to avoid oscillation
            if random::<u64>() % (*self.num_cpus as u64 * 2) == 0 {

                if throughput_trend > 0.0 {
                    // Throughput is increasing, continue increasing chunk size
                    adjustment_factor = 1.1;
                } else if throughput_trend < 0.0 {
                    // Throughput is decreasing, reduce chunk size
                    adjustment_factor = 0.9;
                }

            }
        } else if active_cores < self.num_cpus && queue_length < ( self.max_queue_length as f32 / 0.2) as  usize {
            // Reduce chunk size if we have capacity
            adjustment_factor = 0.9;
        }


        let mut optimal_chunk_size = (current_chunk_size as f64 * adjustment_factor) as usize;

        if optimal_chunk_size < optimal_chunk_size_min {
            optimal_chunk_size = optimal_chunk_size_min;
        }

        if current_chunk_size != optimal_chunk_size {
            println!("Optimising chunk size: active_cores: {}, active_tasks: {}, throughput: {}/s, trend: {:.2}, optimal_chunk_size: {} from {}, adjustment_factor: {}",
                     active_cores,
                     queue_length,
                     Helpers::human_readable_size(current_throughput),
                     throughput_trend,
                     Helpers::human_readable_size(optimal_chunk_size as u64),
                     Helpers::human_readable_size(current_chunk_size as u64),
                     adjustment_factor
            );
            
            self.optimal_chunk_size.store(optimal_chunk_size, Ordering::SeqCst);
            // *self.last_adjustment.write().unwrap() = now;
        }
    }

    /// Add a file to the ingestion queue
    /// 
    /// This function will either process the file immediately if there is capacity
    /// or queue it for later processing. It includes backpressure handling to prevent
    /// unbounded queue growth.
    /// 
    /// Returns: A ThroughputMetrics struct containing current system state and optimal chunk size
    pub fn ingest_file(
        &self,
        datas: &Arc<Vec<IngestBatch>>,
        offset_db: &Arc<Offsets>,
        shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
    ) -> ThroughputMetrics {
        // If we're not running, exit after current threads finish.
        if !RUNNING.read().load(Ordering::SeqCst) {
            println!("Waiting for remaining threads to complete");
            self.wait_for_completion();
            exit(0);
        } else {
            match CLI_MODE.read().clone() {
                Mode::Sync(_) => {},
                _ => {
                    let max_records = 1000;
                    let mut pipeline_metadata: PipelineMetadata;
                    {
                        pipeline_metadata = METADATA.read().clone();
                    }

                    let mut count: u64 = 0;

                    for data in datas.iter() {
                        count += self.analyse_schema.infer_json_schema(
                            &mut data.data.clone(),
                            Some(max_records),
                            &mut pipeline_metadata.metadata,
                        );

                        {
                            *NUM_ANALYSED_RECORDS.write() += count;
                        }

                        if *NUM_ANALYSED_RECORDS.read() >= max_records {
                            break;
                        }
                    }

                    {
                        METADATA.write().metadata = pipeline_metadata.metadata.clone();
                    }

                    if *NUM_ANALYSED_RECORDS.read() >= max_records {
                        let mut pipeline_metadata = METADATA.read().clone();

                        if pipeline_metadata.metadata.len() == 0 {
                            println!("No data found in data source, skipping schema discovery");
                            std::process::exit(0);
                        }

                        let flatten = Config::truth_value(&Config::get_transform_config().flatten_events.unwrap_or("false".to_string()));

                        for (_namespace, metadata) in pipeline_metadata.metadata.iter_mut() {
                            AnalyseSchema::determine_field_types(&mut metadata.fields, None, flatten);
                        }

                        println!("Schema discovery complete, writing metadata to Skippr");

                        tokio::spawn(async move {
                            pipeline_metadata.enabled = true;
                            Config::set_metadata(&pipeline_metadata, false).await;
                            std::process::exit(0);
                        });
                    }

                    println!("Analysed schema for {} -> {}/{} records", count, *NUM_ANALYSED_RECORDS.read(), max_records);

                    return ThroughputMetrics {
                        bytes_per_second: self.get_current_throughput(),
                        active_cores: self.active_count.load(Ordering::Acquire),
                        queue_length: self.queue_length.load(Ordering::Acquire),
                        optimal_chunk_size: self.optimal_chunk_size.load(Ordering::Acquire),
                    };
                }
            }

            // Calculate total bytes in this batch
            let batch_bytes = datas.first().unwrap().bytes as u64;
            
            // Check if we need to wait before adding more to the queue
            let mut current_queue_length = self.queue_length.load(Ordering::Acquire);
            
            // Wait if queue is too full (but don't wait indefinitely)
            let mut wait_attempts = 0;
            while current_queue_length >= self.max_queue_length {
                std::thread::sleep(std::time::Duration::from_millis(200));
                wait_attempts += 1;
                
                // Check again after waiting
                current_queue_length = self.queue_length.load(Ordering::Acquire);
            }
            
            // Get current CPU utilization
            let active_threads = self.active_count.load(Ordering::SeqCst);
            
            // Try to process immediately if we have capacity or if we have no active threads
            if active_threads < self.num_cpus || active_threads == 0 {
                let tx = self.tx.clone();
                let offset_db_clone = offset_db.clone();
                let datas_clone = datas.clone();
                let mut schema_hashes = self.schema_hashes.clone();
                let handle = runtime::Handle::current();
                let shared_output_clone = shared_output.clone();

                // Increment active count and queue length before spawning
                self.active_count.fetch_add(1, Ordering::Acquire);
                self.queue_length.fetch_add(1, Ordering::Acquire);

                // Spawn the task and ensure it's executed
                self.thread_pool.execute(move || {
                    Ingest::process_batch(&datas_clone, &offset_db_clone, &mut schema_hashes, handle, shared_output_clone);
                    tx.send(0).unwrap();
                });
                
                // Update throughput metrics // not perfect as we're not waiting for the task to finish
                self.update_throughput(batch_bytes);

                
            } else {
                // Queue the task for later processing
                let _lock = self.queue_lock.write().unwrap();
                self.task_queue.write().unwrap().push_back(IngestTask {
                    datas: datas.clone(),
                    offset_db: offset_db.clone(),
                    shared_output: shared_output.clone()
                });
                
                // Increment queue length when adding to queue
                self.queue_length.fetch_add(1, Ordering::Acquire);

                // Update throughput metrics // not perfect as we're not waiting for the task to finish
                self.update_throughput(batch_bytes);
            }

            self.get_optmial_chunk_size();

            // Get current metrics for logging
            let current_queue_length = self.queue_length.load(Ordering::Acquire);
            let current_active_threads = self.active_count.load(Ordering::Acquire);
            let queued_tasks = self.task_queue.read().unwrap().len();

            println!("Queueing ingest task of {} ({} tasks in queue, {}/{} active threads, {} tasks waiting)",
                Helpers::human_readable_size(batch_bytes),
                current_queue_length,
                current_active_threads,
                self.num_cpus,
                queued_tasks
            );

            // Return throughput metrics
            ThroughputMetrics {
                bytes_per_second: self.get_current_throughput(),
                active_cores: current_active_threads,
                queue_length: current_queue_length,
                optimal_chunk_size: self.optimal_chunk_size.load(Ordering::Acquire),
            }
        }
    }

    pub(crate) fn deadletter(dl: Deadletter) {

        // let mut deadletter_file = DEADLETTER_FILE.write();
        //
        // let lines = serde_json::to_string(&dl).or(Err("Could not serialize deadletter")).unwrap();
        //
        // deadletter_file.write(lines.as_bytes()).or(Err("Could not write to deadletter file")).unwrap();
        // deadletter_file.write("\n".as_bytes()).or(Err("Could not write to deadletter file")).unwrap();
        //
        // deadletter_file.flush().or(Err("Could not flush deadletter file")).unwrap();

    }

    fn process_batch(
        datas: &Arc<Vec<IngestBatch>>,
        offset_db_clone: &Arc<Offsets>,
        schema_hashes: &mut DashMap<String, SchemaHash>,
        handle: runtime::Handle,
        shared_output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>,
    ) {

        // optional: enforce allowed partition values
        // This is thread local, a micro-optimization would be to move this to a global variable since it's not going to change and therefore no locking is required
        let allowed_values = Config::get_partition_allowed_values();

        PARTITION_ALLOWED_VALUES_CACHE.with(|cache| {
            cache.write().unwrap().extend(allowed_values.split(",").map(|v| {
                Helpers::clean_field_name(v.to_string())
            }).collect::<HashSet<String>>());
        });


        let _default_schema_hash = format!("{:?}", md5::compute(Helpers::random_str(10)));
        
        let flatten = Config::truth_value(&Config::get_transform_config().flatten_events.or(Some("no".to_string())).unwrap());

        let data_dir = Config::get_data_dir();
        let _output_dir = format!("{}/ingest_buffer", data_dir);
        let _deadletter_dir = format!("{}/deadletter_buffer", data_dir);

        let _aprox_now = SystemTime::now();
        
        let mut updated_schema= "no".to_string();

        let mut buffers = Buffers::new();

        let mut bytes: u64 = 0;
        let mut latest_timestamp: i64 = 0;
        let mut i: u64 = 0;
        let mut j = 0;
        let mut d = 0;
        let mut x = 0;
        let mut batch_line: u64 = 0;

        let format = match Config::get_pipline_plugin_config("input") {
            Ok(plugin) => plugin.format(),
            Err(_) => "json".to_string()
        };

        // @todo - check PluginConfig format is xml
        // if format == "xml" {
        //     let batch = IngestBatch {
        //         offset_key: datas[0].offset_key.clone(),
        //         data: datas.iter().map(|v| v.data.as_str()).collect::<Vec<&str>>().join(""),
        //     };
        //     datas.clear();
        //     datas.push(batch);
        // }

        let entity_field_dot = match Config::get_transform_config().record_field_path {
            Some(ref field) => field.clone(),
            None => "".to_string()
        };
        
        let mut buf: HashMap<(String, String, Option<i64>, String), IngestBufferBatch> = HashMap::with_capacity(32);

        for ingest_batch in datas.iter() {

            bytes += ingest_batch.data.len() as u64;
            
            let has_offsets =
                offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Closed, 0);
            let current_line_offset = offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Position, 0);

            let mut records: Vec<Value>;
            if format == "csv" {
                records = SerderCsv::deserialize(&ingest_batch.data);
            } else if format == "xml" {
                records = SerdeXml::deserialize(ingest_batch.data.as_bytes());
            } else {
                records = SerdeJson::deserialize(&ingest_batch.data.clone());
            }

            if !entity_field_dot.is_empty() {
                records = match Helpers::process_values(&records, &entity_field_dot) {
                    Some(records) => records,
                    None => Vec::new()
                };
            }

            batch_line = 0;

            let mut unwrapped_records: Vec<Value> = Vec::with_capacity(records.len());

            for record in records {
                
                match record.as_object() {
                    Some(_v) => unwrapped_records.push(record),
                    None => {
                        match record.as_array() {
                            Some(v) => {
                                for item in v {
                                    // println!("Item: {}", item);
                                    unwrapped_records.push(item.clone());
                                }
                            },
                            None => {
                                let line_no = if batch_line == 0 || batch_line > ingest_batch.data.lines().count() as u64 {
                                    1
                                } else {
                                    batch_line - 1
                                };

                                // deadletter
                                let line_str = match ingest_batch.data.lines().nth(line_no as usize) {
                                    Some(line) => line,
                                    None => ""
                                };

                                let dl = Deadletter {
                                    namespace: ingest_batch.offset_key.namespace.clone(),
                                    partition: ingest_batch.offset_key.partition.clone(),
                                    time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
                                    error: "Source data is not an object or array".to_string(),
                                    records: line_str.to_string(),
                                };

                                Self::deadletter(dl);
                                offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Position, batch_line);

                                d += 1;

                            }
                        }
                    }
                };

            }


            for record in unwrapped_records {

                batch_line += 1;

                if record.is_null()
                    || (record.is_object() && record.as_object().unwrap().is_empty())
                    || (record.is_array() && record.as_array().unwrap().is_empty())
                {

                    let line_str = match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                        Some(line) => line,
                        None => {
                            ""
                        }
                    };

                    let dl = Deadletter {
                        namespace: ingest_batch.offset_key.namespace.clone(),
                        partition: ingest_batch.offset_key.partition.clone(),
                        time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
                        error: "Source data is empty".to_string(),
                        records: line_str.to_string(),
                    };

                    Self::deadletter(dl);
                    offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Position, batch_line);

                    d += 1;

                    continue;
                }

                if has_offsets.is_none()
                    || current_line_offset.is_none()
                    || (Some(true) == has_offsets
                        && Some(true) == offset_db_clone.validate(&ingest_batch.offset_key, OffsetTypes::Position, batch_line))
                {

                    i += 1;

                    let mut namesapce_cache =  PARSE_NAMESPACE_CACHE.with(|cache| cache.read().unwrap().clone());
                    let skpr_namespace = Helpers::parse_namespace_field(
                        &record,
                        Config::get_pipeline_name(),
                        &mut namesapce_cache,
                    );

                    if namesapce_cache != PARSE_NAMESPACE_CACHE.with(|cache| cache.read().unwrap().clone()) {
                        PARSE_NAMESPACE_CACHE.with(|cache| cache.write().unwrap().clear());
                        PARSE_NAMESPACE_CACHE.with(|cache| cache.write().unwrap().extend(namesapce_cache));
                    }

                    let allowed_values = PARTITION_ALLOWED_VALUES_CACHE.with(|cache| cache.read().unwrap().clone());
                    let skpr_partition = Helpers::parse_partition_field(&record, allowed_values);
                    let skpr_time = Helpers::parse_time_field(&record);

                    let mut skpr_time_bucket: Option<i64> = None;

                    if skpr_time.is_some() {
                        skpr_time_bucket =
                            Some(BufferChunker::event_time_bucket(skpr_time.unwrap()));

                        if skpr_time.unwrap() > latest_timestamp {
                            latest_timestamp = skpr_time.unwrap();
                        }
                    }

                    if METADATA.read().metadata.get(&skpr_namespace).is_none() {
                        METADATA.write().metadata.insert(skpr_namespace.clone(), Metadata::new().unwrap());
                        println!("Discovered new namespace: {}", skpr_namespace);
                    }

                    let msg = match METADATA.read().metadata.get(&skpr_namespace) {
                        Some(metadata) => {
                            fast_path_ingest(
                                &record,
                                metadata.fields.as_ref(),
                                &skpr_namespace,
                                flatten,
                            )
                        }
                        None => {
                            Err(format!("Failed to find metadata for namespace: {}", skpr_namespace).into())
                        }
                    };

                    let record_value = match msg {
                        Ok(msg) => {
                            msg
                        },
                        Err(_err) => {

                            // println!("Falling back to slow path due to: {}", _err);

                            // hurrendous allocation, but we're handling an error case.
                            // It's important to not update METADATA mutex for other threads till we know the discovered schema is valid
                            // This may be called very often making troublesome data even worse
                            let mut metadata: PipelineMetadata;
                            {
                                metadata = METADATA.read().clone();
                            }

                            let msg = match ingest(
                                &record,
                                &mut metadata.metadata.get_mut(&skpr_namespace).unwrap().fields,
                                &skpr_namespace,
                                &mut updated_schema,
                                flatten,
                            ) {
                                Ok(msg) => msg,
                                Err(_err) => {

                                    updated_schema = "no".to_string();

                                    drop(metadata); // drop discovered schema to ensure we can accedentally use it

                                    // @todo - if we're going to log this, we should only do it when the schema was evovled for the deadlettered record
                                    if METRICS.read().deadletters_total == 0 {
                                        println!("Record deadlettered, schema evolution for deadletters will be ignored: {}", _err);
                                    }

                                    // deadletter record
                                    let line_str = match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                                        Some(line) => line,
                                        None => {
                                            // println!("Could not find line {} in batch", batch_line);
                                            ""
                                        }
                                    };

                                    let dl = Deadletter {
                                        namespace: ingest_batch.offset_key.namespace.clone(),
                                        partition: ingest_batch.offset_key.partition.clone(),
                                        time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
                                        error: _err.to_string(),
                                        records: line_str.to_string(),
                                    };

                                    Self::deadletter(dl);
                                    offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Position, batch_line);

                                    let mut counter_lock = METRICS.write();
                                    counter_lock.deadletters_total += 1;

                                    continue
                                }
                            };

                            
                            // update metadata in runtime and ingest message
                            if Config::get_auto_approve() {

                                if updated_schema.as_str() == "yes" {
                                    
                                    {
                                        METADATA.write().metadata = metadata.metadata.clone();
                                    }

                                    updated_schema = "no".to_string();

                                    handle.block_on(async {
                                        Config::set_metadata(&metadata, true).await;
                                    });

                                    println!("Updated schema for namespace: {}", skpr_namespace);
                                    
                                    let mut default_message = Value::Null;
                                    {
                                        default_message = create_default_nested_message(&metadata.metadata.get(&skpr_namespace).unwrap().fields);
                                    }

                                    {
                                        let mut lock = DEFAULT_NESTED_MESSAGE.write();
                                        lock.insert(skpr_namespace.clone(), default_message);
                                    }

                                    Ingest::prepare_arrow_schema_with_metadata(&skpr_namespace, &metadata.metadata, flatten).unwrap();

                                    {
                                        let schemas = ARROW_SCHEMA.read();

                                        let schema = Arc::clone(schemas.get(&skpr_namespace).unwrap());
                                        let hash = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));

                                        schema_hashes.insert(skpr_namespace.clone(), SchemaHash {
                                            schema,
                                            hash
                                        });
                                    }
                                }

                                x += 1;

                                msg

                            } else { // or just deadletter message for later approval
                                let line_str = match ingest_batch.data.lines().nth(batch_line as usize - 1) {
                                    Some(line) => line,
                                    None => {
                                        // println!("Could not find line {} in batch", batch_line);
                                        ""
                                    }
                                };

                                let dl = Deadletter {
                                    namespace: ingest_batch.offset_key.namespace.clone(),
                                    partition: ingest_batch.offset_key.partition.clone(),
                                    time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
                                    error: "Schema evolution is disabled".to_string(),
                                    records: line_str.to_string(),
                                };

                                Self::deadletter(dl);
                                offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Position, batch_line);
                                
                                d += 1;

                                continue;
                            }
                        }
                    };

                    let ingest_record = IngestRecord {
                        record: record_value,
                        _namespace: skpr_namespace.clone(),
                        _partition: skpr_partition.clone(),
                        _time: skpr_time_bucket.clone(),
                    };

                    let schema_hash = match schema_hashes.get(&skpr_namespace) {
                        Some(hash) => hash.clone(),
                        None => {
                            // Try a single read first to avoid unnecessary cloning
                            let schema_opt = {
                                ARROW_SCHEMA.read().get(&skpr_namespace).map(Arc::clone)
                            };
                            
                            if let Some(schema) = schema_opt {
                                // We found the schema, create and return the hash
                                let hash = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));
                                let schema_hash = SchemaHash {
                                    schema,
                                    hash
                                };
                                schema_hashes.insert(skpr_namespace.clone(), schema_hash.clone());
                                schema_hash
                            } else {
                                // Schema not found, we need to create it
                                println!("Schema not found for namespace: {}, creating it", skpr_namespace);
                                
                                // Create schema with a timeout to prevent deadlock
                                let start_time = Instant::now();
                                let timeout = Duration::from_secs(30); // 30 second timeout
                                let mut created = false;
                                
                                // Try to create the schema if needed
                                if METADATA.read().metadata.get(&skpr_namespace).is_some() {
                                    let metadata_clone = METADATA.read().clone();
                                    match Ingest::prepare_arrow_schema_with_metadata(&skpr_namespace, &metadata_clone.metadata, flatten) {
                                        Ok(_) => {
                                            created = true;
                                            println!("Created schema for namespace: {}", skpr_namespace);
                                        },
                                        Err(e) => {
                                            println!("Failed to create schema: {}", e);
                                        }
                                    }
                                }
                                
                                // Wait with timeout
                                let mut i = 0;
                                while !created && start_time.elapsed() < timeout {
                                    // Check if schema now exists
                                    if let Some(schema) = ARROW_SCHEMA.read().get(&skpr_namespace).map(Arc::clone) {
                                        let hash = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));
                                        let schema_hash = SchemaHash {
                                            schema,
                                            hash
                                        };
                                        schema_hashes.insert(skpr_namespace.clone(), schema_hash.clone());
                                        // return schema_hash;
                                    }
                                    
                                    if i == 0 || i % 100 == 0 {
                                        println!("Waiting for schema to be prepared for namespace: {} (timeout in {}s)", 
                                            skpr_namespace, 
                                            (timeout.as_secs() as f64 - start_time.elapsed().as_secs_f64()).max(0.0));
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                    i += 1;
                                }
                                
                                // If we timed out, create a default schema to unblock processing
                                if !created && start_time.elapsed() >= timeout {
                                    println!("Timed out waiting for schema, creating default schema for namespace: {}", skpr_namespace);
                                    
                                    // Create a default empty schema as fallback
                                    let schema = Arc::new(arrow::datatypes::Schema::empty());
                                    ARROW_SCHEMA.write().insert(skpr_namespace.to_string(), schema.clone());
                                    
                                    let hash = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));
                                    let schema_hash = SchemaHash {
                                        schema,
                                        hash
                                    };
                                    schema_hashes.insert(skpr_namespace.clone(), schema_hash.clone());
                                    schema_hash
                                } else {
                                    // We should have the schema by now
                                    let schema = Arc::clone(ARROW_SCHEMA.read().get(&skpr_namespace).unwrap());
                                    let hash = format!("{:?}", md5::compute(format!("{:?}", schema.deref())));
                                    
                                    let schema_hash = SchemaHash {
                                        schema,
                                        hash
                                    };
                                    
                                    schema_hashes.insert(skpr_namespace.clone(), schema_hash.clone());
                                    schema_hash
                                }
                            }
                        }
                    };

                    let buf_entry = buf.entry((
                        skpr_namespace.clone(),
                        skpr_partition.clone(),
                        skpr_time_bucket.clone(),
                        schema_hash.hash
                    )).or_insert_with(|| {

                        IngestBufferBatch {
                            offsets: HashMap::new(),
                            _namespace: skpr_namespace.clone(),
                            _partition: skpr_partition.clone(),
                            _time: skpr_time_bucket,
                            _shard: "".to_string(),
                            records: vec![ingest_record.clone()],
                            schema: schema_hash.schema,
                        }
                    });
                    
                    // an ingest batch consist of many small files/queue messages, etc. Each will need its offset committed in the WAL.
                    buf_entry.offsets.insert(ingest_batch.offset_key.clone(), batch_line);
                    
                    buf_entry.records.push(ingest_record);

                    j += 1;
                    
                }
            }

            // may have deadlettered some records, so we need to update the offset since they won't be in the WAL
            // offset_db_clone.insert(&ingest_batch.offset_key, OffsetTypes::Closed, 1);

        }
        
        buffers.write(buf);

        let offset_db_clone = offset_db_clone.clone();
        let shared_output_clone = shared_output.clone();
        
        // Use block_on safely with proper error handling
        match handle.block_on(async {
            match buffers.flush(offset_db_clone.clone(), shared_output_clone.clone()).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    println!("Error flushing buffers: {}", e);
                    Err(e)
                }
            }
        }) {
            Ok(_) => {
                // Success, continue
            },
            Err(e) => {
                println!("Failed to execute in Tokio runtime: {:?}", e);
                // Handle the error gracefully, maybe create a new runtime
                match tokio::runtime::Runtime::new() {
                    Ok(rt) => {
                        // Try again with a new runtime
                        if let Err(e) = rt.block_on(buffers.flush(offset_db_clone, shared_output_clone)) {
                            println!("Failed to flush buffers with new runtime: {:?}", e);
                        }
                    },
                    Err(e) => {
                        println!("Failed to create runtime: {:?}", e);
                    }
                }
            }
        }
        
        // Update metrics safely, without panicking
        let update_result = std::panic::catch_unwind(|| {
            let mut counter_lock = METRICS.write();
            counter_lock.deadletters_total += d;
            counter_lock.ingeted_slow_total += x;
            counter_lock.messages_total += i;
            counter_lock.source_bytes_total += bytes;

            if latest_timestamp as u64 > counter_lock.latest_timestamp {
                counter_lock.latest_timestamp = latest_timestamp as u64;
            }
        });
        
        if let Err(e) = update_result {
            println!("Warning: Could not update metrics - {:?}", e);
        }
    }

    pub(crate) fn prepare_arrow_schema_with_metadata(
        skpr_namespace: &str,
        metadata: &HashMap<String, Metadata>,
        flatten: bool,
    ) -> Result<Arc<arrow::datatypes::Schema>, ArrowError> {
        let mut _arrow_schema: Result<datatypes::Schema, ArrowError> = Ok(datatypes::Schema::empty());
        let mut _schema_ref = Arc::new(datatypes::Schema::empty());

        let skpr_metadata = metadata.get(skpr_namespace);

        let mut output_metadata: OutputMetadata = OutputMetadata::new();

        if let Some(metadata_for_namespace) = skpr_metadata {
            if flatten {
                output_metadata = OutputMetadata::from_flatterened_metadata(metadata_for_namespace);
            } else {
                output_metadata = OutputMetadata::from_metadata(metadata_for_namespace);
            }
        }
        
        _arrow_schema = convert_skippr_to_arrow(
            output_metadata.fields,
        );

        _schema_ref = Arc::new(_arrow_schema.unwrap());

        ARROW_SCHEMA.write().insert(skpr_namespace.to_string(), _schema_ref.clone());

        Ok(_schema_ref)
    }
    
}


