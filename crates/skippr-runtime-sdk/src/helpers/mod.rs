pub mod configuration;
pub mod logging;
pub mod names;
pub mod offsets;
pub mod plugin_config;

use rand::seq::SliceRandom;
use rand::Rng;

pub struct Helpers;

impl Helpers {
    pub fn clean_field_name(_config: &configuration::Config, field: String) -> String {
        names::clean_field_name(&field, false)
    }

    pub fn random_str(length: usize) -> String {
        let mut rng = rand::thread_rng();
        let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().collect();
        let mut pass = String::with_capacity(length + 16);
        for _ in 0..length {
            pass.push(*alphabet.choose(&mut rng).unwrap());
        }
        pass.push_str(&rng.gen::<u64>().to_string());
        pass
    }

    pub fn human_readable_size(bytes: u64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
        let mut size = bytes as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit < units.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }
        format!("{:.1} {}", size, units[unit])
    }
}
