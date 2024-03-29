use chrono::{Datelike, DateTime, FixedOffset, Timelike};
use std::io;
use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;

const GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];

pub struct TimePartitioner {
    filename: String,
}

impl TimePartitioner {
    pub fn new(filename: &String) -> Self {
        TimePartitioner { filename: filename.clone() }
    }

    pub fn process(&self) -> Result<String, io::Error> {
        let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(&self.filename);

        if time_partition_str.is_empty() {
            return Err(io::Error::new(io::ErrorKind::Other, "Time partition string is empty"));
        }

        let granularity_target = Config::get_transform_batch_time_unit();
        let date = self.parse_datetime(&time_partition_str)?;

        let mut full_key = String::new();
        for granularity in GRANULARITIES.iter() {
            let foo = self.get_date_component(date, &granularity)?;
            let granularity_name = TimePartitioner::get_granularity_name(granularity);
            full_key = format!("{}/{}={}", full_key, granularity_name, foo);

            if granularity == &granularity_target {
                break;
            }
        }

        Ok(full_key)
    }

    pub fn parse_datetime(&self, time_str: &str) -> Result<DateTime<FixedOffset>, io::Error> {
        match DateTime::parse_from_rfc3339(time_str) {
            Ok(date) => Ok(date),
            Err(err) => {
                println!(
                    "Failed to parse time partition string {}, Error: {}",
                    time_str,
                    err.to_string()
                );
                Err(io::Error::new(io::ErrorKind::Other, "Failed to parse time partition string"))
            }
        }
    }

    pub fn get_date_component(&self, date: DateTime<FixedOffset>, granularity: &str) -> Result<u32, io::Error> {
        match granularity {
            "year" => Ok(date.year() as u32),
            "month" => Ok(date.month()),
            "day" => Ok(date.day()),
            "hour" => Ok(date.hour()),
            "minute" => Ok(date.minute()),
            _ => Err(io::Error::new(io::ErrorKind::Other, format!("Did not recognise date granularity of {}", granularity))),
        }
    }

    pub fn get_granularities() -> Vec<String> {
        let prefix = Config::get_time_partition_prefix();
        GRANULARITIES.iter().map(|g| {
            if let Some(p) = &prefix {
                format!("{}{}", p, g)
            } else {
                g.to_string()
            }
        }).collect()
    }
    pub fn get_granularity_name(granularity: &str) -> String {
        let prefix = Config::get_time_partition_prefix();
        if let Some(p) = &prefix {
            format!("{}{}", p, granularity)
        } else {
            granularity.to_string()
        }
    }
}
