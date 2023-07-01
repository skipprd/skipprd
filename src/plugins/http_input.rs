use crate::helpers::configuration::{Config, Metrics};
use std::collections::HashMap;
use std::io::{Cursor, Read, BufReader};
use std::fs::File;
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use std::thread::sleep;
use crate::discover::Metadata;
use futures::future::join_all;
use futures::{AsyncReadExt, StreamExt};
use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch};
use reqwest::Client;
use tar::Archive;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use crate::{INPUT_GRACEFUL_SHUTDOWN_COMPLETE, RUNNING};
use std::path::Path;
use flate2::read::GzDecoder;

pub struct DataSourceHttpPlugin {
    client: Client,
    ingest: Ingest,
    source_url: String,
    temp_dir: String,
}

impl DataSourceHttpPlugin {
    pub async fn new(source_url: String, temp_dir: String) -> DataSourceHttpPlugin {
        let client = Client::new();
        DataSourceHttpPlugin {
            client,
            ingest: Ingest::new(),
            source_url,
            temp_dir,
        }
    }

    pub async fn download_and_ingest(
        &self,
        metadata: Arc<Mutex<HashMap<String, Metadata>>>,
        metrics: Arc<Mutex<Metrics>>,
        offsets: Arc<Offsets>,
    ) {
        // Download the file
        let response = self.client.get(&self.source_url).send().await.unwrap();
        let bytes = response.bytes().await.unwrap();

        // Save to a temp file
        let temp_file_path = Path::new(&self.temp_dir).join("temp");
        fs::write(&temp_file_path, bytes).unwrap();

        let offset_key = OffsetKey {
            namespace: self.source_url.clone(),
            partition: "".to_string(),
        };

        let data = if self.source_url.ends_with(".gz") {
            let file = File::open(&temp_file_path).unwrap();
            let buf_rdr = BufReader::new(file);
            let gz_decoder = GzDecoder::new(buf_rdr);
            let mut archive = Archive::new(gz_decoder);
            let mut data = String::new();
            for entry_result in archive.entries().unwrap() {
                let mut entry = entry_result.unwrap();
                entry.read_to_string(&mut data).unwrap();
            }
            data
        } else {
            fs::read_to_string(&temp_file_path).unwrap()
        };

        let batch = IngestBatch {
            offset_key,
            data,
        };

        self.ingest.ingest_file(
            vec![batch],
            &metadata,
            &metrics,
            &offsets,
        );
    }
}
