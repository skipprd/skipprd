use crate::buffer::BufferChunker;
use crate::helpers::configuration::Config;
use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use std::io;

const GRANULARITIES: [&str; 5] = ["year", "month", "day", "hour", "minute"];

pub struct TimePartitioner {
    filename: String,
}

impl TimePartitioner {
    pub fn new(filename: &String) -> Self {
        TimePartitioner {
            filename: filename.clone(),
        }
    }

    pub fn process(&self, config: &Config) -> Result<String, io::Error> {
        let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(&self.filename);

        if time_partition_str.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Time partition string is empty",
            ));
        }

        let granularity_target = config.get_transform_batch_time_unit();
        let date = self.parse_datetime(&time_partition_str)?;

        let mut full_key = String::new();
        for granularity in GRANULARITIES.iter() {
            let foo = TimePartitioner::get_date_component(date, granularity)?;
            let granularity_name = TimePartitioner::get_granularity_name(config, granularity);
            full_key = format!("{}/{}={}", full_key, granularity_name, foo);

            if granularity == &granularity_target {
                break;
            }
        }

        Ok(full_key)
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
            let foo = TimePartitioner::get_date_component(date, granularity)?;
            let granularity_name = match prefix {
                Some(p) if !p.is_empty() => format!("{p}{granularity}"),
                _ => granularity.to_string(),
            };
            full_key = format!("{}/{}={}", full_key, granularity_name, foo);

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
        match DateTime::parse_from_rfc3339(time_str) {
            Ok(date) => Ok(date),
            Err(err) => {
                println!(
                    "Failed to parse time partition string {}, Error: {}",
                    time_str,
                    err.to_string()
                );
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Failed to parse time partition string",
                ))
            }
        }
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
                format!("Did not recognise date granularity of {}", granularity),
            )),
        }
    }

    pub fn get_granularity_names(config: &Config) -> Vec<String> {
        let granularity_target = config.get_transform_batch_time_unit();

        let mut names: Vec<String> = Vec::new();

        for granularity in GRANULARITIES.iter() {
            let granularity_name = TimePartitioner::get_granularity_name(config, granularity);

            names.push(granularity_name.clone());

            if granularity == &granularity_target {
                break;
            }
        }

        names
    }

    pub fn get_granularity_values(&self, config: &Config) -> Result<Vec<u32>, io::Error> {
        let time_partition_str = BufferChunker::decode_file_time_to_datetime_string(&self.filename);

        if time_partition_str.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Time partition string is empty",
            ));
        }

        let date = self.parse_datetime(&time_partition_str)?;

        let granularity_target = config.get_transform_batch_time_unit();

        let mut granularities: Vec<u32> = Vec::new();

        for granularity in GRANULARITIES.iter() {
            let date_part = TimePartitioner::get_date_component(date, &granularity)?;
            granularities.push(date_part);

            if granularity == &granularity_target {
                break;
            }
        }

        Ok(granularities)
    }

    pub fn get_granularity_name(config: &Config, granularity: &str) -> String {
        let prefix = config.get_time_partition_prefix();
        if let Some(p) = &prefix {
            format!("{}{}", p, granularity)
        } else {
            granularity.to_string()
        }
    }
}

/// Hive-style object key: `{prefix}/{namespace}/{p_field=...}/{p_year=...}/{stem}.{ext}`.
pub fn hive_object_relative_path(
    prefix: &str,
    filename: &str,
    object_stem: &str,
    extension: &str,
    time_partition_granularity: Option<&str>,
    time_partition_prefix: Option<&str>,
) -> String {
    let namespace = BufferChunker::decode_file_namespace(filename);
    let prefix = prefix.trim_matches('/');
    let mut parts = Vec::new();
    if !prefix.is_empty() {
        parts.push(prefix.to_string());
    }
    if !namespace.is_empty() {
        parts.push(namespace);
    }
    let partition_path = BufferChunker::decode_file_partition(filename);
    if !partition_path.is_empty() {
        parts.push(partition_path);
    }
    let filename_owned = filename.to_string();
    if let Ok(time_key) = TimePartitioner::new(&filename_owned)
        .process_from_layout(time_partition_granularity, time_partition_prefix)
    {
        let time_key = time_key.trim_matches('/');
        if !time_key.is_empty() {
            parts.push(time_key.to_string());
        }
    }
    let extension = extension.trim_start_matches('.');
    let file = if extension.is_empty() {
        object_stem.to_string()
    } else {
        format!("{object_stem}.{extension}")
    };
    parts.push(file);
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_from_layout_uses_runtime_granularity() {
        let filename = "buffer=test&namespace=&partition=&time=1645296045".to_string();
        let key = TimePartitioner::new(&filename)
            .process_from_layout(Some("day"), None)
            .unwrap();
        assert_eq!(key, "/year=2022/month=2/day=19");
    }

    #[test]
    fn process_from_layout_empty_granularity_skips_time_key() {
        let filename = "buffer=test&namespace=&partition=&time=1645296045".to_string();
        let err = TimePartitioner::new(&filename)
            .process_from_layout(None, None)
            .unwrap_err();
        assert!(err.to_string().contains("granularity is empty"));
    }

    #[test]
    fn hive_object_relative_path_covers_namespace_partition_time_and_edges() {
        let hive = "buffer=test&namespace=events&partition=p_rmc=EU1&time=1645296045";
        let encoded_partition =
            "buffer=test&namespace=events&partition=region%252Dus&time=1645296045";
        let cases = [
            (
                "warehouse",
                hive,
                "apply-0001",
                "jsonl",
                Some("day"),
                Some("p_"),
                "warehouse/events/p_rmc=EU1/p_year=2022/p_month=2/p_day=19/apply-0001.jsonl",
            ),
            (
                "/root/",
                "namespace=events",
                "apply-0001",
                "parquet",
                None,
                None,
                "root/events/apply-0001.parquet",
            ),
            (
                "",
                hive,
                "apply-0001",
                "parquet",
                Some("day"),
                Some("p_"),
                "events/p_rmc=EU1/p_year=2022/p_month=2/p_day=19/apply-0001.parquet",
            ),
            (
                "warehouse",
                "buffer=test&namespace=&partition=&time=1645296045",
                "apply-0001",
                "parquet",
                Some("day"),
                None,
                "warehouse/year=2022/month=2/day=19/apply-0001.parquet",
            ),
            (
                "warehouse",
                hive,
                "apply-0001",
                "parquet",
                None,
                None,
                "warehouse/events/p_rmc=EU1/apply-0001.parquet",
            ),
            (
                "exports",
                encoded_partition,
                "apply-0001",
                "parquet",
                None,
                None,
                "exports/events/region/us/apply-0001.parquet",
            ),
            (
                "",
                "namespace=",
                "apply-0001",
                "jsonl",
                None,
                None,
                "apply-0001.jsonl",
            ),
        ];
        for (prefix, filename, stem, ext, gran, time_prefix, expected) in cases {
            assert_eq!(
                hive_object_relative_path(prefix, filename, stem, ext, gran, time_prefix),
                expected,
                "prefix={prefix:?} filename={filename:?} ext={ext}"
            );
        }
    }
}
