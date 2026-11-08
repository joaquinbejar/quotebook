//! `jupiter-rfq serve --config <file> --listen 127.0.0.1:<port> --debt-ledger <file>`: the
//! webhook prototype on a loopback address. It holds no key and never signs (see the crate's
//! docs). The debt ledger must exist (`{"debts": []}` on a first start); a missing or unreadable
//! one refuses every quote.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use jupiter_rfq::config::{ConfigFile, StaticChain, StaticSpot};
use jupiter_rfq::desk::{Desk, Start, SystemClock};
use jupiter_rfq::ledger::FileLedger;

const USAGE: &str =
    "usage: jupiter-rfq serve --config <file> --listen 127.0.0.1:<port> --debt-ledger <file>";
/// How often the static wallet is re-read, so the snapshot stays fresh.
const RECONCILE_EVERY: Duration = Duration::from_secs(5);

fn args() -> Result<(String, SocketAddr, String), String> {
    let mut it = std::env::args().skip(1);
    if it.next().as_deref() != Some("serve") {
        return Err(USAGE.into());
    }
    let (mut config, mut listen, mut ledger) = (None, None, None);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--config" => config = it.next(),
            "--listen" => listen = it.next(),
            "--debt-ledger" => ledger = it.next(),
            _ => return Err(USAGE.into()),
        }
    }
    let config = config.ok_or(USAGE)?;
    let ledger = ledger.ok_or(USAGE)?;
    let listen: SocketAddr = listen
        .ok_or(USAGE)?
        .parse()
        .map_err(|_| "--listen: not an address".to_owned())?;
    // A prototype: never on a public interface.
    if !listen.ip().is_loopback() {
        return Err("--listen must be a loopback address".into());
    }
    Ok((config, listen, ledger))
}

async fn run() -> Result<(), String> {
    let (path, listen, ledger) = args()?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let loaded = ConfigFile::from_json(&text)
        .and_then(|c| c.load())
        .map_err(|e| e.to_string())?;
    let desk = Arc::new(Desk::new(
        loaded.desk.clone(),
        Box::new(StaticSpot::new(loaded.mids.clone(), Box::new(SystemClock))),
        Box::new(SystemClock),
        Box::new(FileLedger::new(ledger)),
        // Nothing was ever co-signed, so no previous run's transaction can be pending.
        Start::Fresh,
    ));
    let chain = Arc::new(StaticChain::new(
        loaded.wallet.0,
        loaded.wallet.1.clone(),
        Box::new(SystemClock),
    ));
    desk.reconcile(chain.as_ref()).map_err(|e| e.to_string())?;
    {
        let desk = Arc::clone(&desk);
        let chain = Arc::clone(&chain);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(RECONCILE_EVERY);
            loop {
                tick.tick().await;
                if let Err(e) = desk.reconcile(chain.as_ref()) {
                    tracing::warn!(error = %e, "reconcile failed");
                }
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| format!("bind {listen}: {e}"))?;
    tracing::info!(%listen, maker = %loaded.desk.maker, markets = loaded.desk.markets.len(), "jupiter-rfq prototype listening; it never signs");
    axum::serve(
        listener,
        jupiter_rfq::server::router(desk, loaded.quote_deadline),
    )
    .await
    .map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "jupiter-rfq stopped");
            std::process::ExitCode::FAILURE
        }
    }
}
