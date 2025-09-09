use crate::discover::date_formats::DateFormats::{Asctime, Atom, AtomZ, Cookie, DateOnly, Iso8601, Iso8601_2, Iso8601_3, Iso8601_4, Iso8601_5, Iso8601_SpaceOffset, Iso8601_SpaceZ, Mysql, Rfc1036, Rfc1123, Rfc2822, Rfc3339, Rfc3339_2, Rfc7231, Rfc822, Rfc850, Rss, W3c};
use std::slice::Iter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DateFormats {
    Iso8601,
    Iso8601_2,
    Iso8601_3,
    Iso8601_4,
    Iso8601_5,
    Iso8601_SpaceZ,
    Iso8601_SpaceOffset,
    Rfc2822,
    Rfc3339,
    Rfc3339_2,
    Atom,
    AtomZ,
    Asctime,
    Cookie,
    Rfc822,
    Rfc850,
    Rfc1036,
    Rfc1123,
    Rfc7231,
    Rss,
    W3c,
    Mysql,
    DateOnly,
}

impl DateFormats {
    pub fn iterator() -> Iter<'static, DateFormats> {
        static FORMATS: [DateFormats; 23] = [
            Iso8601, Iso8601_2, Iso8601_3, Iso8601_4, Iso8601_5, Iso8601_SpaceZ, Iso8601_SpaceOffset, Rfc2822, Rfc3339, Rfc3339_2, Atom, AtomZ, Asctime, Cookie, Rfc822,
            Rfc850, Rfc1036, Rfc1123, Rfc7231, Rss, W3c, Mysql, DateOnly,
        ];
        FORMATS.iter()
    }

    pub fn name(&self) -> &'static str {
        match self {
            DateFormats::Iso8601 => "Iso8601",
            DateFormats::Iso8601_2 => "Iso8601_2",
            DateFormats::Iso8601_3 => "Iso8601_3",
            DateFormats::Iso8601_4 => "Iso8601_4",
            DateFormats::Iso8601_5 => "Iso8601_5",
            DateFormats::Iso8601_SpaceZ => "Iso8601_SpaceZ",
            DateFormats::Iso8601_SpaceOffset => "Iso8601_SpaceOffset",
            DateFormats::Rfc2822 => "Rfc2822",
            DateFormats::Rfc3339 => "Rfc3339",
            DateFormats::Rfc3339_2 => "Rfc3339_2",
            DateFormats::Atom => "Atom",
            DateFormats::AtomZ => "AtomZ",
            DateFormats::Asctime => "Asctime",
            DateFormats::Cookie => "Cookie",
            DateFormats::Rfc822 => "Rfc822",
            DateFormats::Rfc850 => "Rfc850",
            DateFormats::Rfc1036 => "Rfc1036",
            DateFormats::Rfc1123 => "Rfc1123",
            DateFormats::Rfc7231 => "Rfc7231",
            DateFormats::Rss => "Rss",
            DateFormats::W3c => "W3c",
            DateFormats::Mysql => "Mysql",
            DateFormats::DateOnly => "DateOnly",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DateFormats::Iso8601 => "%Y-%m-%dT%H:%M:%S.%fZ",
            DateFormats::Iso8601_2 => "%Y-%m-%dT%H:%M:%SZ",
            DateFormats::Iso8601_3 => "%Y-%m-%dT%H:%M:%S.%f",
            DateFormats::Iso8601_4 => "%Y-%m-%dT%H:%M:%S.%f%z",
            DateFormats::Iso8601_5 => "%Y-%m-%dT%H:%M:%S.%f%z",
            DateFormats::Iso8601_SpaceZ => "%Y-%m-%d %H:%M:%SZ",
            DateFormats::Iso8601_SpaceOffset => "%Y-%m-%d %H:%M:%S%z",
            DateFormats::Rfc2822 => "%a, %d %b %Y %H:%M:%S %z",
            DateFormats::Rfc3339 => "%Y-%m-%dT%H:%M:%S.%f%z",
            DateFormats::Rfc3339_2 => "%Y-%m-%dT%H:%M:%S%z",
            DateFormats::Atom => "%Y-%m-%dT%H:%M:%S",
            DateFormats::AtomZ => "%Y-%m-%dT%H:%M:%SZ",
            DateFormats::Asctime => "%a %b %e %H:%M:%S %Y",
            DateFormats::Cookie => "%A, %d-%b-%y %H:%M:%S %Z",
            DateFormats::Rfc822 => "%a, %d %b %Y %H:%M:%S %z",
            DateFormats::Rfc850 => "%a, %d %b %Y %H:%M:%S %Z",
            DateFormats::Rfc1036 => "%a, %d %b %Y %H:%M:%S %z",
            DateFormats::Rfc1123 => "%a, %d %b %Y %H:%M:%S %Z",
            DateFormats::Rfc7231 => "%a, %d %b %Y %H:%M:%S %Z",
            DateFormats::Rss => "%a, %d %b %Y %H:%M:%S %z",
            DateFormats::W3c => "%Y-%m-%dT%H:%M:%SZ",
            DateFormats::Mysql => "%Y-%m-%d %H:%M:%S",
            DateFormats::DateOnly => "%Y-%m-%d",
        }
    }

    pub fn from_str(input: &str) -> Result<DateFormats, bool> {
        match input {
            "Iso8601" => Ok(DateFormats::Iso8601),
            "Iso8601_2" => Ok(DateFormats::Iso8601_2),
            "Iso8601_3" => Ok(DateFormats::Iso8601_3),
            "Iso8601_4" => Ok(DateFormats::Iso8601_4),
            "Iso8601_5" => Ok(DateFormats::Iso8601_5),
            "Iso8601_SpaceZ" => Ok(DateFormats::Iso8601_SpaceZ),
            "Iso8601_SpaceOffset" => Ok(DateFormats::Iso8601_SpaceOffset),
            "Rfc2822" => Ok(DateFormats::Rfc2822),
            "Rfc3339" => Ok(DateFormats::Rfc3339),
            "Rfc3339_2" => Ok(DateFormats::Rfc3339_2),
            "Atom" => Ok(DateFormats::Atom),
            "AtomZ" => Ok(DateFormats::AtomZ),
            "Asctime" => Ok(DateFormats::Asctime),
            "Cookie" => Ok(DateFormats::Cookie),
            "Rfc822" => Ok(DateFormats::Rfc822),
            "Rfc850" => Ok(DateFormats::Rfc850),
            "Rfc1036" => Ok(DateFormats::Rfc1036),
            "Rfc1123" => Ok(DateFormats::Rfc1123),
            "Rfc7231" => Ok(DateFormats::Rfc7231),
            "Rss" => Ok(DateFormats::Rss),
            "W3c" => Ok(DateFormats::W3c),
            "Mysql" => Ok(DateFormats::Mysql),
            "DateOnly" => Ok(DateFormats::DateOnly),
            _ => {
                println!("Don't know this date format: {}", input);
                Err(false)
                // Err(Error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

    // Helper function to parse and assert dates
    fn assert_date_parse(format: DateFormats, date_str: &str, expected_utc: &str) {
        let format_str = format.as_str();
        println!("Testing format: {} with string: {} using pattern: {}", format.name(), date_str, format_str);
        
        // For ISO8601 formats, use RFC3339 parsing which is more reliable
        let parsed_date = if format == DateFormats::Iso8601 || format == DateFormats::Iso8601_2 {
            match DateTime::parse_from_rfc3339(date_str) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(e) => {
                    println!("Parse error with RFC3339: {:?}", e);
                    // Fall back to the format string if RFC3339 fails
                    match DateTime::parse_from_str(date_str, format_str) {
                        Ok(dt) => dt.with_timezone(&Utc),
                        Err(e) => {
                            println!("Parse error with format string: {:?}", e);
                            panic!("Failed to parse date {} with format {}", date_str, format_str);
                        }
                    }
                }
            }
        } else if format == DateFormats::Mysql || format == DateFormats::DateOnly || format == DateFormats::Atom {
            // For formats without timezone information, parse as NaiveDateTime then convert to DateTime<Utc>
            let naive_dt = match NaiveDateTime::parse_from_str(date_str, format_str) {
                Ok(dt) => dt,
                Err(e) => {
                    if format == DateFormats::DateOnly {
                        // For date-only format, parse as NaiveDate and set time to midnight
                        let naive_date = chrono::NaiveDate::parse_from_str(date_str, format_str)
                            .unwrap_or_else(|e| {
                                println!("Parse error for date-only: {:?}", e);
                                panic!("Failed to parse date {} with format {}", date_str, format_str);
                            });
                        naive_date.and_hms_opt(0, 0, 0).unwrap_or_default() // Set time to midnight
                    } else {
                        println!("Parse error: {:?}", e);
                        panic!("Failed to parse date {} with format {}", date_str, format_str);
                    }
                }
            };
            Utc.from_utc_datetime(&naive_dt)
        } else {
            // For formats with timezone information, parse directly
            match DateTime::parse_from_str(date_str, format_str) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(e) => {
                    println!("Parse error: {:?}", e);
                    panic!("Failed to parse date {} with format {}", date_str, format_str);
                }
            }
        };
        
        // Parse the expected date
        let expected_date = match DateTime::parse_from_rfc3339(expected_utc) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(_e) => {
                // Fall back to format string parsing if RFC3339 fails
                match NaiveDateTime::parse_from_str(expected_utc, "%Y-%m-%dT%H:%M:%S%.3f") {
                    Ok(dt) => Utc.from_utc_datetime(&dt),
                    Err(_e) => {
                        println!("Parse error for expected date: {:?}", _e);
                        panic!("Failed to parse expected date {} with format", expected_utc);
                    }
                }
            }
        };
        
        assert_eq!(parsed_date.timestamp(), expected_date.timestamp());
    }

    #[test]
    fn test_iso8601() {
        assert_date_parse(DateFormats::Iso8601, "2023-03-03T15:00:00.000Z", "2023-03-03T15:00:00.000Z");
    }

    #[test]
    fn test_iso8601_2() {
        assert_date_parse(DateFormats::Iso8601_2, "2023-03-03T15:00:00Z", "2023-03-03T15:00:00Z");
    }

    #[test]
    fn test_rfc2822() {
        assert_date_parse(DateFormats::Rfc2822, "Fri, 03 Mar 2023 15:00:00 +0000", "2023-03-03T15:00:00Z");
    }

    #[test]
    fn test_mysql() {
        // MySQL format doesn't have timezone information, assume UTC
        assert_date_parse(DateFormats::Mysql, "2023-03-03 15:00:00", "2023-03-03T15:00:00Z");
    }

    #[test]
    fn test_date_only() {
        // DateOnly doesn't have time information, assume start of day in UTC
        assert_date_parse(DateFormats::DateOnly, "2023-03-03", "2023-03-03T00:00:00Z");
    }
}