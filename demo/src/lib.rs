//! The demo maker's metalbook side: which series it quotes, what they are worth and when it
//! must not quote (`docs/PLAN-phase2.md`, D2), its watcher (D4), and its writer cycle and taker
//! (D5). Devnet only.
//!
//! - [`chain`]: [`chain::RpcChainView`], the risk book's finalized view of devnet (D3).
//! - [`config`]: the static pairing of metalbook and quotebook series, vol and rate.
//! - [`series`]: reads and filters the metalbook series.
//! - [`spot`]: the Pyth push-feed value and every check on it.
//! - [`schedule`]: the metals market's listed closures.
//! - [`marks`]: one optfix `price_strip` per expiry.
//! - [`feed`]: [`feed::Board`] and [`feed::SeriesFeed`], the `PriceFeed` the maker service uses.
//! - [`reader`]: the read loop.
//! - [`host`]: the hosted maker's settings, markets and service (D3).
//! - [`ops`]: drain and revoke, judged on finalized state (D3).
//! - [`logging`]: JSON log lines, with a size-rotated file (D3).
//! - [`watch`]: the watcher's checks and alert book (D4).
//! - [`writer_cap`]: the cap on contracts written, read from chain (D4, D5).
//! - [`cycle`]: the writer cycle: its finalized view, plan and runner (D5).
//! - [`taker`]: the demo taker's choices and its `trades.jsonl` line (D5).
//!
//! A mid is in USDC per long token. Long mints have 0 decimals and one token is one contract, so
//! this is the mark per contract; `quotebook_maker::market::MarketInfo::price_to_ticks` converts
//! it at the series' lot sizes and rounds toward the maker.
//!
//! The limits here are demo settings, not a statement about price quality: spot can be minutes
//! old, which is acceptable only because the service signs for its own taker. Logs never carry
//! keys or RPC URLs.

pub mod chain;
pub mod config;
pub mod cycle;
pub mod feed;
pub mod host;
pub mod logging;
pub mod marks;
pub mod ops;
pub mod reader;
pub mod schedule;
pub mod series;
pub mod spot;
pub mod taker;
pub mod watch;
pub mod writer_cap;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod wiring;
