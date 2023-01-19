

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DateFormats {
    Atom,
    Cookie,
    Iso8601,
    Rfc822,
    Rfc850,
    Rfc1036,
    Rfc1123,
    Rfc7231,
    Rfc2822,
    Rfc3339,
    Rss,
    W3c,
    Mysql,
    DateOnly,
}

impl DateFormats {
    pub fn as_str(&self) -> &'static str {
        match self {
            DateFormats::Atom => "Y-m-d\\TH:i:sP",
            DateFormats::Cookie => "l, d-M-Y H:i:s T",
            DateFormats::Iso8601 => "Y-m-d\\TH:i:sO",
            DateFormats::Rfc822 => "D, d M y H:i:s O",
            DateFormats::Rfc850 => "l, d-M-y H:i:s T",
            DateFormats::Rfc1036 => "D, d M y H:i:s O",
            DateFormats::Rfc1123 => "D, d M Y H:i:s O",
            DateFormats::Rfc2822 => "D, d M Y H:i:s O",
            DateFormats::Rfc3339 => "Y-m-d\\TH:i:sP",
            DateFormats::Rfc7231 => "D, d M Y H:i:s \\G\\M\\T",
            DateFormats::Rss => "D, d M Y H:i:s O",
            DateFormats::W3c => "Y-m-d\\TH:i:sP",
            DateFormats::Mysql => "Y-m-d H:i:s",
            DateFormats::DateOnly => "%Y-%m-%d",
        }
    }
}
