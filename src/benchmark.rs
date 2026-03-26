use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::plugins::file_input::DataSourceLocalFilePlugin;
use crate::plugins::file_output::DataOutputFilePlugin;

/// Represents the results of a performance benchmark
pub struct BenchmarkResults {
    pub name: String,
    pub description: String,
    pub duration_ms: u128,
    pub throughput_records_per_sec: f64,
    pub throughput_mb_per_sec: f64,
    pub records_processed: u64,
    pub bytes_processed: u64,
    pub heap_memory_usage_mb: f64,
    pub timestamp: String,
}

impl BenchmarkResults {
    pub fn to_csv_line(&self) -> String {
        format!(
            "{},\"{}\",{},{:.2},{:.2},{},{},{:.2},{}",
            self.name,
            self.description,
            self.duration_ms,
            self.throughput_records_per_sec,
            self.throughput_mb_per_sec,
            self.records_processed,
            self.bytes_processed,
            self.heap_memory_usage_mb,
            self.timestamp
        )
    }

    pub fn csv_header() -> &'static str {
        "name,description,duration_ms,records_per_sec,mb_per_sec,records_processed,bytes_processed,heap_memory_mb,timestamp"
    }
}

/// Utility to generate test data and benchmark the performance of the system
pub struct PerformanceBenchmark {
    pub data_dir: String,
    pub temp_dir: String,
    pub num_files: usize,
    pub records_per_file: usize,
    pub avg_record_size_bytes: usize,
}

impl PerformanceBenchmark {
    pub fn new(num_files: usize, records_per_file: usize, avg_record_size_bytes: usize) -> Self {
        let data_dir = Config::get_data_dir();
        let temp_dir = format!("{}/benchmark", data_dir);

        PerformanceBenchmark {
            data_dir,
            temp_dir,
            num_files,
            records_per_file,
            avg_record_size_bytes,
        }
    }

    /// Generate random JSON records for benchmarking
    fn generate_random_json_record(&self, id: usize) -> String {
        // Generate a random JSON document with predictable size
        let base_record = format!(
            r#"{{
                "id": {},
                "timestamp": {},
                "name": "{}",
                "value": {},
                "is_active": {},
                "nested": {{
                    "field1": "{}",
                    "field2": {},
                    "array": [{{"item": "value1"}}, {{"item": "value2"}}]
                }},
                "tags": ["tag1", "tag2", "tag3"],
                "data": "{}"
            }}"#,
            id,
            chrono::Utc::now().timestamp(),
            format!("test-{}", id % 100),
            id as f64 * 1.5,
            id % 2 == 0,
            format!("nested-{}", id % 50),
            id % 10,
            // Generate a string of the right size to achieve our avg_record_size_bytes
            "x".repeat(
                self.avg_record_size_bytes
                    .saturating_sub(300) // 300 is approx size of the base record
                    .max(0) // Ensure it's not negative
            )
        );
        base_record
    }

    /// Create benchmark data files
    pub fn create_benchmark_data(&self) -> std::io::Result<u64> {
        // Create benchmark directory
        if Path::new(&self.temp_dir).exists() {
            fs::remove_dir_all(&self.temp_dir)?;
        }
        fs::create_dir_all(&self.temp_dir)?;

        let mut total_bytes = 0;

        // Create benchmark files
        for i in 0..self.num_files {
            let file_path = format!("{}/test_file_{}.json", self.temp_dir, i);
            let mut file = File::create(&file_path)?;

            // Write random records to file
            for j in 0..self.records_per_file {
                let record = self.generate_random_json_record(i * self.records_per_file + j);
                writeln!(file, "{}", record)?;
                total_bytes += record.len() as u64 + 1; // +1 for newline
            }
        }

        Ok(total_bytes)
    }

    /// Run the performance benchmark
    pub async fn run_benchmark(
        &self,
        name: &str,
        description: &str,
    ) -> std::io::Result<BenchmarkResults> {
        // Setup output directory
        let output_dir = format!("{}/benchmark_output", self.data_dir);
        if Path::new(&output_dir).exists() {
            fs::remove_dir_all(&output_dir)?;
        }
        fs::create_dir_all(&output_dir)?;

        // Configure the environment for local benchmark
        Config::setenv("DATA_SOURCE_PLUGIN_NAME", "File");
        Config::setenv("DATA_SOURCE_PATH", &self.temp_dir);
        Config::setenv("DATA_OUTPUT_PATH", &output_dir);

        // Initialize components
        let offsets = Arc::new(Offsets::init().expect("Failed to initialize offsets"));
        let mut input_plugin = DataSourceLocalFilePlugin::new().await;
        let output_plugin = DataOutputFilePlugin::new("output".to_string()).await;
        let boxed_output_plugin: Box<dyn crate::plugins::DataSink + Send + Sync> =
            Box::new(output_plugin);
        let arc_output_plugin = Arc::new(boxed_output_plugin);

        // Set up memory measurement
        #[allow(unused_mut)]
        let mut initial_memory = 0.0_f64;
        #[allow(unused_mut)]
        let mut peak_memory = 0.0_f64;

        #[cfg(target_os = "linux")]
        {
            use std::fs::read_to_string;
            let proc_statm = read_to_string("/proc/self/statm").unwrap_or_default();
            let parts: Vec<&str> = proc_statm.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(heap_pages) = parts[1].parse::<u64>() {
                    // Convert pages to MB (typically 4KB pages)
                    initial_memory = (heap_pages * 4096) as f64 / 1_048_576.0;
                }
            }
        }

        // Run the benchmark
        let start_time = Instant::now();

        // Process the files
        input_plugin.sync(offsets.clone(), arc_output_plugin).await;

        let elapsed = start_time.elapsed();

        // Measure memory usage
        #[cfg(target_os = "linux")]
        {
            use std::fs::read_to_string;
            let proc_statm = read_to_string("/proc/self/statm").unwrap_or_default();
            let parts: Vec<&str> = proc_statm.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(heap_pages) = parts[1].parse::<u64>() {
                    // Convert pages to MB (typically 4KB pages)
                    peak_memory = (heap_pages * 4096) as f64 / 1_048_576.0;
                }
            }
        }

        // Get metrics
        let total_records = self.num_files as u64 * self.records_per_file as u64;
        let total_bytes = self.num_files as u64
            * self.records_per_file as u64
            * self.avg_record_size_bytes as u64;

        let duration_ms = elapsed.as_millis();
        let throughput_records_per_sec = total_records as f64 / (elapsed.as_secs_f64().max(0.001));
        let throughput_mb_per_sec =
            (total_bytes as f64 / (1024.0 * 1024.0)) / (elapsed.as_secs_f64().max(0.001));

        let heap_memory_usage_mb = peak_memory - initial_memory;

        let results = BenchmarkResults {
            name: name.to_string(),
            description: description.to_string(),
            duration_ms,
            throughput_records_per_sec,
            throughput_mb_per_sec,
            records_processed: total_records,
            bytes_processed: total_bytes,
            heap_memory_usage_mb,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        // Output results to file
        let results_file = format!("{}/benchmark_results.csv", self.data_dir);
        let file_exists = Path::new(&results_file).exists();

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&results_file)?;

        if !file_exists {
            writeln!(file, "{}", BenchmarkResults::csv_header())?;
        }

        writeln!(file, "{}", results.to_csv_line())?;

        println!("Benchmark '{}' completed:", name);
        println!("Description: {}", description);
        println!("Duration: {}ms", results.duration_ms);
        println!(
            "Throughput: {:.2} records/sec, {:.2} MB/sec",
            results.throughput_records_per_sec, results.throughput_mb_per_sec
        );
        println!("Memory usage: {:.2} MB", results.heap_memory_usage_mb);
        println!("Results saved to: {}", results_file);

        Ok(results)
    }
}
