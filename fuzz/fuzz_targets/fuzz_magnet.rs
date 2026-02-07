#![no_main]
use libfuzzer_sys::fuzz_target;
use gosh_dl::torrent::MagnetUri;

fuzz_target!(|data: &str| {
    // parse() should never panic on arbitrary UTF-8 strings
    let _ = MagnetUri::parse(data);
});
