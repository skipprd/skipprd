#[derive(Clone, Debug)]
pub struct LanceWriteOptions {
    pub dimension: Option<usize>,
    pub s3_uri: Option<String>,
}

impl Default for LanceWriteOptions {
    fn default() -> Self {
        Self {
            dimension: None,
            s3_uri: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LanceRecord<'a> {
    pub namespace: &'a str,
    pub source_uri: &'a str,
    pub chunk_id: &'a str,
    pub text: &'a str,
    pub vector: &'a [f32],
}

/// Stub Lance writer/reader. Wire to `lance` crate in implementation.
pub struct LanceStore {}

impl LanceStore {
    pub fn new() -> Self {
        Self {}
    }

    pub fn write(
        &self,
        _namespace: &str,
        _records: &[LanceRecord],
        _opts: &LanceWriteOptions,
    ) -> Result<(), String> {
        Ok(())
    }

    pub fn knn(
        &self,
        _namespace: &str,
        _query: &[f32],
        _k: usize,
    ) -> Result<Vec<(String, f32)>, String> {
        Ok(Vec::new())
    }
}
