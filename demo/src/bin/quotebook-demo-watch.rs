//! The demo maker's watcher on devnet (`docs/PLAN-phase2.md`, D4).
//!
//! ```text
//! quotebook-demo-watch --url <RPC> --config <DEMO_JSON> --settings <MAKER_JSON>
//!   --maker-address <ADDRESS> --payer <ADDRESS> [--taker <ADDRESS>]
//!   --endpoint ws://127.0.0.1:<PORT> --halt-file <PATH> [--taker-log <PATH>]
//!   [--out <PATH>] [--log-file <PATH>] [--interval <SECS>] [--realert <SECS>]
//!   [--min-balance <LAMPORTS>] [--min-usdc <ATOMS>] [--min-long <TOKENS>]
//!   [--min-taker-usdc <ATOMS>] [--usdc-approval <ATOMS>] [--long-approval <TOKENS>]
//! ```
//!
//! Each pass ([`quotebook_demo::watch::pass`]) writes one `pass` JSON line, an `expected` line per
//! feed and listed closure, a `resolved` line for each alert that cleared and an `alert` line for
//! each alert due, to `--out` (rotated at 10 MB, 5 kept) or stdout. For each alert due it runs
//! `QUOTEBOOK_WATCH_ALERT_CMD`, if set, with `sh -c`, the alert in `QUOTEBOOK_ALERT_KIND`,
//! `QUOTEBOOK_ALERT_SUBJECT` and `QUOTEBOOK_ALERT_MESSAGE`; an alert that stays active is
//! delivered again after `--realert` seconds (default 3,600). Nothing goes on any bus.
//!
//! It holds no key: every account is an address and it signs nothing. It reads the chain at
//! `finalized`, the maker's `Status` (2 s timeout, no nonce taken), the halt file and the
//! taker's log. The RPC URL is never printed. Without `--interval` it makes one pass.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use quotebook_cli::args::Args;
use quotebook_cli::rpc::Cluster;
use quotebook_cli::CliError;
use quotebook_demo::config::DemoConfig;
use quotebook_demo::host::HostSettings;
use quotebook_demo::logging::{self, RotatingFile, KEEP_LOGS, MAX_LOG_BYTES};
use quotebook_demo::watch::{
    self, AlertBook, Limits, LineOut, WatchState, Watched, WsStatus, REALERT_SECS,
};
use serde_json::{json, Value};

fn emit(lines: &mut LineOut, value: &Value) {
    if let Err(e) = lines.line(value) {
        tracing::warn!(error = %e, "status line not written");
    }
}

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

fn watched(args: &Args) -> Result<Watched, CliError> {
    let demo = DemoConfig::from_json(&read(args.req("config")?)?)
        .map_err(|e| CliError::Args(format!("--config: {e}")))?;
    let settings = HostSettings::from_json(&read(args.req("settings")?)?)
        .map_err(|e| CliError::Args(format!("--settings: {e}")))?;
    let d = Limits::default();
    let limits = Limits {
        min_balance: args.parse_or("min-balance", d.min_balance)?,
        min_usdc: args.parse_or("min-usdc", d.min_usdc)?,
        min_long: args.parse_or("min-long", d.min_long)?,
        min_taker_usdc: args.parse_or("min-taker-usdc", d.min_taker_usdc)?,
        usdc_approval: args.parse_or("usdc-approval", d.usdc_approval)?,
        long_approval: args.parse_or("long-approval", d.long_approval)?,
        max_snapshot_age: settings.max_snapshot_age.get(),
    };
    Ok(Watched {
        demo,
        maker: args.parse_req("maker-address")?,
        payer: args.parse_req("payer")?,
        taker: args
            .opt("taker")
            .map(str::parse)
            .transpose()
            .map_err(|_| CliError::Args("--taker: not an address".into()))?,
        halt_file: PathBuf::from(args.req("halt-file")?),
        taker_log: args.opt("taker-log").map(PathBuf::from),
        limits,
    })
}

fn run(args: &Args) -> Result<(), CliError> {
    let log_file = args.opt("log-file").map(PathBuf::from);
    logging::init(log_file.as_deref()).map_err(|e| CliError::Args(format!("--log-file: {e}")))?;
    let endpoint = args.req("endpoint")?;
    let loopback = ["ws://127.0.0.1:", "ws://localhost:", "ws://[::1]:"];
    if !loopback.iter().any(|p| endpoint.starts_with(p)) {
        return Err(CliError::Args(
            "--endpoint must be the maker's loopback ws:// address".into(),
        ));
    }
    let watched = watched(args)?;
    let interval = args
        .opt("interval")
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| CliError::Args(format!("--interval: cannot parse {v:?}")))
        })
        .transpose()?;
    let realert: i64 = args.parse_or("realert", REALERT_SECS)?;
    let mut lines = match args.opt("out") {
        Some(path) => LineOut::File(
            RotatingFile::open(Path::new(path), MAX_LOG_BYTES, KEEP_LOGS)
                .map_err(|e| CliError::Args(format!("--out: {e}")))?,
        ),
        None => LineOut::Stdout,
    };
    let alert_cmd = std::env::var("QUOTEBOOK_WATCH_ALERT_CMD").ok();
    let mut rpc = Cluster::new(args.req("url")?);
    let mut status =
        WsStatus::new(endpoint).map_err(|e| CliError::Maker(format!("runtime: {e}")))?;
    let mut state = WatchState::default();
    let mut book = AlertBook::default();
    tracing::info!(
        maker = %watched.maker,
        payer = %watched.payer,
        interval = interval.unwrap_or(0),
        "watching; read only, no key"
    );
    loop {
        let pass = watch::pass(&mut rpc, &mut status, &watched, &mut state, host_now());
        for l in &pass.lines {
            emit(&mut lines, l);
        }
        let (due, resolved) = book.update(&pass.alerts, &pass.observed, pass.now, realert);
        for (kind, subject) in resolved {
            emit(
                &mut lines,
                &json!({"resolved": kind, "subject": subject, "now": pass.now}),
            );
        }
        for a in &due {
            emit(
                &mut lines,
                &json!({
                    "alert": a.kind,
                    "subject": a.subject,
                    "message": a.message,
                    "now": pass.now,
                }),
            );
            if watch::deliver(alert_cmd.as_deref(), a) {
                book.delivered(a, pass.now);
            }
        }
        match interval {
            Some(s) => std::thread::sleep(Duration::from_secs(s)),
            None => return Ok(()),
        }
    }
}

fn main() {
    let result = Args::parse(std::env::args().skip(1), &[]).and_then(|args| run(&args));
    if let Err(e) = result {
        tracing::error!(error = %e, "failed");
        let _ = writeln!(std::io::stderr(), "error: {e}");
        std::process::exit(1);
    }
}
