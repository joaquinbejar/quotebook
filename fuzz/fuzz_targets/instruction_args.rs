//! Instruction tag and argument decoders: accept implies `encode(decode(x)) == x` where an
//! encoder exists, and the reference predicts accept (or the exact error) exactly. Covers the
//! I10 additions: tags 8 to 10, `create_series` with a fee and `set_profile`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quotebook_core::instruction::{
    decode_deployment_id, decode_new_admin, decode_paused, decode_u64, CreateSeriesArgs,
    SettleArgs, Tag,
};
use quotebook_core::profile::ProfileArgs;
use quotebook_core::QuotebookError;
use quotebook_fuzz::reference_profile_args;

fuzz_target!(|data: &[u8]| {
    // Tag::split: accepted iff non-empty with a tag 0..=10.
    let tag_ok = matches!(data.first(), Some(t) if *t <= 10);
    assert_eq!(Tag::split(data).is_ok(), tag_ok);
    if let Ok((tag, rest)) = Tag::split(data) {
        assert_eq!(tag as u8, data[0]);
        assert_eq!(rest, &data[1..]);
    }

    // CreateSeriesArgs: 32 bytes (no fee) or 34 (a fee, at most 100 bps).
    if let Ok(a) = CreateSeriesArgs::decode(data) {
        assert_eq!(a.encode().as_slice(), data);
    }
    let w = |i: usize| u64::from_le_bytes(data[8 * i..8 * i + 8].try_into().unwrap());
    let cs_want = if (data.len() == 32 || data.len() == 34)
        && w(0) != 0
        && w(1) != 0
        && (w(2) as i64) > 0
        && w(3) != 0
    {
        match data.get(32..34) {
            Some(f) if u16::from_le_bytes([f[0], f[1]]) > 100 => Err(QuotebookError::FeeTooHigh),
            _ => Ok(()),
        }
    } else {
        Err(QuotebookError::InvalidInstructionData)
    };
    assert_eq!(CreateSeriesArgs::decode(data).map(|_| ()), cs_want);

    // ProfileArgs (set_profile): exact lengths, then the text rules.
    assert_eq!(
        ProfileArgs::decode(data).map(|_| ()),
        reference_profile_args(data)
    );
    if let Ok(a) = ProfileArgs::decode(data) {
        assert_eq!(a.encode().as_slice(), data);
    }

    assert_eq!(SettleArgs::decode(data).is_ok(), data.len() == 9);
    if let Ok(a) = SettleArgs::decode(data) {
        assert_eq!(&a.encode()[..], data);
    }

    assert_eq!(decode_u64(data).is_ok(), data.len() == 8);
    if let Ok(v) = decode_u64(data) {
        assert_eq!(&v.to_le_bytes()[..], data);
    }
    let nonzero = data.iter().any(|b| *b != 0);
    assert_eq!(
        decode_deployment_id(data).is_ok(),
        data.len() == 16 && nonzero
    );
    if let Ok(id) = decode_deployment_id(data) {
        assert_eq!(&id[..], data);
    }
    assert_eq!(decode_new_admin(data).is_ok(), data.len() == 32 && nonzero);
    if let Ok(a) = decode_new_admin(data) {
        assert_eq!(a.as_ref(), data);
    }
    assert_eq!(
        decode_paused(data).ok(),
        match data {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    );
});
