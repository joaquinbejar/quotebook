//! The demo maker's writer cycle on devnet (`docs/PLAN-phase2.md`, D5).
//!
//! ```text
//! quotebook-demo-roll plan  --url <RPC> --config <DEMO_JSON> --settings <MAKER_JSON>
//!   --maker-address <ADDRESS>
//! quotebook-demo-roll apply --url <RPC> --config <DEMO_JSON> --settings <MAKER_JSON>
//!   --maker <KEYPAIR> --payer <KEYPAIR> --admin <KEYPAIR>
//!   --endpoint ws://127.0.0.1:<PORT> --halt-file <PATH> --state-file <PATH>
//!   [--drain-timeout-secs 300] [--confirm-timeout-secs 180] [--log-file <PATH>]
//! ```
//!
//! `plan` is the dry run: every read of a cycle at `finalized`, then the plan as one JSON line on
//! stdout. It takes no key and sends nothing.
//!
//! `apply` runs one cycle ([`quotebook_demo::cycle::run_cycle`]): `create_series` if a pair needs
//! one (signed by `--admin`), then, only if anything else is due, the halt file, the drain, and
//! the mints, approvals, claims, redeems and revokes, each confirmed at `finalized`. When the
//! pairs change it rewrites `--config` (the previous one kept as `<config>.prev`). It prints one
//! JSON line: what it sent, `restart` (the maker and the watcher must restart to load the new
//! pairs) and `halt_created` (this run created the halt file, so the caller may remove it once the
//! maker has restarted). On a failure it prints the same with `error`, exits 1, and leaves the
//! halt file: the maker stays halted until someone looks.
//!
//! **Devnet only:** both commands refuse a cluster whose genesis is not devnet's. **The admin key
//! in a scheduled job is devnet only.** It signs only `create_series`, and its keypair is a file
//! path; on mainnet a scheduled job never holds an admin key.
//!
//! `--state-file` records when the last transaction was sent: `apply` refuses to start within
//! 120 s of it, so a rerun right after a failure cannot plan from a finalized view that misses a
//! transaction still landing. Keypair options take file paths and only public keys are printed.
//! Logs are JSON lines; they never carry keys or RPC URLs.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use metalbook_sdk::ix as metal_ix;
use quotebook_cli::args::Args;
use quotebook_cli::rpc::{send, Cluster};
use quotebook_cli::{admin, keys, CliError};
use quotebook_core::instruction::CreateSeriesArgs;
use quotebook_demo::chain::TOKEN_PROGRAM;
use quotebook_demo::config::DemoConfig;
use quotebook_demo::cycle::{
    self, CycleIo, Patience, Step, View, SERIES_BASE_LOT, SERIES_MAX_QUOTE_TTL, SERIES_MIN_SIZE,
    SERIES_QUOTE_LOT,
};
use quotebook_demo::host::HostSettings;
use quotebook_demo::logging;
use quotebook_demo::ops;
use quotebook_demo::watch::{StatusSource, WsStatus};
use quotebook_maker::protocol::StatusMessage;
use serde_json::json;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Seconds between polls of the drain and of each confirmation.
const POLL: Duration = Duration::from_secs(5);
/// The shortest gap between a send and the next `apply`, seconds: a blockhash's life and
/// finalization, with a margin.
const RESEND_GUARD_SECS: i64 = 120;

fn read(path: &str) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|e| CliError::Args(format!("{path}: {e}")))
}

fn out(value: &serde_json::Value) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}").map_err(|e| CliError::Maker(e.to_string()))
}

fn host_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn inputs(args: &Args) -> Result<(DemoConfig, u64, Cluster), CliError> {
    let demo = DemoConfig::from_json(&read(args.req("config")?)?)
        .map_err(|e| CliError::Args(format!("--config: {e}")))?;
    let settings = HostSettings::from_json(&read(args.req("settings")?)?)
        .map_err(|e| CliError::Args(format!("--settings: {e}")))?;
    let mut rpc = Cluster::new(args.req("url")?);
    cycle::require_devnet(&mut rpc)?;
    Ok((demo, settings.limits.max_long, rpc))
}

fn plan(args: &Args) -> Result<(), CliError> {
    let (demo, max_long, mut rpc) = inputs(args)?;
    let maker: Address = args.parse_req("maker-address")?;
    let view = cycle::read_view(&mut rpc, &demo, &maker)?;
    let exposure = view.exposure()?;
    let planned = cycle::plan(&view, &demo, max_long)?;
    out(&json!({
        "dry_run": true,
        "slot": view.slot,
        "finalized_clock": view.now,
        "spot_atoms": view.spot,
        "maker": maker.to_string(),
        "usdc": view.usdc.map(|t| json!({"amount": t.amount, "delegated": t.delegated_amount})),
        "exposure": {"contracts": exposure.contracts, "committed": exposure.committed, "positions": exposure.positions},
        "eligible": view.metal.iter().filter(|m| m.eligible(&demo, view.now)).map(|m| json!({
            "series": m.address.to_string(),
            "strike": m.strike,
            "cap": m.cap,
            "expiry": m.expiry,
            "max_payout": m.max_payout,
        })).collect::<Vec<_>>(),
        "needs_halt": !planned.halted().is_empty(),
        "plan": planned.to_json(),
    }))
}

/// The real world of [`cycle::run_cycle`]: devnet, the maker's `Status`, the halt file and the
/// keys.
struct Chain {
    rpc: Cluster,
    demo: DemoConfig,
    maker: Keypair,
    payer: Keypair,
    admin_path: String,
    status: WsStatus,
    halt_file: PathBuf,
    state_file: PathBuf,
}

impl Chain {
    fn record_send(&self) -> Result<(), CliError> {
        std::fs::write(&self.state_file, format!("{}\n", host_now()))
            .map_err(|e| CliError::Args(format!("--state-file: {e}")))
    }
}

impl CycleIo for Chain {
    fn read(&mut self) -> Result<View, CliError> {
        cycle::read_view(&mut self.rpc, &self.demo, &self.maker.pubkey())
    }

    fn status(&mut self) -> Result<StatusMessage, String> {
        self.status.status()
    }

    fn finalized_clock(&mut self) -> Result<i64, CliError> {
        ops::finalized_clock(&mut self.rpc).map(|(_, clock)| clock.get())
    }

    fn halt(&mut self) -> Result<bool, CliError> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.halt_file)
        {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(CliError::Args(format!("--halt-file: {e}"))),
        }
    }

    fn send(&mut self, step: &Step, view: &View) -> Result<String, CliError> {
        let metalbook = self.demo.metalbook_program;
        let quotebook = self.demo.quotebook_program;
        let usdc = self.demo.usdc_mint;
        let maker = self.maker.pubkey();
        let series_of = |metal: &Address| {
            view.metal(metal)
                .copied()
                .ok_or_else(|| CliError::Account(format!("series {metal} not in the view")))
        };
        self.record_send()?;
        let signature = match *step {
            Step::CreateSeries { long_mint, .. } => {
                let admin_key = keys::load(&self.admin_path)?;
                let args = CreateSeriesArgs {
                    base_lot_size: SERIES_BASE_LOT,
                    quote_lot_size: SERIES_QUOTE_LOT,
                    max_quote_ttl: SERIES_MAX_QUOTE_TTL,
                    min_size: SERIES_MIN_SIZE,
                    fee_bps: Some(0),
                };
                let (series, sig) = admin::create_series(
                    &mut self.rpc,
                    &quotebook,
                    &admin_key,
                    &self.payer,
                    &long_mint,
                    &usdc,
                    &args,
                )?;
                tracing::info!(admin = %admin_key.pubkey(), %series, "create_series");
                sig
            }
            Step::Claim { metal } => send(
                &mut self.rpc,
                &[metal_ix::claim_writer(&metalbook, &maker, &metal, &usdc)],
                &self.payer,
                &[&self.maker],
            )?,
            Step::Redeem { metal, contracts } => send(
                &mut self.rpc,
                &[metal_ix::redeem(
                    &metalbook, &maker, &metal, &usdc, contracts,
                )],
                &self.payer,
                &[&self.maker],
            )?,
            Step::Revoke { account } => {
                admin::revoke(&mut self.rpc, &self.maker, &self.payer, &account)?
            }
            Step::Mint { metal, contracts } => {
                let m = series_of(&metal)?;
                send(
                    &mut self.rpc,
                    &[
                        admin::create_associated_account(
                            &self.payer.pubkey(),
                            &maker,
                            &m.long_mint,
                            &TOKEN_PROGRAM,
                        ),
                        metal_ix::mint(&metalbook, &maker, &metal, &usdc, contracts),
                    ],
                    &self.payer,
                    &[&self.maker],
                )?
            }
            Step::Approve { account, amount } => admin::approve(
                &mut self.rpc,
                &quotebook,
                &self.maker,
                &self.payer,
                &account,
                amount,
            )?,
        };
        Ok(signature.to_string())
    }

    fn pause(&mut self) {
        std::thread::sleep(POLL);
    }
}

fn polls(args: &Args, name: &str, default_secs: u64) -> Result<u32, CliError> {
    let secs: u64 = args.parse_or(name, default_secs)?;
    u32::try_from(secs.div_ceil(POLL.as_secs()))
        .map_err(|_| CliError::Args(format!("--{name}: too large")))
}

/// Refuses to start within [`RESEND_GUARD_SECS`] of the last send.
fn guard(state_file: &Path) -> Result<(), CliError> {
    let last = match std::fs::read_to_string(state_file) {
        Ok(text) => text.trim().parse::<i64>().ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(CliError::Args(format!("--state-file: {e}"))),
    };
    match last {
        Some(t) if host_now().saturating_sub(t) < RESEND_GUARD_SECS => {
            Err(CliError::Args(format!(
                "a transaction was sent at {t}: wait {RESEND_GUARD_SECS} s after the last send"
            )))
        }
        _ => Ok(()),
    }
}

fn apply(args: &Args) -> Result<(), CliError> {
    let log_file = args.opt("log-file").map(PathBuf::from);
    logging::init(log_file.as_deref()).map_err(|e| CliError::Args(format!("--log-file: {e}")))?;
    let endpoint = args.req("endpoint")?;
    if !endpoint.starts_with("ws://127.0.0.1:") {
        return Err(CliError::Args(
            "--endpoint must be ws://127.0.0.1:<PORT>: the demo maker listens on loopback only"
                .into(),
        ));
    }
    let state_file = PathBuf::from(args.req("state-file")?);
    guard(&state_file)?;
    let config_path = args.req("config")?.to_owned();
    let (demo, max_long, rpc) = inputs(args)?;
    let patience = Patience {
        drain_polls: polls(args, "drain-timeout-secs", 300)?,
        confirm_polls: polls(args, "confirm-timeout-secs", 180)?,
    };
    let mut chain = Chain {
        rpc,
        demo: demo.clone(),
        maker: keys::load(args.req("maker")?)?,
        payer: keys::load(args.req("payer")?)?,
        admin_path: args.req("admin")?.to_owned(),
        status: WsStatus::new(endpoint).map_err(|e| CliError::Maker(e.to_string()))?,
        halt_file: PathBuf::from(args.req("halt-file")?),
        state_file,
    };
    tracing::info!(maker = %chain.maker.pubkey(), payer = %chain.payer.pubkey(), "cycle starts");
    match cycle::run_cycle(&mut chain, &demo, max_long, patience) {
        Ok(outcome) => {
            if outcome.config_changed {
                let old = read(&config_path)?;
                let new = cycle::with_pairs(&old, &outcome.pairs)?;
                let prev = format!("{config_path}.prev");
                let next = format!("{config_path}.next");
                std::fs::write(&prev, &old).map_err(|e| CliError::Args(format!("{prev}: {e}")))?;
                std::fs::write(&next, &new).map_err(|e| CliError::Args(format!("{next}: {e}")))?;
                std::fs::rename(&next, &config_path)
                    .map_err(|e| CliError::Args(format!("{config_path}: {e}")))?;
                tracing::info!(pairs = outcome.pairs.len(), "configuration rewritten");
            }
            tracing::info!(
                sent = outcome.sent.len(),
                restart = outcome.config_changed,
                "cycle done"
            );
            out(&json!({
                "sent": outcome.sent,
                "pairs": cycle::pairs_json(&outcome.pairs),
                "restart": outcome.config_changed,
                "halt_created": outcome.halt_created,
                "notes": outcome.notes,
            }))
        }
        Err(failure) => {
            tracing::error!(error = %failure.error, sent = failure.sent.len(), "cycle failed; the halt file stays");
            out(&json!({
                "error": failure.error.to_string(),
                "sent": failure.sent,
                "restart": false,
                "halt_created": failure.halt_created,
            }))?;
            Err(failure.error)
        }
    }
}

fn main() {
    let result =
        Args::parse(std::env::args().skip(1), &[]).and_then(|args| match args.positional(0) {
            Some("plan") => plan(&args),
            Some("apply") => apply(&args),
            _ => Err(CliError::Args("command: plan or apply".into())),
        });
    if let Err(e) = result {
        tracing::error!(error = %e, "failed");
        let _ = writeln!(std::io::stderr(), "error: {e}");
        std::process::exit(1);
    }
}
