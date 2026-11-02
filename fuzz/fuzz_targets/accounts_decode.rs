//! Account decoders: `accounts::decode::<T>` accepts exactly the right length, tag and version,
//! and an accepted account re-encodes to the same bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quotebook_core::accounts::{self, Config, MakerState, ProgramAccount, Series, VERSION};

fn check<T: ProgramAccount + bytemuck::Pod>(data: &[u8]) {
    let ok = data.len() == T::LEN && data[0] == T::TAG as u8 && data[1] == VERSION;
    let got = accounts::decode::<T>(data);
    assert_eq!(got.is_some(), ok);
    if let Some(v) = got {
        assert_eq!(bytemuck::bytes_of(&v), data);
    }
}

fuzz_target!(|data: &[u8]| {
    check::<Config>(data);
    check::<Series>(data);
    check::<MakerState>(data);
});
