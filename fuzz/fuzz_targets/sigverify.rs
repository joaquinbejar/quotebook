//! `sigverify::verified_quote` against the independent reference parser (fuzz/src/lib.rs).
#![no_main]

use libfuzzer_sys::fuzz_target;
use quotebook_fuzz::{check_sigverify, SigInput};

fuzz_target!(|input: SigInput| {
    let _ = check_sigverify(&input.bytes());
});
