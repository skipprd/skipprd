use crate::discover::date_formats::DateFormats::{Asctime, Atom, AtomZ, Cookie, DateOnly, Iso8601, Iso8601_2, Iso8601_3, Iso8601_4, Iso8601_5, Mysql, Rfc1036, Rfc1123, Rfc2822, Rfc3339, Rfc3339_2, Rfc7231, Rfc822, Rfc850, Rss, W3c};
use std::slice::Iter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DateFormats {
    Iso8601,
    Iso8601_2,
    Iso8601_3,
    Iso8601_4,
    Iso8601_5,
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
        static FORMATS: [DateFormats; 22] = [
            Iso8601, Iso8601_2, Iso8601_3, Iso8601_4, Iso8601_5, Iso8601_3, Rfc2822, Rfc3339, Rfc3339_2, Atom, AtomZ, Asctime, Cookie, Rfc822,
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
            DateFormats::Iso8601_2 => "%Y-%m-%dT%H:%M:%S%.3fZ",
            DateFormats::Iso8601_3 => "%Y-%m-%dT%H:%M:%S.%f",
            DateFormats::Iso8601_4 => "%Y-%m-%dT%H:%M:%S.%f%:z",
            DateFormats::Iso8601_5 => "%Y-%m-%dT%H:%M:%S%.3f%:z",
            DateFormats::Rfc2822 => "%a, %d %b %Y %T %z",
            DateFormats::Rfc3339 => "%Y-%m-%dT%H:%M:%S.%f%:z",
            DateFormats::Rfc3339_2 => "%Y-%m-%dT%H:%M:%S%:z",
            DateFormats::Atom => "%Y-%m-%dT%H:%M:%S",
            DateFormats::AtomZ => "%Y-%m-%dT%H:%M:%SZ",
            DateFormats::Asctime => "%a %b %e %H:%M:%S %Y",
            DateFormats::Cookie => "%A, %d-%b-%y %H:%M:%S %Z",
            DateFormats::Rfc822 => "%a, %d %b %Y %H:%M:%S %z",
            DateFormats::Rfc850 => "%a, %d %b %Y %H:%M:%S %Z",
            DateFormats::Rfc1036 => "%a, %d %b %Y %H:%M:%S %z",
            DateFormats::Rfc1123 => "%a, %d %b %Y %H:%M:%S  %Z",
            DateFormats::Rfc7231 => "%a, %d %b %Y %H:%M:%S %Z",
            DateFormats::Rss => "%a, %d %b %Y %H:%M:%S %z",
            DateFormats::W3c => "%Y-%m-%dT%H:%M:%S%.fZ",
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
    use chrono::{DateTime};

    // Helper function to parse and assert dates
    fn assert_date_parse(format: DateFormats, date_str: &str, expected_utc: &str) {
        let format_str = format.as_str();
        let parsed_date = DateTime::parse_from_str(date_str, format_str).unwrap();
        let expected_date = DateTime::parse_from_str(expected_utc, DateFormats::Iso8601.as_str()).unwrap();
        assert_eq!(parsed_date, expected_date);
    }

    #[test]
    fn test_iso8601() {
        assert_date_parse(DateFormats::Iso8601, "2023-03-03T15:00:00.000Z", "2023-03-03T15:00:00.000Z");
    }

    #[test]
    fn test_iso8601_2() {
        assert_date_parse(DateFormats::Iso8601_2, "2023-03-03T15:00:00.123Z", "2023-03-03T15:00:00.123Z");
    }

    #[test]
    fn test_rfc2822() {
        assert_date_parse(DateFormats::Rfc2822, "Fri, 03 Mar 2023 15:00:00 +0000", "2023-03-03T15:00:00Z");
    }

    // Continue with similar tests for each DateFormats variant...

    #[test]
    fn test_mysql() {
        assert_date_parse(DateFormats::Mysql, "2023-03-03 15:00:00", "2023-03-03T15:00:00Z");
    }

    #[test]
    fn test_date_only() {
        assert_date_parse(DateFormats::DateOnly, "2023-03-03", "2023-03-03T00:00:00Z");
    }
}