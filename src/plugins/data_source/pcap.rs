use std::sync::Arc;

use async_trait::async_trait;

use crate::helpers::offsets::Offsets;
use crate::plugins::{DataSink, DataSource};

pub struct DataSourcePcapPlugin;

impl DataSourcePcapPlugin {
    pub async fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DataSource for DataSourcePcapPlugin {
    async fn sync(
        &mut self,
        _offsets: Arc<Offsets>,
        _output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        Err(std::io::Error::other(
            "pcap support not compiled -- enable the pcap feature",
        ))
    }
}
