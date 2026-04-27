#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::{FileReader, SerializedFileReader};
use walkdir::WalkDir;

pub const TEST_WORKSPACE: &str = "batch-tests";

fn skipprd_bin() -> PathBuf {
    std::env::var_os("SKIPPR_E2E_SKIPPRD_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_skipprd")))
}

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/formats")
        .join(name)
}

pub struct BatchE2eHarness {
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub pipeline_name: String,
}

impl BatchE2eHarness {
    pub fn new(pipeline_name: &str, config_yaml: &str) -> Self {
        let id: u64 = rand::random();
        let data_dir = std::env::temp_dir().join(format!("skippr_batch_e2e_{}", id));
        std::fs::create_dir_all(&data_dir).unwrap();

        let config_path = data_dir.join("skipprd.yml");
        std::fs::write(&config_path, config_yaml).unwrap();

        Self {
            config_path,
            data_dir,
            pipeline_name: pipeline_name.to_string(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn spawn_sync(&self) -> Child {
        let bin = skipprd_bin();
        Command::new(&bin)
            .args(["sync", "--pipeline", &self.pipeline_name])
            .env("SKIPPR_CONFIG_FILE", &self.config_path)
            .env("DATA_DIR", &self.data_dir)
            .env("AWS_ACCESS_KEY_ID", "test")
            .env("AWS_SECRET_ACCESS_KEY", "test")
            .env("AWS_DEFAULT_REGION", "us-east-1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {:?}: {}", bin, e))
    }

    pub fn run_sync(&self, timeout: Duration) -> Output {
        let mut child = self.spawn_sync();
        let deadline = Instant::now() + timeout;

        loop {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    return child.wait_with_output().expect("failed to collect output");
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let output = child.wait_with_output().expect("failed to collect output");
                    panic!(
                        "skipprd sync timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                        timeout,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Err(err) => panic!("failed to poll child process: {}", err),
            }
        }
    }

    pub fn run_sync_with_timeout(&self, timeout: Duration) -> Output {
        let mut child = self.spawn_sync();
        std::thread::sleep(timeout);
        let _ = child.kill();
        child.wait_with_output().expect("failed to collect output")
    }
}

impl Drop for BatchE2eHarness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "skipprd sync failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn assert_no_main_panic(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("thread 'main' panicked"),
        "skipprd panicked\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
}

pub fn parquet_row_count_in_dir(dir: &Path) -> i64 {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("parquet"))
        .map(|path| {
            let file = std::fs::File::open(&path)
                .unwrap_or_else(|e| panic!("failed to open parquet file {:?}: {}", path, e));
            let reader =
                SerializedFileReader::new(file).unwrap_or_else(|e| panic!("bad parquet: {}", e));
            reader.metadata().file_metadata().num_rows()
        })
        .sum()
}

pub fn parquet_column_names_in_dir(dir: &Path) -> Vec<String> {
    let mut names = BTreeSet::new();
    for path in WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("parquet"))
    {
        let file = std::fs::File::open(&path)
            .unwrap_or_else(|e| panic!("failed to open parquet file {:?}: {}", path, e));
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap_or_else(|e| panic!("bad parquet reader for {:?}: {}", path, e));
        for field in builder.schema().fields() {
            names.insert(field.name().to_string());
        }
    }
    names.into_iter().collect()
}

pub fn output_buffer_dir(harness: &BatchE2eHarness) -> PathBuf {
    harness
        .data_dir()
        .join(format!("{}_{}", TEST_WORKSPACE, harness.pipeline_name))
        .join("output_buffer")
}
