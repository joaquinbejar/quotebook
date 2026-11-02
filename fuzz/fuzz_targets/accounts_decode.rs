//! Account decoders: `accounts::decode::<T>` accepts exactly the right length, tag and
//! version, and an accepted account re-encodes to the same bytes. Structured inputs build
//! valid layouts of `Config`, `Series` and `MakerState`, so every accepting path is reached.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quotebook_fuzz::{check_accounts, AccountInput};

fuzz_target!(|input: AccountInput| {
    let _ = check_accounts(&input.bytes());
});
