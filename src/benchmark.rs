use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use crate::helpers::configuration::Config;

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
    pub fn new(
        config: &Config,
        num_files: usize,
        records_per_file: usize,
        avg_record_size_bytes: usize,
    ) -> Self {
        let data_dir = config.get_data_dir();
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
        let _ = (&self.data_dir, &self.temp_dir, name, description);
        Err(std::io::Error::other(
            "benchmark mode requires standalone runtime plugin packages; the host binary no longer embeds file source/sink implementations",
        ))
    }
}
