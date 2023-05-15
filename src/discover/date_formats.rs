#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DateFormats {
    Iso8601,
    Iso8601_2,
    Rfc2822,
    Rfc3339,
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
    pub fn name(&self) -> &'static str {
        match self {
            DateFormats::Iso8601 => "Iso8601",
            DateFormats::Iso8601_2 => "Iso8601",
            DateFormats::Rfc2822 => "Rfc2822",
            DateFormats::Rfc3339 => "Rfc3339",
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
            DateFormats::Rfc2822 => "%a, %d %b %Y %T %z",
            DateFormats::Rfc3339 => "%Y-%m-%dT%H:%M:%S%.f%:z",
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
            "Rfc2822" => Ok(DateFormats::Rfc2822),
            "Rfc3339" => Ok(DateFormats::Rfc3339),
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
