//! quotebook taker CLI over RPC.
//!
//! ```text
//! quotebook-taker quote|settle --url <RPC> --program-id <ID> --maker <ws://HOST:PORT>
//!   --series <ADDRESS> --side buy|sell --size <LOTS> --taker <KEYPAIR>
//!   [--payer <KEYPAIR>] [--taker-base <TOKEN_ACCOUNT> --taker-quote <TOKEN_ACCOUNT>]
//! ```
//!
//! `quote` requests and checks a quote (signature and every rule `settle` applies); `settle`
//! also sends it. Keypair options take file paths; only public keys are printed.

use quotebook_cli::args::Args;
use quotebook_cli::rpc::Cluster;
use quotebook_cli::{keys, taker, CliError};
use quotebook_core::BaseLots;
use quotebook_maker::protocol::TakerSide;
use solana_address::Address;
use solana_signer::Signer;

fn run() -> Result<(), CliError> {
    let args = Args::parse(std::env::args().skip(1), &[])?;
    let command = args
        .positional(0)
        .ok_or_else(|| CliError::Args("missing command: quote or settle".into()))?
        .to_owned();
    let program_id: Address = args.parse_req("program-id")?;
    let series: Address = args.parse_req("series")?;
    let side = match args.req("side")? {
        "buy" => TakerSide::Buy,
        "sell" => TakerSide::Sell,
        other => {
            return Err(CliError::Args(format!(
                "--side must be buy or sell, not {other:?}"
            )))
        }
    };
    let size = BaseLots::new(args.parse_req("size")?);
    let taker_key = keys::load(args.req("taker")?)?;
    let request = taker::request(1, &series, &taker_key.pubkey(), side, size);

    let answer = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::Rpc(e.to_string()))?;
        runtime.block_on(taker::ask(args.req("maker")?, &request))?
    };
    let mut rpc = Cluster::new(args.req("url")?);
    taker::check(
        &mut rpc,
        &program_id,
        &series,
        &answer,
        &taker_key.pubkey(),
        side,
        size,
    )?;
    let t = answer.signed.quote.terms();
    println!("maker {}", t.maker);
    println!("side {:?}", t.side);
    println!("price_ticks {}", t.price.get());
    println!("size {}", t.size.get());
    println!("nonce {}", t.nonce.get());
    println!("expires_at {}", t.expires_at.get());
    if command == "quote" {
        return Ok(());
    }
    if command != "settle" {
        return Err(CliError::Args(format!("unknown command {command:?}")));
    }
    let payer = match args.opt("payer") {
        Some(path) => keys::load(path)?,
        None => taker_key.insecure_clone(),
    };
    let signature = taker::settle(
        &mut rpc,
        &program_id,
        &answer,
        &taker_key,
        &payer,
        &args.parse_req("taker-base")?,
        &args.parse_req("taker-quote")?,
        size,
    )?;
    println!("signature {signature}");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
