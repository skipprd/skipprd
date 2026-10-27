use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use std::io;

use super::BufferChunker;

const GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];

pub struct TimePartitioner {
    filename: String,
}

impl TimePartitioner {
    pub fn new(filename: &str) -> Self {
        Self {
            filename: filename.to_string(),
        }
    }

    pub fn process_from_layout(
        &self,
        granularity_target: Option<&str>,
        prefix: Option<&str>,
    ) -> Result<String, io::Error> {
        let Some(granularity_target) = granularity_target.map(str::trim).filter(|s| !s.is_empty())
        else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Time partition granularity is empty",
            ));
        };
        let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(&self.filename);
        if time_partition_str.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Time partition string is empty",
            ));
        }
        let date = self.parse_datetime(&time_partition_str)?;
        let mut full_key = String::new();
        for granularity in GRANULARITIES.iter() {
            let component = Self::get_date_component(date, granularity)?;
            let granularity_name = match prefix {
                Some(p) if !p.is_empty() => format!("{p}{granularity}"),
                _ => granularity.to_string(),
            };
            full_key = format!("{full_key}/{granularity_name}={component}");
            if granularity.eq_ignore_ascii_case(granularity_target) {
                return Ok(full_key);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported time partition granularity '{granularity_target}'"),
        ))
    }

    pub fn parse_datetime(&self, time_str: &str) -> Result<DateTime<FixedOffset>, io::Error> {
        DateTime::parse_from_rfc3339(time_str).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "Failed to parse time partition string",
            )
        })
    }

    pub fn get_date_component(
        date: DateTime<FixedOffset>,
        granularity: &str,
    ) -> Result<u32, io::Error> {
        match granularity {
            "year" => Ok(date.year() as u32),
            "month" => Ok(date.month()),
            "day" => Ok(date.day()),
            "hour" => Ok(date.hour()),
            "minute" => Ok(date.minute()),
            _ => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Did not recognise date granularity of {granularity}"),
            )),
        }
    }
}
