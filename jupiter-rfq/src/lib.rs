//! A prototype of a Jupiter RFQ V1 webhook over quotebook's pricing
//! (docs/PLAN-phase2.md, D6; the approved design is docs/design/jupiter-rfq.md).
//!
//! **Off chain only.** It holds no key, never signs or sends a transaction, calls no Jupiter
//! endpoint and no mainnet RPC, and registers nowhere. A fill that passes every check is
//! answered `rejected`, with the reason that the prototype never co-signs.
//!
//! - [`api`]: the wire types, from Jupiter's OpenAPI schema and its `enums.rs`;
//! - [`tx`]: a hand parser for legacy and v0 transactions;
//! - [`programs`], [`validate`]: the fill-transaction validator and its fee budget;
//! - [`pricing`]: amounts in atoms from `quotebook_maker::pricing`;
//! - [`desk`]: quotes, reservations and limits behind one lock;
//! - [`ledger`]: the durable fee debt ledger;
//! - [`config`]: the JSON configuration, with a static wallet and static mids;
//! - [`server`]: the axum routes.

pub mod api;
pub mod config;
pub mod desk;
pub mod ledger;
pub mod pricing;
pub mod programs;
pub mod server;
pub mod tx;
pub mod validate;

#[cfg(test)]
mod tests;
