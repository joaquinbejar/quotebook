//! quotebook maker service (docs/PLAN.md, section 4.6 and milestone 6).
//!
//! A maker prices a taker's request with market-maker-rs (Avellaneda-Stoikov), checks the
//! price against the mid ([`pricing`]), converts it to on-chain ticks rounding toward itself
//! ([`market`]), allocates a nonce under the issuance bound ([`issuer`]), signs the quote's
//! canonical bytes ([`keys`]) and answers over WebSocket ([`server`], [`protocol`]). The
//! taker side requests, verifies and ranks quotes ([`client`]); the SDK then builds the
//! settle transaction.
//!
//! Not in v1: a Pyth Hermes price feed and live inventory from chain (both behind the
//! [`server::PriceFeed`] and [`server::Inventory`] traits), and persisting the issuer across
//! restarts. A restarted service must resume above every nonce it issued, or `cancel_all`
//! first.

pub mod client;
pub mod issuer;
pub mod keys;
pub mod market;
pub mod pricing;
pub mod protocol;
pub mod server;
