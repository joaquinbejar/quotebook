//! quotebook maker service over RPC.
//!
//! ```text
//! quotebook-maker --url <RPC> --program-id <ID> --series <ADDRESS> --maker <KEYPAIR>
//!   --maker-base <TOKEN_ACCOUNT> --maker-quote <TOKEN_ACCOUNT> --listen <HOST:PORT>
//!   --mid <PRICE> [--ttl 30] [--max-size 1000] [--risk-aversion 0.1] [--volatility 0.6]
//!   [--horizon-ms 3600000] [--order-intensity 1.5] [--max-deviation 0.05]
//!   [--max-ttl-any-series <SECONDS>]
//! ```
//!
//! Every start resumes fail-closed: no quotes for `max_ttl_any_series + 10 s` (70 s at 60 s),
//! then a refresh from chain. The key is read from the file and never logged.

use std::str::FromStr;
use std::time::Duration;

use quotebook_cli::args::Args;
use quotebook_cli::maker::{prepare, refresh, ClusterClock, MakerSettings};
use quotebook_cli::rpc::Cluster;
use quotebook_cli::{keys, CliError};
use quotebook_core::{BaseLots, Seconds};
use quotebook_maker::pricing::PricingParams;
use rust_decimal::Decimal;

fn decimal(args: &Args, name: &str, default: &str) -> Result<Decimal, CliError> {
    let v = args.opt(name).unwrap_or(default);
    Decimal::from_str(v).map_err(|_| CliError::Args(format!("--{name}: cannot parse {v:?}")))
}

fn run() -> Result<(), CliError> {
    tracing_subscriber::fmt().with_target(false).init();
    let args = Args::parse(std::env::args().skip(1), &[])?;
    let url = args.req("url")?.to_owned();
    let key = keys::load(args.req("maker")?)?;
    let ttl = Seconds::new(args.parse_or("ttl", 30i64)?);
    let settings = MakerSettings {
        program_id: args.parse_req("program-id")?,
        series: args.parse_req("series")?,
        maker_base: args.parse_req("maker-base")?,
        maker_quote: args.parse_req("maker-quote")?,
        mid: decimal(&args, "mid", args.req("mid")?)?,
        pricing: PricingParams {
            risk_aversion: decimal(&args, "risk-aversion", "0.1")?,
            volatility: decimal(&args, "volatility", "0.6")?,
            time_to_terminal_ms: args.parse_or("horizon-ms", 3_600_000u64)?,
            order_intensity: decimal(&args, "order-intensity", "1.5")?,
            max_deviation: decimal(&args, "max-deviation", "0.05")?,
        },
        ttl,
        max_size: BaseLots::new(args.parse_or("max-size", 1_000u64)?),
        max_ttl_any_series: Seconds::new(args.parse_or("max-ttl-any-series", 0i64)?),
    };
    let mut rpc = Cluster::new(&url);
    let clock = ClusterClock::default();
    clock.sync(&mut rpc)?;
    let mut settings = settings;
    if settings.max_ttl_any_series.get() == 0 {
        // Default: the series' own maximum.
        let market =
            quotebook_cli::market::market(&mut rpc, &settings.program_id, &settings.series)?;
        settings.max_ttl_any_series = market.max_quote_ttl;
    }
    let prepared = prepare(&mut rpc, &settings, key, Box::new(clock.clone()))?;
    tracing::info!(
        maker = %prepared.service.maker(),
        cold_start_until = prepared.cold_start_until.get(),
        "resumed; quoting after the cold start"
    );

    // Background: keep the clock in sync and refresh the issuer once the cold start ends.
    let service = std::sync::Arc::clone(&prepared.service);
    let until = prepared.cold_start_until;
    std::thread::spawn(move || {
        let prepared = quotebook_cli::maker::Prepared {
            service,
            cold_start_until: until,
        };
        let mut rpc = Cluster::new(&url);
        let mut refreshed = false;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if let Err(e) = clock.sync(&mut rpc) {
                tracing::warn!(error = %e, "clock sync failed");
                continue;
            }
            if !refreshed {
                match refresh(&mut rpc, &prepared) {
                    Ok(true) => {
                        refreshed = true;
                        tracing::info!("cold start over; quoting");
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e, "refresh failed; retrying"),
                }
            }
        }
    });

    let listen = args.req("listen")?.to_owned();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Maker(e.to_string()))?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&listen)
            .await
            .map_err(|e| CliError::Maker(format!("bind {listen}: {e}")))?;
        tracing::info!(%listen, "serving");
        prepared
            .service
            .serve(listener)
            .await
            .map_err(|e| CliError::Maker(e.to_string()))
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
