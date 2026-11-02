//! `Quote::decode`: a reference decode predicts accept or the exact error, and accept implies
//! the canonical round trip `encode(decode(x)) == x`. Structured inputs start from a valid
//! encoding, so the accepting path and every later quote rule are reached.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quotebook_fuzz::{check_quote_decode, QuoteInput};

fuzz_target!(|input: QuoteInput| {
    let _ = check_quote_decode(&input.bytes());
});
