//! The demo maker's metalbook side: which series it quotes, what they are worth and when it
//! must not quote (`docs/PLAN-phase2.md`, D2). Devnet only.
//!
//! - [`config`]: the static pairing of metalbook and quotebook series, vol and rate.
//! - [`series`]: reads and filters the metalbook series.
//! - [`spot`]: the Pyth push-feed value and every check on it.
//! - [`schedule`]: the metals market's listed closures.
//! - [`marks`]: one optfix `price_strip` per expiry.
//! - [`feed`]: [`feed::Board`] and [`feed::SeriesFeed`], the `PriceFeed` the maker service uses.
//! - [`reader`]: the read loop.
//!
//! A mid is in USDC per long token. Long mints have 0 decimals and one token is one contract, so
//! this is the mark per contract; `quotebook_maker::market::MarketInfo::price_to_ticks` converts
//! it at the series' lot sizes and rounds toward the maker.
//!
//! The limits here are demo settings, not a statement about price quality: spot can be minutes
//! old, which is acceptable only because the service signs for its own taker. Logs never carry
//! keys or RPC URLs.

pub mod config;
pub mod feed;
pub mod marks;
pub mod reader;
pub mod schedule;
pub mod series;
pub mod spot;

#[cfg(test)]
pub(crate) mod test_support;
