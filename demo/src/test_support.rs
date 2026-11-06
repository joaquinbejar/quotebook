//! A `PriceUpdateV2` account image and a series, for tests.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::unwrap_used
)]

use metalbook_core::{
    OptionKind, OuncesPerContract, SeriesKey, SeriesTerms, StrikeUsd, Underlying, UnixTimestamp,
};
use pyth_lite::PRICE_UPDATE_V2_DISCRIMINATOR;
use solana_address::Address;

use crate::series::ListedSeries;

/// The settlement window of [`capped_call`], seconds.
pub(crate) const WINDOW_S: u32 = 1_800;

/// The feed the tests use.
pub(crate) const FEED: [u8; 32] = [1u8; 32];

/// The fields a test varies.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Update {
    pub feed: [u8; 32],
    pub price: i64,
    pub conf: u64,
    pub exponent: i32,
    pub publish_time: i64,
    pub full: bool,
}

impl Update {
    /// A good update published at `publish_time`: 52.84681 at 1 bp of confidence.
    pub(crate) const fn at(publish_time: i64) -> Self {
        Self {
            feed: FEED,
            price: 5_284_681,
            conf: 528,
            exponent: -5,
            publish_time,
            full: true,
        }
    }
}

/// The account's bytes.
pub(crate) fn update(u: Update) -> Vec<u8> {
    let mut d = Vec::with_capacity(134);
    d.extend_from_slice(&PRICE_UPDATE_V2_DISCRIMINATOR);
    d.extend_from_slice(&[2u8; 32]);
    if u.full {
        d.push(1);
    } else {
        d.push(0);
        d.push(5);
    }
    d.extend_from_slice(&u.feed);
    d.extend_from_slice(&u.price.to_le_bytes());
    d.extend_from_slice(&u.conf.to_le_bytes());
    d.extend_from_slice(&u.exponent.to_le_bytes());
    d.extend_from_slice(&u.publish_time.to_le_bytes());
    d.extend_from_slice(&(u.publish_time - 1).to_le_bytes());
    d.extend_from_slice(&u.price.to_le_bytes());
    d.extend_from_slice(&u.conf.to_le_bytes());
    d.extend_from_slice(&7u64.to_le_bytes());
    d.resize(134, 0);
    d
}

/// A 1 oz XAG capped call at `strike` USD, with its cap 10% above.
pub(crate) fn capped_call(n: u8, strike: u64, expiry: i64) -> ListedSeries {
    const USD: u64 = 1_000_000;
    let key = SeriesKey::try_new(
        Underlying::Xag,
        OptionKind::Call,
        StrikeUsd::new(strike * USD),
        UnixTimestamp::new(expiry),
    )
    .unwrap();
    ListedSeries {
        metalbook_series: Address::new_from_array([n; 32]),
        quotebook_series: Address::new_from_array([n.wrapping_add(100); 32]),
        long_mint: Address::new_from_array([n.wrapping_add(200); 32]),
        terms: SeriesTerms::try_new(
            key,
            OuncesPerContract::try_new(1_000).unwrap(),
            StrikeUsd::new(strike * USD * 11 / 10),
        )
        .unwrap(),
        window_s: WINDOW_S,
    }
}
