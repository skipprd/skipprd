#![allow(dead_code)]

use std::collections::BTreeSet;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::{FileReader, SerializedFileReader};
use walkdir::WalkDir;

use crate::support::cdc_e2e::skippr_el_bin;

pub const LOCALSTACK_PORT: u16 = 14566;
pub const POSTGRES_PORT: u16 = 15432;
pub const SFTP_PORT: u16 = 12222;
pub const AMQP_CONNECTION_STRING: &str = "amqp://guest:guest@127.0.0.1:15672";
pub const TEST_WORKSPACE: &str = "batch-tests";

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

        let config_path = data_dir.join("skippr-el.yml");
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
        let bin = skippr_el_bin();
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
                        "skippr-el sync timed out after {:?}\nstderr:\n{}",
                        timeout,
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
        "skippr-el sync failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn assert_no_main_panic(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("thread 'main' panicked"),
        "skippr-el panicked\nstdout:\n{}\nstderr:\n{}",
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

pub fn localstack_s3_client(port: u16) -> S3Client {
    let config = aws_sdk_s3::Config::builder()
        .endpoint_url(format!("http://127.0.0.1:{}", port))
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("test", "test", None, None, "static"))
        .force_path_style(true)
        .behavior_version_latest()
        .build();
    S3Client::from_conf(config)
}

pub async fn ensure_s3_bucket(client: &S3Client, bucket: &str) {
    let _ = client.create_bucket().bucket(bucket).send().await;
}

pub async fn put_s3_object(client: &S3Client, bucket: &str, key: &str, body: &str) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(body.as_bytes().to_vec()))
        .send()
        .await
        .unwrap();
}

pub async fn list_s3_keys(client: &S3Client, bucket: &str, prefix: &str) -> Vec<String> {
    client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .send()
        .await
        .unwrap()
        .contents
        .unwrap_or_default()
        .into_iter()
        .filter_map(|obj| obj.key().map(|key| key.to_string()))
        .collect()
}

pub async fn get_s3_object_bytes(client: &S3Client, bucket: &str, key: &str) -> Vec<u8> {
    client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes()
        .to_vec()
}

pub fn sftp_write_file(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    remote_path: &str,
    contents: &str,
) {
    use ssh2::Session;

    let tcp = TcpStream::connect(format!("{}:{}", host, port)).unwrap();
    let mut session = Session::new().unwrap();
    session.set_tcp_stream(tcp);
    session.handshake().unwrap();
    session.userauth_password(username, password).unwrap();

    let sftp = session.sftp().unwrap();
    let mut file = sftp.create(Path::new(remote_path)).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

pub async fn prepare_amqp_queue(
    connection_string: &str,
    exchange: &str,
    routing_key: &str,
    queue: &str,
) {
    use lapin::options::{
        ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions, QueuePurgeOptions,
    };
    use lapin::types::FieldTable;
    use lapin::{Connection, ConnectionProperties, ExchangeKind};

    let conn = Connection::connect(connection_string, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = conn.create_channel().await.unwrap();

    channel
        .exchange_declare(
            exchange,
            ExchangeKind::Direct,
            ExchangeDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    channel
        .queue_declare(
            queue,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    channel
        .queue_purge(queue, QueuePurgeOptions::default())
        .await
        .unwrap();
    channel
        .queue_bind(
            queue,
            exchange,
            routing_key,
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();
}

pub async fn collect_amqp_messages(
    connection_string: &str,
    queue: &str,
    expected_count: usize,
    timeout: Duration,
) -> Vec<String> {
    use futures_util::StreamExt;
    use lapin::options::{BasicAckOptions, BasicConsumeOptions};
    use lapin::types::FieldTable;
    use lapin::{Connection, ConnectionProperties};

    let conn = Connection::connect(connection_string, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = conn.create_channel().await.unwrap();
    let consumer_tag = format!("skippr-batch-test-{}", rand::random::<u64>());
    let mut consumer = channel
        .basic_consume(
            queue,
            &consumer_tag,
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();

    let deadline = Instant::now() + timeout;
    let mut messages = Vec::new();

    while messages.len() < expected_count && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), consumer.next()).await {
            Ok(Some(Ok(delivery))) => {
                messages.push(String::from_utf8_lossy(&delivery.data).into_owned());
                delivery.ack(BasicAckOptions::default()).await.unwrap();
            }
            Ok(Some(Err(err))) => panic!("amqp consume failed: {}", err),
            Ok(None) => break,
            Err(_) => {}
        }
    }

    assert_eq!(
        messages.len(),
        expected_count,
        "expected {} AMQP messages, got {}",
        expected_count,
        messages.len()
    );
    messages
}
