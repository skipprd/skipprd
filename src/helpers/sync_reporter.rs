use chrono::Utc;
use serde::Serialize;
use std::io::Write;

use super::progress::ProgressUi;

#[derive(Serialize)]
struct SyncEvent {
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pipeline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    field_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fields_added: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows_written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parquet_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespaces_synced: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespaces_discovered: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_fields: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uploads_in_flight: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
    timestamp: String,
}

impl SyncEvent {
    fn new(event: &str) -> Self {
        SyncEvent {
            event: event.to_string(),
            pipeline: None,
            namespace: None,
            field_count: None,
            fields_added: None,
            rows: None,
            bytes: None,
            rows_written: None,
            parquet_file: None,
            namespaces_synced: None,
            namespaces_discovered: None,
            total_fields: None,
            total_rows: None,
            elapsed_ms: None,
            error: None,
            uploads_in_flight: None,
            ok: None,
            timestamp: Utc::now().to_rfc3339(),
        }
    }
}

pub enum SyncReporter {
    Progress(ProgressUi),
    Json,
    Text,
}

impl SyncReporter {
    pub fn new(mode: &str, tty: bool, logs_enabled: bool) -> Self {
        match mode {
            "json" => SyncReporter::Json,
            "text" => SyncReporter::Text,
            _ => SyncReporter::Progress(ProgressUi::new(tty && !logs_enabled)),
        }
    }

    pub fn enabled(&self) -> bool {
        match self {
            SyncReporter::Progress(p) => p.enabled(),
            SyncReporter::Json => true,
            SyncReporter::Text => true,
        }
    }

    pub fn add_tasks(&self, tasks: &[&str]) {
        if let SyncReporter::Progress(p) = self {
            p.add_tasks(tasks);
        }
    }

    pub fn start(&self, name: &str) {
        match self {
            SyncReporter::Progress(p) => p.start(name),
            SyncReporter::Text => println!("[start] {}", name),
            SyncReporter::Json => {}
        }
    }

    pub fn complete(&self, name: &str) {
        match self {
            SyncReporter::Progress(p) => p.complete(name),
            SyncReporter::Text => println!("[done]  {}", name),
            SyncReporter::Json => {}
        }
    }

    pub fn finish(&self) {
        if let SyncReporter::Progress(p) = self {
            p.finish();
        }
    }

    pub fn sync_start(&self, pipeline: &str) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("sync_start");
                ev.pipeline = Some(pipeline.to_string());
                emit_json(&ev);
            }
            SyncReporter::Text => println!("Sync started: pipeline={}", pipeline),
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn namespace_discovered(&self, namespace: &str, field_count: usize) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("namespace_discovered");
                ev.namespace = Some(namespace.to_string());
                ev.field_count = Some(field_count);
                emit_json(&ev);
            }
            SyncReporter::Text => {
                println!(
                    "Namespace discovered: {} ({} fields)",
                    namespace, field_count
                );
            }
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn schema_evolved(&self, namespace: &str, fields_added: Vec<String>) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("schema_evolved");
                ev.namespace = Some(namespace.to_string());
                ev.fields_added = Some(fields_added);
                emit_json(&ev);
            }
            SyncReporter::Text => {
                println!("Schema evolved: {}", namespace);
            }
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn batch_ingested(&self, namespace: &str, rows: u64, bytes: u64) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("batch_ingested");
                ev.namespace = Some(namespace.to_string());
                ev.rows = Some(rows);
                ev.bytes = Some(bytes);
                emit_json(&ev);
            }
            SyncReporter::Text => {
                println!(
                    "Batch ingested: {} rows={} bytes={}",
                    namespace, rows, bytes
                );
            }
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn compaction_complete(&self, namespace: &str, parquet_file: &str) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("compaction_complete");
                ev.namespace = Some(namespace.to_string());
                ev.parquet_file = Some(parquet_file.to_string());
                emit_json(&ev);
            }
            SyncReporter::Text => {
                println!("Compaction complete: {}", namespace);
            }
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn output_synced(&self, namespace: &str, rows_written: u64) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("output_synced");
                ev.namespace = Some(namespace.to_string());
                ev.rows_written = Some(rows_written);
                emit_json(&ev);
            }
            SyncReporter::Text => {
                println!("Output synced: {} rows_written={}", namespace, rows_written);
            }
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn sync_complete(
        &self,
        pipeline: &str,
        namespaces_synced: usize,
        total_rows: u64,
        elapsed_ms: u64,
    ) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("sync_complete");
                ev.pipeline = Some(pipeline.to_string());
                ev.namespaces_synced = Some(namespaces_synced);
                ev.total_rows = Some(total_rows);
                ev.elapsed_ms = Some(elapsed_ms);
                emit_json(&ev);
            }
            SyncReporter::Text => {
                println!(
                    "Sync complete: pipeline={} namespaces={} rows={} elapsed={}ms",
                    pipeline, namespaces_synced, total_rows, elapsed_ms
                );
            }
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn discover_start(&self, pipeline: &str) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("discover_start");
                ev.pipeline = Some(pipeline.to_string());
                emit_json(&ev);
            }
            SyncReporter::Text => println!("Discover started: pipeline={}", pipeline),
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn discover_complete(
        &self,
        pipeline: &str,
        namespaces_discovered: usize,
        total_fields: u64,
        elapsed_ms: u64,
    ) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("discover_complete");
                ev.pipeline = Some(pipeline.to_string());
                ev.ok = Some(true);
                ev.namespaces_discovered = Some(namespaces_discovered);
                ev.total_fields = Some(total_fields);
                ev.elapsed_ms = Some(elapsed_ms);
                emit_json(&ev);
            }
            SyncReporter::Text => {
                println!(
                    "Discover complete: pipeline={} namespaces={} fields={} elapsed={}ms",
                    pipeline, namespaces_discovered, total_fields, elapsed_ms
                );
            }
            SyncReporter::Progress(_) => {}
        }
    }

    pub fn sync_status(
        &self,
        pipeline: &str,
        messages_total: u64,
        bytes_total: u64,
        rows_written: u64,
        elapsed_ms: u64,
        uploads_in_flight: u64,
    ) {
        if let SyncReporter::Json = self {
            let mut ev = SyncEvent::new("sync_status");
            ev.pipeline = Some(pipeline.to_string());
            ev.total_rows = Some(messages_total);
            ev.bytes = Some(bytes_total);
            ev.rows_written = Some(rows_written);
            ev.elapsed_ms = Some(elapsed_ms);
            ev.uploads_in_flight = Some(uploads_in_flight);
            emit_json(&ev);
        }
    }

    pub fn sync_error(&self, pipeline: &str, error: &str, namespace: Option<&str>) {
        match self {
            SyncReporter::Json => {
                let mut ev = SyncEvent::new("sync_error");
                ev.pipeline = Some(pipeline.to_string());
                ev.error = Some(error.to_string());
                ev.namespace = namespace.map(|s| s.to_string());
                emit_json(&ev);
            }
            SyncReporter::Text => {
                println!("Sync error: pipeline={} error={}", pipeline, error);
            }
            SyncReporter::Progress(_) => {}
        }
    }
}

fn emit_json(event: &SyncEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{}", json);
        let _ = stdout.flush();
    }
}
