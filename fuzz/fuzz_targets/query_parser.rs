#![no_main]

use libfuzzer_sys::fuzz_target;
use rsbts::query::Query;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        if let Ok(query) = Query::parse(input) {
            let _compiled = query.compile();
        }
    }
});
