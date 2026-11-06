//! The hosted demo maker on devnet (`docs/PLAN-phase2.md`, D3).
//!
//! ```text
//! quotebook-demo-maker run --url <RPC> --config <DEMO_JSON> --settings <MAKER_JSON>
//!   --maker <KEYPAIR> --listen 127.0.0.1:<PORT> --halt-file <PATH> [--log-file <PATH>]
//! quotebook-demo-maker status --endpoint ws://127.0.0.1:<PORT>
//! quotebook-demo-maker drain  --endpoint ws://127.0.0.1:<PORT> --url <RPC> [--timeout-secs 300]
//! quotebook-demo-maker revoke --url <RPC> --config <DEMO_JSON> --maker <KEYPAIR> --payer <KEYPAIR>
//!   (--endpoint ws://127.0.0.1:<PORT> | --new-min <NONCE>) [--timeout-secs 180]
//! quotebook-demo-maker accounts --url <RPC> --config <DEMO_JSON> --maker-address <ADDRESS>
//! ```
//!
//! `run` serves D1's multi-series service over D2's marks, on a loopback address only. It
//! resumes fail-closed on every start: no quote until the cold start has passed on a finalized
//! Clock. `status` prints the service's state as one JSON line. `drain` waits, on a halted
//! service, until a finalized Clock reaches its newest expiry. `revoke` is the emergency path:
//! `cancel_all` up to the next nonce, a token `Revoke` on every maker account, both confirmed at
//! `finalized`. `accounts` prints the token accounts the maker uses.
//!
//! Keypair options take file paths and only public keys are printed. Logs are JSON lines; they
//! never carry keys or RPC URLs.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quotebook_cli::args::Args;
use quotebook_cli::maker::ClusterClock;
use quotebook_cli::rpc::{Cluster, Rpc};
use quotebook_cli::{admin, keys, CliError};
use quotebook_demo::chain::RpcChainView;
use quotebook_demo::config::DemoConfig;
use quotebook_demo::feed::Board;
use quotebook_demo::host::{self, HostSettings};
use quotebook_demo::ops::{self, Drain};
use quotebook_demo::{logging, reader};
use quotebook_maker::client;
use quotebook_maker::risk::ChainView;
use quotebook_maker::server::{Start, TimeSource};
use serde_json::json;
use solana_signer::Signer;

fn read(path: &str) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|e| CliError::Args(format!("{path}: {e}")))
}

fn demo_config(args: &Args) -> Result<DemoConfig, CliError> {
    DemoConfig::from_json(&read(args.req("config")?)?)
        .map_err(|e| CliError::Args(format!("--config: {e}")))
}

fn out(value: &serde_json::Value) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}").map_err(|e| CliError::Maker(e.to_string()))
}

fn runtime() -> Result<tokio::runtime::Runtime, CliError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Maker(e.to_string()))
}

fn status(endpoint: &str) -> Result<quotebook_maker::protocol::StatusMessage, CliError> {
    let id = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0);
    runtime()?
        .block_on(client::request_status(endpoint, id))
        .map_err(CliError::Quote)
}

fn deadline(args: &Args, default_secs: u64) -> Result<Instant, CliError> {
    let secs = args.parse_or("timeout-secs", default_secs)?;
    Instant::now()
        .checked_add(Duration::from_secs(secs))
        .ok_or_else(|| CliError::Args("--timeout-secs: too large".into()))
}

fn run(args: &Args) -> Result<(), CliError> {
    let log_file = args.opt("log-file").map(PathBuf::from);
    logging::init(log_file.as_deref()).map_err(|e| CliError::Args(format!("--log-file: {e}")))?;
    let url = args.req("url")?.to_owned();
    let demo = demo_config(args)?;
    let settings = HostSettings::from_json(&read(args.req("settings")?)?)
        .map_err(|e| CliError::Args(format!("--settings: {e}")))?;
    let listen: SocketAddr = args.parse_req("listen")?;
    if !listen.ip().is_loopback() {
        return Err(CliError::Args(
            "--listen must be a loopback address: the demo has no public endpoint".into(),
        ));
    }
    let halt_file = PathBuf::from(args.req("halt-file")?);
    let key = keys::load(args.req("maker")?)?;
    let maker = key.pubkey();

    let mut rpc = Cluster::new(&url);
    let clock = ClusterClock::default();
    clock.sync(&mut rpc)?;
    let plan = host::plan(&mut rpc, &demo, maker)?;
    let view = Arc::new(
        RpcChainView::new(
            Cluster::new(&url),
            demo.quotebook_program,
            maker,
            plan.watched(demo.usdc_mint),
        )
        .map_err(|e| CliError::Maker(e.0))?,
    );
    // The window to resume from, at finalized; started_at is now, after every quote of any
    // earlier process was stamped.
    let first = view.snapshot().map_err(|e| CliError::Rpc(e.0))?;
    let started_at = clock.now();
    let board = Arc::new(Board::new(&demo, Box::new(clock.clone())));
    reader::poll_once(&mut rpc, &demo, &board);
    reader::spawn_reader(
        Cluster::new(&url),
        demo.clone(),
        Arc::clone(&board),
        reader::READ_INTERVAL,
    )
    .map_err(|e| CliError::Maker(e.to_string()))?;
    let service = Arc::new(host::service(
        key,
        &plan,
        &settings,
        &board,
        &view.balances(),
        Start::Resume {
            window: Box::new(first.window),
            started_at,
        },
        halt_file.clone(),
        Box::new(clock.clone()),
    )?);
    tracing::info!(
        maker = %maker,
        markets = plan.markets.len(),
        started_at = started_at.get(),
        longest_ttl = plan.longest_ttl().map_or(0, |t| t.get()),
        halt_file = %halt_file.display(),
        %listen,
        "resumed; quoting after the cold start on a finalized Clock"
    );

    let background = Arc::clone(&service);
    let interval = Duration::from_secs(settings.reconcile_secs);
    std::thread::Builder::new()
        .name("demo-reconcile".into())
        .spawn(move || {
            let mut rpc = rpc;
            loop {
                if let Err(e) = clock.sync(&mut rpc) {
                    tracing::warn!(error = %e, "clock sync failed");
                }
                if let Err(e) = background.reconcile(view.as_ref()) {
                    tracing::warn!(error = %e, "snapshot refused: quoting stops until one is accepted");
                }
                std::thread::sleep(interval);
            }
        })
        .map_err(|e| CliError::Maker(e.to_string()))?;

    runtime()?.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| CliError::Maker(format!("bind {listen}: {e}")))?;
        tracing::info!(%listen, "serving");
        service
            .serve(listener)
            .await
            .map_err(|e| CliError::Maker(e.to_string()))
    })
}

fn drain(args: &Args) -> Result<(), CliError> {
    let endpoint = args.req("endpoint")?;
    let mut rpc = Cluster::new(args.req("url")?);
    let deadline = deadline(args, 300)?;
    loop {
        let state = status(endpoint)?;
        let (_, clock) = ops::finalized_clock(&mut rpc)?;
        match ops::drain(&state, clock)? {
            Drain::Done => {
                return out(&json!({
                    "drained": true,
                    "finalized_clock": clock.get(),
                    "newest_expires_at": state.newest_expires_at,
                    "open_quotes": state.open_quotes,
                }));
            }
            Drain::Waiting { until } => {
                if Instant::now() >= deadline {
                    return Err(CliError::Maker(format!(
                        "not drained: finalized clock {} below {until}",
                        clock.get()
                    )));
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

fn revoke(args: &Args) -> Result<(), CliError> {
    let url = args.req("url")?;
    let demo = demo_config(args)?;
    let maker = keys::load(args.req("maker")?)?;
    let payer = keys::load(args.req("payer")?)?;
    let next_nonce = match (args.opt("endpoint"), args.opt("new-min")) {
        (_, Some(n)) => n
            .parse::<u64>()
            .map_err(|_| CliError::Args("--new-min: not a nonce".into()))?,
        (Some(endpoint), None) => {
            let state = status(endpoint)?;
            if !state.halted {
                return Err(CliError::Maker(
                    "the service is not halted: create its halt file first".into(),
                ));
            }
            state.next_nonce
        }
        (None, None) => return Err(CliError::Args("--endpoint or --new-min".into())),
    };
    let mut rpc = Cluster::new(url);
    let plan = host::plan(&mut rpc, &demo, maker.pubkey())?;
    let view = RpcChainView::new(
        Cluster::new(url),
        demo.quotebook_program,
        maker.pubkey(),
        plan.watched(demo.usdc_mint),
    )
    .map_err(|e| CliError::Maker(e.0))?;
    let before = view.snapshot().map_err(|e| CliError::Rpc(e.0))?;
    let new_min = ops::cancel_minimum(next_nonce, &before);
    let mut sent = Vec::new();
    if let Some(n) = new_min {
        let sig = admin::cancel_all(&mut rpc, &demo.quotebook_program, &maker, &payer, n)?;
        tracing::info!(new_min = n.get(), signature = %sig, "cancel_all sent");
        sent.push(json!({"cancel_all": sig.to_string(), "new_min": n.get()}));
    }
    for (account, _) in &before.accounts {
        if rpc.account(account)?.is_none() {
            continue;
        }
        let sig = admin::revoke(&mut rpc, &maker, &payer, account)?;
        tracing::info!(account = %account, signature = %sig, "revoke sent");
        sent.push(json!({"revoke": sig.to_string(), "account": account.to_string()}));
    }
    let deadline = deadline(args, 180)?;
    loop {
        let snapshot = view.snapshot().map_err(|e| CliError::Rpc(e.0))?;
        if ops::revoke_confirmed(&snapshot, new_min) {
            return out(&json!({
                "revoked": true,
                "finalized_slot": snapshot.slot,
                "window_base": snapshot.window.window_base().get(),
                "sent": sent,
            }));
        }
        if Instant::now() >= deadline {
            return Err(CliError::Maker(
                "revoke not confirmed at finalized before the timeout".into(),
            ));
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn accounts(args: &Args) -> Result<(), CliError> {
    let demo = demo_config(args)?;
    let maker = args.parse_req("maker-address")?;
    let mut rpc = Cluster::new(args.req("url")?);
    let plan = host::plan(&mut rpc, &demo, maker)?;
    let markets: Vec<_> = plan
        .markets
        .iter()
        .map(|m| {
            json!({
                "quotebook_series": m.pair.quotebook_series.to_string(),
                "metalbook_series": m.pair.metalbook_series.to_string(),
                "base_mint": m.base_mint.to_string(),
                "maker_base": m.maker_base.to_string(),
            })
        })
        .collect();
    out(&json!({
        "maker": maker.to_string(),
        "maker_state": quotebook_sdk::pda::maker_state(&demo.quotebook_program, &maker).0.to_string(),
        "maker_quote": plan.maker_quote.to_string(),
        "markets": markets,
    }))
}

fn main() {
    let result =
        Args::parse(std::env::args().skip(1), &[]).and_then(|args| match args.positional(0) {
            Some("run") => run(&args),
            Some("status") => status(args.req("endpoint")?).and_then(|s| {
                out(&serde_json::to_value(s).map_err(|e| CliError::Maker(e.to_string()))?)
            }),
            Some("drain") => drain(&args),
            Some("revoke") => revoke(&args),
            Some("accounts") => accounts(&args),
            _ => Err(CliError::Args(
                "command: run, status, drain, revoke or accounts".into(),
            )),
        });
    if let Err(e) = result {
        tracing::error!(error = %e, "failed");
        let _ = writeln!(std::io::stderr(), "error: {e}");
        std::process::exit(1);
    }
}
