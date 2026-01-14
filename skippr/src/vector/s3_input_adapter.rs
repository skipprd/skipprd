#[derive(Clone, Debug)]
pub struct S3ObjectBatch {
    pub keys: Vec<String>,
}

/// Placeholder for adapting existing s3_input plugin to batches for vector indexing.
pub struct S3InputAdapter {}

impl S3InputAdapter {
    pub fn new() -> Self { Self {} }
    pub fn next_batch(&self) -> Option<S3ObjectBatch> { None }
}


