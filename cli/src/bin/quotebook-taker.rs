//! quotebook taker CLI over RPC.
//!
//! ```text
//! quotebook-taker discover --url <RPC> --program-id <ID> [--allow <MAKER>[,<MAKER>…]]
//! quotebook-taker quote|settle --url <RPC> --program-id <ID>
//!   (--maker <wss://…> | --discover) [--allow <MAKER>[,<MAKER>…]]
//!   --series <ADDRESS> --side buy|sell --size <LOTS> --taker <KEYPAIR>
//!   [--payer <KEYPAIR>] [--taker-base <TOKEN_ACCOUNT> --taker-quote <TOKEN_ACCOUNT>]
//! ```
//!
//! `discover` lists the maker registry (ADR 0002). `quote` requests and checks a quote
//! (signature and every rule `settle` applies); `settle` also sends it. With `--discover`, every
//! listed maker is asked, each quote must be signed by the maker of the profile its endpoint
//! came from, and the best checked price wins. `--allow` restricts the makers asked and, in
//! every mode, the signed `quote.maker` accepted. Keypair options take file paths; only public
//! keys are printed.

use quotebook_cli::args::Args;
use quotebook_cli::rpc::Cluster;
use quotebook_cli::taker::MakerFilter;
use quotebook_cli::{keys, taker, CliError};
use quotebook_core::BaseLots;
use quotebook_maker::protocol::TakerSide;
use solana_address::Address;
use solana_signer::Signer;

/// `--allow A,B,…`, if given.
fn allowlist(args: &Args) -> Result<Option<Vec<Address>>, CliError> {
    args.opt("allow")
        .map(|list| {
            list.split(',')
                .map(|m| {
                    m.trim()
                        .parse::<Address>()
                        .map_err(|_| CliError::Args(format!("--allow: invalid address {m:?}")))
                })
                .collect()
        })
        .transpose()
}

fn run() -> Result<(), CliError> {
    let args = Args::parse(std::env::args().skip(1), &["discover"])?;
    let command = args
        .positional(0)
        .ok_or_else(|| CliError::Args("missing command: discover, quote or settle".into()))?
        .to_owned();
    let program_id: Address = args.parse_req("program-id")?;
    let allow = allowlist(&args)?;
    if command == "discover" {
        let mut rpc = Cluster::new(args.req("url")?);
        let registry = taker::discover(&mut rpc, &program_id, allow.as_deref())?;
        // Profile text is validated printable ASCII (no control bytes) and public on chain.
        for l in &registry.listings {
            println!(
                "maker {} endpoint {} name {:?}",
                l.maker, l.endpoint, l.name
            );
        }
        for a in &registry.invalid {
            println!("invalid profile {a}");
        }
        println!(
            "{} listed, {} invalid, {} not allowed",
            registry.listings.len(),
            registry.invalid.len(),
            registry.not_allowed
        );
        return Ok(());
    }
    if command != "quote" && command != "settle" {
        return Err(CliError::Args(format!("unknown command {command:?}")));
    }
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

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Rpc(e.to_string()))?;
    let mut rpc = Cluster::new(args.req("url")?);
    let answer = match (args.flag("discover"), args.opt("maker")) {
        (true, None) => {
            let registry = taker::discover(&mut rpc, &program_id, allow.as_deref())?;
            let endpoints: Vec<(Address, String)> = registry
                .listings
                .iter()
                .map(|l| (l.maker, l.endpoint.clone()))
                .collect();
            let asked = runtime.block_on(taker::ask_each(&endpoints, &request));
            let choice = taker::choose(
                &mut rpc,
                &program_id,
                &series,
                asked,
                &taker_key.pubkey(),
                side,
                size,
                allow.as_deref(),
            );
            for (maker, e) in &choice.rejected {
                eprintln!("rejected {maker}: {e}");
            }
            let (_, answer, _) = choice
                .best
                .ok_or_else(|| CliError::Args("no listed maker gave a valid quote".into()))?;
            answer
        }
        (false, Some(url)) => {
            let answer = runtime.block_on(taker::ask(url, &request))?;
            let filter = MakerFilter {
                expected: None,
                allow: allow.as_deref(),
            };
            taker::check(
                &mut rpc,
                &program_id,
                &series,
                &answer,
                &taker_key.pubkey(),
                side,
                size,
                &filter,
            )?;
            answer
        }
        _ => {
            return Err(CliError::Args(
                "give exactly one of --maker or --discover".into(),
            ))
        }
    };
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
