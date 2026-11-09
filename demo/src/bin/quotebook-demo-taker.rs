//! The demo taker on devnet (`docs/PLAN-phase2.md`, D5): one run, one fill of 1 lot against the
//! demo maker, one line in `trades.jsonl` ([`quotebook_demo::taker`]).
//!
//! ```text
//! quotebook-demo-taker --url <RPC> --config <DEMO_JSON> --settings <MAKER_JSON>
//!   --taker <KEYPAIR> --maker-address <ADDRESS> --endpoint ws://127.0.0.1:<PORT>
//!   --halt-file <PATH> --trades <PATH> [--dry-run] [--log-file <PATH>]
//! ```
//!
//! A run skips outside market hours, while the maker's halt file exists, without a quotable series
//! or inside the settlement window, and with nothing to sell on either side. Otherwise it requests
//! a quote, checks it as `quotebook-taker` does with `--maker-address` as the only maker allowed,
//! settles it (the taker pays its own fee) and reads the transaction and both sides' balances at
//! `finalized`. Every run appends one line to `--trades` and prints it.
//!
//! `--dry-run` makes every read, asks the maker for its `Status` (no nonce, no reservation) and
//! prints the decision; it requests no quote, sends nothing and writes no line.
//!
//! **Devnet only:** it refuses a cluster whose genesis is not devnet's. The keypair is a file
//! path; only public keys are printed, and no line carries a key or the RPC URL.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use quotebook_cli::args::Args;
use quotebook_cli::rpc::{send, Cluster, Rpc};
use quotebook_cli::taker::{self as cli_taker, MakerFilter};
use quotebook_cli::{admin, keys, market, CliError};
use quotebook_core::BaseLots;
use quotebook_demo::chain::{parse_token_account, WatchedAccount, TOKEN_PROGRAM};
use quotebook_demo::config::DemoConfig;
use quotebook_demo::host::{associated_token_account, HostSettings};
use quotebook_demo::series::{read_series, ListedSeries};
use quotebook_demo::taker::{
    append, choose_series, decide, refused_by_limits, transaction_facts, Balances, Decision,
    Outcome, Situation, TradeLine, SIZE_LOTS,
};
use quotebook_demo::watch::{printable, StatusSource, WsStatus};
use quotebook_demo::{cycle, logging, ops, schedule};
use quotebook_maker::protocol::TakerSide;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Polls of the finalized transaction, 5 s apart.
const FINALIZE_POLLS: u32 = 24;
const POLL: Duration = Duration::from_secs(5);

fn read(path: &str) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|e| CliError::Args(format!("{path}: {e}")))
}

fn host_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn out(value: &serde_json::Value) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}").map_err(|e| CliError::Maker(e.to_string()))
}

struct Run {
    rpc: Cluster,
    demo: DemoConfig,
    taker: Keypair,
    maker: Address,
    endpoint: String,
}

impl Run {
    fn accounts(&self, series: &ListedSeries) -> [Address; 4] {
        let t = self.taker.pubkey();
        [
            associated_token_account(&t, &self.demo.usdc_mint, &TOKEN_PROGRAM),
            associated_token_account(&t, &series.long_mint, &TOKEN_PROGRAM),
            associated_token_account(&self.maker, &self.demo.usdc_mint, &TOKEN_PROGRAM),
            associated_token_account(&self.maker, &series.long_mint, &TOKEN_PROGRAM),
        ]
    }

    /// Both sides' balances at `finalized`, and whether the taker's long account exists.
    fn balances(&mut self, series: &ListedSeries) -> Result<(Balances, bool), CliError> {
        let addresses = self.accounts(series);
        let (_, found) = self.rpc.finalized_accounts(&addresses)?;
        if found.len() != addresses.len() {
            return Err(CliError::Rpc(
                "getMultipleAccounts returned the wrong number of accounts".into(),
            ));
        }
        let t = self.taker.pubkey();
        let owners = [t, t, self.maker, self.maker];
        let mints = [
            self.demo.usdc_mint,
            series.long_mint,
            self.demo.usdc_mint,
            series.long_mint,
        ];
        let mut amounts = [0u64; 4];
        for (i, account) in found.iter().enumerate() {
            let (Some(address), Some(owner), Some(mint), Some(slot)) = (
                addresses.get(i),
                owners.get(i),
                mints.get(i),
                amounts.get_mut(i),
            ) else {
                continue;
            };
            let watched = WatchedAccount {
                address: *address,
                mint: *mint,
            };
            *slot = parse_token_account(account.as_ref(), &watched, owner, &Address::default())
                .map_err(|e| CliError::Account(e.0))?
                .amount;
        }
        let [taker_usdc, taker_long, maker_usdc, maker_long] = amounts;
        let long_exists = found.get(1).is_some_and(Option::is_some);
        Ok((
            Balances {
                taker_usdc,
                taker_long,
                maker_usdc,
                maker_long,
            },
            long_exists,
        ))
    }

    fn trade(
        &mut self,
        series: ListedSeries,
        side: TakerSide,
        before: Balances,
        long_exists: bool,
    ) -> Result<TradeLine, (CliError, Option<String>)> {
        let program = self.demo.quotebook_program;
        let size = BaseLots::new(SIZE_LOTS);
        let [taker_usdc, taker_long, _, _] = self.accounts(&series);
        let plain = |e| (e, None);
        if !long_exists {
            let ix = admin::create_associated_account(
                &self.taker.pubkey(),
                &self.taker.pubkey(),
                &series.long_mint,
                &TOKEN_PROGRAM,
            );
            let sig = send(&mut self.rpc, &[ix], &self.taker, &[]).map_err(plain)?;
            tracing::info!(account = %taker_long, signature = %sig, "taker long account created");
        }
        let id = u64::try_from(host_now()).unwrap_or(0).saturating_mul(1_000);
        let request = cli_taker::request(
            id,
            &series.quotebook_series,
            &self.taker.pubkey(),
            side,
            size,
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| plain(CliError::Maker(e.to_string())))?;
        let answer = runtime
            .block_on(cli_taker::ask(&self.endpoint, &request))
            .map_err(plain)?;
        let allow = [self.maker];
        let filter = MakerFilter {
            expected: Some(&self.maker),
            allow: Some(&allow),
        };
        let checked = cli_taker::check(
            &mut self.rpc,
            &program,
            &series.quotebook_series,
            &answer,
            &self.taker.pubkey(),
            side,
            size,
            &filter,
        )
        .map_err(plain)?;
        let book =
            market::series(&mut self.rpc, &program, &series.quotebook_series).map_err(plain)?;
        let terms = answer.signed.quote.terms();
        let price = terms.price.get();
        let quote_atoms = price
            .checked_mul(SIZE_LOTS)
            .and_then(|v| v.checked_mul(checked.quote_lot_size));
        let fee_atoms = quote_atoms.and_then(|q| quotebook_core::fee::fee(q, book.fee_bps()));
        let signature = cli_taker::settle(
            &mut self.rpc,
            &program,
            &answer,
            &self.taker,
            &self.taker,
            &taker_long,
            &taker_usdc,
            size,
        )
        .map_err(plain)?
        .to_string();
        tracing::info!(%signature, ?side, price, "settled");
        let sent = Some(signature.clone());
        let mut tx = None;
        for _ in 0..FINALIZE_POLLS {
            match self.rpc.finalized_transaction(&signature) {
                Ok(Some(t)) => {
                    tx = Some(t);
                    break;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "transaction read failed"),
            }
            std::thread::sleep(POLL);
        }
        let Some(tx) = tx else {
            return Err((
                CliError::Rpc("settle not seen at finalized before the timeout".into()),
                sent,
            ));
        };
        let (slot, cu, tx_fee, failed) = transaction_facts(&tx);
        if failed {
            return Err((CliError::Rpc("settle failed on chain".into()), sent));
        }
        let (after, _) = self.balances(&series).map_err(|e| (e, sent.clone()))?;
        Ok(TradeLine {
            series: Some(series.quotebook_series),
            metalbook_series: Some(series.metalbook_series),
            side: Some(side),
            price_ticks: Some(price),
            size_lots: Some(SIZE_LOTS),
            quote_atoms,
            fee_atoms,
            signature: sent,
            slot,
            cu,
            tx_fee_lamports: tx_fee,
            balances_before: Some(before),
            balances_after: Some(after),
            ..TradeLine::bare(host_now(), Outcome::Filled, None)
        })
    }
}

/// What a run will do, from its reads.
enum Plan {
    Skip(&'static str),
    Trade {
        series: ListedSeries,
        side: TakerSide,
        before: Balances,
        long_exists: bool,
    },
}

/// Every read before a trade: the finalized Clock, the quotable series and, for the chosen
/// series, both sides' balances.
fn prepare(
    run: &mut Run,
    max_maker_long: u64,
    halt_file: &Path,
) -> Result<(Situation, Plan), CliError> {
    let (_, now) = ops::finalized_clock(&mut run.rpc)?;
    let now = now.get();
    let situation = Situation {
        now,
        market_open: schedule::market_open(now),
        halted: halt_file.exists(),
        series: read_series(&mut run.rpc, &run.demo)?,
        max_maker_long,
    };
    let series = match choose_series(&situation) {
        Ok(s) => s,
        Err(reason) => return Ok((situation, Plan::Skip(reason))),
    };
    let (before, long_exists) = run.balances(&series)?;
    let plan = match decide(&situation, series, &before) {
        Decision::Skip(reason) => Plan::Skip(reason),
        Decision::Trade { series, side } => Plan::Trade {
            series,
            side,
            before,
            long_exists,
        },
    };
    Ok((situation, plan))
}

fn record(path: &Path, line: &TradeLine) -> Result<(), CliError> {
    append(path, line).map_err(|e| CliError::Args(format!("--trades: {e}")))?;
    out(&serde_json::to_value(line).map_err(|e| CliError::Maker(e.to_string()))?)
}

fn main_run(args: &Args) -> Result<bool, CliError> {
    let log_file = args.opt("log-file").map(PathBuf::from);
    logging::init(log_file.as_deref()).map_err(|e| CliError::Args(format!("--log-file: {e}")))?;
    let endpoint = args.req("endpoint")?.to_owned();
    if !endpoint.starts_with("ws://127.0.0.1:") {
        return Err(CliError::Args(
            "--endpoint must be ws://127.0.0.1:<PORT>: the demo maker listens on loopback only"
                .into(),
        ));
    }
    let demo = DemoConfig::from_json(&read(args.req("config")?)?)
        .map_err(|e| CliError::Args(format!("--config: {e}")))?;
    let settings = HostSettings::from_json(&read(args.req("settings")?)?)
        .map_err(|e| CliError::Args(format!("--settings: {e}")))?;
    let trades = PathBuf::from(args.req("trades")?);
    let halt_file = PathBuf::from(args.req("halt-file")?);
    let dry_run = args.flag("dry-run");
    let mut rpc = Cluster::new(args.req("url")?);
    cycle::require_devnet(&mut rpc)?;
    let mut run = Run {
        rpc,
        demo,
        taker: keys::load(args.req("taker")?)?,
        maker: args.parse_req("maker-address")?,
        endpoint,
    };

    let prepared = prepare(&mut run, settings.limits.max_long, &halt_file);
    if dry_run {
        let status = WsStatus::new(&run.endpoint)
            .map_err(|e| e.to_string())
            .and_then(|mut s| s.status());
        let lamports = run
            .rpc
            .finalized_accounts(&[run.taker.pubkey()])
            .ok()
            .and_then(|(_, a)| a.into_iter().next().flatten())
            .map(|a| a.lamports);
        let (situation, decision_json, balances) = match &prepared {
            Err(e) => (
                None,
                serde_json::json!({"error": printable(&e.to_string())}),
                None,
            ),
            Ok((s, Plan::Skip(reason))) => {
                // Shown for the record: the nearest series' balances, even on a skip.
                let nearest = s.series.iter().min_by_key(|l| l.expiry()).copied();
                let b = nearest.and_then(|l| run.balances(&l).ok()).map(|(b, _)| b);
                (Some(s), serde_json::json!({"skip": reason}), b)
            }
            Ok((
                s,
                Plan::Trade {
                    series,
                    side,
                    before,
                    long_exists,
                },
            )) => (
                Some(s),
                serde_json::json!({
                    "trade": side,
                    "series": series.quotebook_series.to_string(),
                    "metalbook_series": series.metalbook_series.to_string(),
                    "taker_long_account_exists": long_exists,
                }),
                Some(*before),
            ),
        };
        return out(&serde_json::json!({
            "dry_run": true,
            "finalized_clock": situation.map(|s| s.now),
            "market_open": situation.map(|s| s.market_open),
            "halted": situation.map(|s| s.halted),
            "quotable_series": situation.map(|s| s.series.len()),
            "taker": run.taker.pubkey().to_string(),
            "taker_lamports": lamports,
            "balances": balances,
            "maker_status": status.map(|s| serde_json::json!({
                "halted": s.halted,
                "open_quotes": s.open_quotes,
                "snapshot_age_secs": s.snapshot_age_secs,
            })).unwrap_or_else(|e| serde_json::json!({"error": printable(&e)})),
            "decision": decision_json,
        }))
        .map(|()| true);
    }

    let line = match prepared {
        Err(e) => TradeLine::bare(host_now(), Outcome::Failed, Some(printable(&e.to_string()))),
        Ok((_, Plan::Skip(reason))) => {
            TradeLine::bare(host_now(), Outcome::Skipped, Some(reason.into()))
        }
        Ok((
            _,
            Plan::Trade {
                series,
                side,
                before,
                long_exists,
            },
        )) => match run.trade(series, side, before, long_exists) {
            Ok(line) => line,
            // Nothing sent and the maker began to halt meanwhile: a skip, like any halted run.
            Err((e, None)) if halt_file.exists() => {
                tracing::info!(error = %e, "the maker began to halt during the run");
                TradeLine::bare(host_now(), Outcome::Skipped, Some("maker halted".into()))
            }
            // Refused before anything was signed: a low allowance or balance, or another limit.
            Err((e, None)) if refused_by_limits(&e).is_some() => {
                tracing::info!(error = %e, "the maker refused the quote");
                TradeLine::bare(host_now(), Outcome::Skipped, refused_by_limits(&e))
            }
            Err((e, signature)) => {
                tracing::error!(error = %e, "trade failed");
                TradeLine {
                    series: Some(series.quotebook_series),
                    metalbook_series: Some(series.metalbook_series),
                    side: Some(side),
                    signature,
                    balances_before: Some(before),
                    ..TradeLine::bare(host_now(), Outcome::Failed, Some(printable(&e.to_string())))
                }
            }
        },
    };
    let ok = line.outcome != Outcome::Failed;
    record(&trades, &line)?;
    Ok(ok)
}

fn main() {
    let result = Args::parse(std::env::args().skip(1), &["dry-run"]).and_then(|a| main_run(&a));
    match result {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            tracing::error!(error = %e, "failed");
            let _ = writeln!(std::io::stderr(), "error: {e}");
            std::process::exit(1);
        }
    }
}
