#![no_main]

use libfuzzer_sys::fuzz_target;
use skipprd::sqlrt::parser::SParser;

fuzz_target!(|data: &[u8]| {
    let Ok(sql) = std::str::from_utf8(data) else {
        return;
    };
    if sql.len() > 4096 {
        return;
    }
    if let Ok(mut parser) = SParser::new(sql) {
        let _ = parser.parse_statement();
    }
});
