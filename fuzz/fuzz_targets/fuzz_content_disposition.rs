#![no_main]
use libfuzzer_sys::fuzz_target;
use gosh_dl::http::parse_content_disposition;

fuzz_target!(|data: &str| {
    // parse_content_disposition() should never panic on arbitrary UTF-8 strings
    let _ = parse_content_disposition(data);
});
