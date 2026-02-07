#![no_main]
use libfuzzer_sys::fuzz_target;
use gosh_dl::torrent::Metainfo;

fuzz_target!(|data: &[u8]| {
    // parse() should never panic on arbitrary input
    let _ = Metainfo::parse(data);
});
