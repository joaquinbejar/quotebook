//! The read loop: the feed accounts and the series, once a minute. No Hermes and no key.
//!
//! If it stops, quotes stop: age is judged on every mid ([`crate::feed`]).

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use quotebook_cli::rpc::Rpc;

use crate::config::DemoConfig;
use crate::feed::Board;
use crate::series::read_series;

/// The time between reads.
pub const READ_INTERVAL: Duration = Duration::from_secs(60);

/// One round of reads. A failed read changes nothing: the board's values stand until their age
/// limit. RPC errors are already redacted by `quotebook_cli::rpc::Cluster`.
pub fn poll_once(rpc: &mut dyn Rpc, config: &DemoConfig, board: &Board) {
    match read_series(rpc, config) {
        Ok(series) => board.set_series(&series),
        Err(e) => tracing::warn!(error = %e, "series not read; keeping the last list"),
    }
    for metal in &config.underlyings {
        let account = match rpc.account(&metal.feed_account) {
            Ok(Some(a)) => a,
            Ok(None) => {
                tracing::warn!(underlying = ?metal.underlying, "feed account not found");
                continue;
            }
            Err(e) => {
                tracing::warn!(underlying = ?metal.underlying, error = %e, "feed not read");
                continue;
            }
        };
        let owner = account.owner.to_bytes();
        match board.ingest(metal.underlying, &owner, &account.data) {
            Some(Ok(spot)) => {
                tracing::debug!(underlying = ?metal.underlying, publish_time = spot.publish_time, "spot read");
            }
            Some(Err(e)) => {
                tracing::warn!(underlying = ?metal.underlying, error = %e, "spot update refused")
            }
            None => tracing::warn!(underlying = ?metal.underlying, "board unavailable"),
        }
    }
}

/// Runs [`poll_once`] every `interval` on its own thread, forever.
///
/// # Errors
///
/// The operating system's error if the thread cannot start.
pub fn spawn_reader<R: Rpc + Send + 'static>(
    mut rpc: R,
    config: DemoConfig,
    board: Arc<Board>,
    interval: Duration,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("demo-reader".into())
        .spawn(move || loop {
            poll_once(&mut rpc, &config, &board);
            std::thread::sleep(interval);
        })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros,
        clippy::unwrap_used
    )]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use metalbook_core::Underlying;
    use pyth_lite::RECEIVER_PROGRAM_ID;
    use quotebook_cli::CliError;
    use quotebook_core::UnixTimestamp;
    use quotebook_maker::server::FixedTime;
    use rust_decimal::Decimal;
    use solana_account::Account;
    use solana_address::Address;

    use super::*;
    use crate::config::{SeriesPair, UnderlyingConfig};
    use crate::test_support::{update, Update, FEED};

    const NOW: i64 = 1_793_900_000;

    /// Answers with one feed account, or fails every call.
    struct FakeRpc {
        feed: Address,
        data: Vec<u8>,
        fail: bool,
        calls: Arc<AtomicUsize>,
    }

    impl Rpc for FakeRpc {
        fn account(&mut self, address: &Address) -> Result<Option<Account>, CliError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(CliError::Rpc("down".into()));
            }
            if *address != self.feed {
                return Ok(None);
            }
            Ok(Some(Account {
                lamports: 1,
                data: self.data.clone(),
                owner: Address::new_from_array(RECEIVER_PROGRAM_ID),
                executable: false,
                rent_epoch: 0,
            }))
        }

        fn latest_blockhash(&mut self) -> Result<solana_hash::Hash, CliError> {
            Err(CliError::Rpc("unused".into()))
        }

        fn send(
            &mut self,
            _: &solana_transaction::Transaction,
        ) -> Result<solana_signature::Signature, CliError> {
            Err(CliError::Rpc("unused".into()))
        }

        fn genesis_hash(&mut self) -> Result<solana_hash::Hash, CliError> {
            Err(CliError::Rpc("unused".into()))
        }
    }

    fn config() -> DemoConfig {
        DemoConfig {
            metalbook_program: Address::new_from_array([90; 32]),
            quotebook_program: Address::new_from_array([91; 32]),
            usdc_mint: Address::new_from_array([92; 32]),
            underlyings: vec![UnderlyingConfig {
                underlying: Underlying::Xag,
                feed_account: Address::new_from_array([93; 32]),
                feed_id: FEED,
                vol: Decimal::new(30, 2),
                rate: Decimal::new(4, 2),
                carry: Decimal::ZERO,
            }],
            pairs: vec![SeriesPair {
                metalbook_series: Address::new_from_array([1; 32]),
                quotebook_series: Address::new_from_array([2; 32]),
            }],
        }
    }

    fn board(config: &DemoConfig) -> Arc<Board> {
        Arc::new(Board::new(
            config,
            Box::new(FixedTime(UnixTimestamp::new(NOW))),
        ))
    }

    #[test]
    fn a_round_reads_the_series_and_the_feed() {
        let config = config();
        let board = board(&config);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut rpc = FakeRpc {
            feed: Address::new_from_array([93; 32]),
            data: update(Update::at(NOW)),
            fail: false,
            calls: Arc::clone(&calls),
        };
        poll_once(&mut rpc, &config, &board);
        // The one configured series, missing here, and the one feed.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_round_changes_nothing() {
        let config = config();
        let board = board(&config);
        let series = crate::test_support::capped_call(1, 52, 1_794_603_600);
        board.set_series(&[series]);
        board
            .ingest(
                Underlying::Xag,
                &RECEIVER_PROGRAM_ID,
                &update(Update::at(NOW)),
            )
            .unwrap()
            .unwrap();
        let before = board.mid(&series.metalbook_series);
        assert!(before.is_some());
        let mut rpc = FakeRpc {
            feed: Address::new_from_array([93; 32]),
            data: Vec::new(),
            fail: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        poll_once(&mut rpc, &config, &board);
        assert_eq!(board.mid(&series.metalbook_series), before);
    }

    #[test]
    fn the_thread_keeps_reading() {
        let config = config();
        let board = board(&config);
        let calls = Arc::new(AtomicUsize::new(0));
        let rpc = FakeRpc {
            feed: Address::new_from_array([93; 32]),
            data: update(Update::at(NOW)),
            fail: false,
            calls: Arc::clone(&calls),
        };
        let handle = spawn_reader(rpc, config, board, Duration::from_millis(1)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while calls.load(Ordering::SeqCst) < 6 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(calls.load(Ordering::SeqCst) >= 6);
        assert!(!handle.is_finished());
    }
}
