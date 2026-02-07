#![no_main]
use libfuzzer_sys::fuzz_target;
use gosh_dl::torrent::BencodeValue;

fuzz_target!(|data: &[u8]| {
    // parse() should never panic on arbitrary input
    let _ = BencodeValue::parse(data);
    let _ = BencodeValue::parse_exact(data);
});
