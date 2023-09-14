// use std::collections::HashMap;
// use std::sync::Arc;
// use async_trait::async_trait;
// use crate::helpers::offsets::Offsets;

pub mod athena;
pub mod stdin_input;
pub mod s3_input;
pub mod s3_inventory;
pub mod file_input;
pub mod s3_output;
pub mod file_output;
pub mod stdout_output;
pub mod pcap_input;

// #[async_trait]
// trait DataSourcePlugin {
//     async fn new() -> Self where Self: Sized;
//     async fn sync(&mut self, offsets: Arc<Offsets>);
// }
//
// #[async_trait]
// trait DataOutputPlugin {
//     async fn new() -> Self where Self: Sized;
//     async fn sync(&mut self, offsets: Arc<Offsets>);
// }

// pub fn init_input_plugin(
//     plugin_name: &str,
//     config: &HashMap<String, String>,
// ) -> Box<dyn DataSourcePlugin> {
//     match plugin_name {
//         "stdin" => stdin_input::DataSourceStdinPlugin::new(),
        // "s3" => Box::new(s3_input::DataSourceS3Plugin::new()),
        // "s3_inventory" => Box::new(s3_inventory::DataSourceS3InventoryPlugin::new()),
//         "file" => Box::new(file_input::DataSourceLocalFilePlugin::new()),
//         _ => panic!("Unknown input plugin: {}", plugin_name),
//     }
// }

