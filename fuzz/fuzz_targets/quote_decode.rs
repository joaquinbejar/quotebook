//! `Quote::decode`: the reference predicts accept or the exact error, and accept implies the
//! canonical round trip `encode(decode(x)) == x`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quotebook_core::Quote;
use quotebook_fuzz::reference_quote_error;

fuzz_target!(|data: &[u8]| {
    let got = Quote::decode(data);
    match (got, reference_quote_error(data)) {
        (Ok(q), None) => assert_eq!(&q.encode()[..], data),
        (Err(g), Some(w)) => assert_eq!(g, w),
        (g, w) => panic!("core {g:?} but reference {w:?}"),
    }
});
