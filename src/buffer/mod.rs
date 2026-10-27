pub mod compaction_index;
pub mod compaction_progress;
pub(crate) mod compaction_scheduler;
pub mod compaction_transaction;
pub mod direct_io;
pub mod durable;
pub mod flush_execution_budget;
pub mod ingest_buffer;
pub mod s3_wal_body_cache;
pub mod s3_wal_memory_budget;
pub mod segment_file;
pub mod sink_conflict;
pub mod wal_catalog_import;
pub mod wal_handle;
pub mod wal_object_store;
pub mod wal_persist;
pub mod wal_reader;
pub mod wal_startup;
pub mod wal_store;
pub mod wal_writer;
// wal_accumulator removed in simplified model

use chrono::{Datelike, TimeZone, Timelike, Utc};

use crate::helpers::configuration::Config;

pub fn event_time_bucket(config: &Config, event_time: i64) -> i64 {
    let datetime = Utc.timestamp_opt(event_time, 0).unwrap();

    let bucket_rounded_timestamp = match config.get_transform_batch_time_unit().as_str() {
        "year" => {
            let year = datetime.year();
            Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        }
        "month" => {
            let year = datetime.year();
            let month = datetime.month();
            Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        }
        "day" => {
            let year = datetime.year();
            let month = datetime.month();
            let day = datetime.day();
            Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
                .unwrap()
                .timestamp()
        }
        "hour" => {
            let year = datetime.year();
            let month = datetime.month();
            let day = datetime.day();
            let hour = datetime.hour();
            Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
                .unwrap()
                .timestamp()
        }
        "minute" => {
            let year = datetime.year();
            let month = datetime.month();
            let day = datetime.day();
            let hour = datetime.hour();
            let minute = datetime.minute();
            Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
                .unwrap()
                .timestamp()
        }
        _ => 0,
    };

    if bucket_rounded_timestamp > 0 {
        event_time - ((event_time).rem_euclid(bucket_rounded_timestamp))
    } else {
        0
    }
}

#[cfg(test)]
mod event_time_bucket_tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_event_time_bucket_year() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "year");
        assert_eq!(event_time_bucket(&Config::new(), 1645296045), 1640995200);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_month() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "month");
        assert_eq!(event_time_bucket(&Config::new(), 1645296045), 1643673600);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_day() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "day");
        assert_eq!(event_time_bucket(&Config::new(), 1645296045), 1645228800);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_hour() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "hour");
        assert_eq!(event_time_bucket(&Config::new(), 1645296045), 1645293600);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_minute() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "minute");
        assert_eq!(event_time_bucket(&Config::new(), 1645296045), 1645296000);
    }

    #[test]
    #[serial]
    fn test_event_time_bucket_none() {
        Config::setenv("TRANSFORM_BATCH_TIME_UNIT", "");
        assert_eq!(event_time_bucket(&Config::new(), 1645296045), 0);
    }
}
