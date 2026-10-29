//! quotebook command-line tools over RPC (docs/devnet.md).
//!
//! Every chain interaction goes through the [`rpc::Rpc`] trait: [`rpc::Cluster`] talks to a
//! real cluster with `solana-rpc-client`, and the tests implement it over LiteSVM, so no test
//! touches a network. The binaries (`quotebook-admin`, `quotebook-maker`, `quotebook-taker`)
//! only parse arguments and call [`admin`], [`maker`] and [`taker`].
//!
//! Keys are read from keypair files whose paths are given as arguments; only public keys are
//! ever printed or logged.

pub mod admin;
pub mod args;
pub mod error;
pub mod keys;
pub mod maker;
pub mod market;
pub mod rpc;
pub mod taker;

pub use error::CliError;
