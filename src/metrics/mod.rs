#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metrics {
    Total,
    Globbing,
    WaitingForFiles,
    Ingesting,
    CreateS3Client,
    AsyncGets,
    AsyncGetObject,
    Await,
}

impl Metrics {
    pub fn as_str(&self) -> &'static str {
        match self {
            Metrics::Total => "Total",
            Metrics::Globbing => "Globbing files",
            Metrics::WaitingForFiles => "Waiting for files",
            Metrics::Ingesting => "Ingesting",
            Metrics::CreateS3Client => "Create S3 client",
            Metrics::AsyncGets => "Async gets",
            Metrics::AsyncGetObject => "Async get s3 object",
            Metrics::Await => "Await thread",
        }
    }
}
