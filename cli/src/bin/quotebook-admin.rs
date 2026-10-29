//! quotebook admin CLI.
//!
//! ```text
//! quotebook-admin <command> --url <RPC> --program-id <ID> --payer <KEYPAIR> [options]
//!   init-config   --admin <KEYPAIR>      (the program's upgrade authority)
//!   create-series --admin <KEYPAIR> --base <MINT> --quote <MINT> --base-lot <ATOMS>
//!                 --quote-lot <ATOMS> --max-ttl <SECONDS> --min-size <LOTS>
//!   pause | resume --admin <KEYPAIR> --series <ADDRESS>
//!   set-admin     --admin <KEYPAIR> --new-admin <ADDRESS>
//!   init-maker    --maker <KEYPAIR>
//!   approve       --maker <KEYPAIR> --account <TOKEN_ACCOUNT> --amount <ATOMS>
//!   cancel-all    --maker <KEYPAIR> --new-min <NONCE>
//!   cancel-nonce  --maker <KEYPAIR> --nonce <NONCE>
//!   show          (the config)
//!   show-series   --series <ADDRESS>
//! ```
//!
//! Keypair options take file paths; only public keys are printed.

use quotebook_cli::args::Args;
use quotebook_cli::rpc::Cluster;
use quotebook_cli::{admin, keys, market, CliError};
use quotebook_core::instruction::CreateSeriesArgs;
use solana_address::Address;
use solana_signer::Signer;

fn address(args: &Args, name: &str) -> Result<Address, CliError> {
    args.parse_req(name)
}

fn run() -> Result<(), CliError> {
    let args = Args::parse(std::env::args().skip(1), &[])?;
    let command = args
        .positional(0)
        .ok_or_else(|| CliError::Args("missing command".into()))?
        .to_owned();
    let mut rpc = Cluster::new(args.req("url")?);
    let program_id = address(&args, "program-id")?;
    if command == "show" {
        let c = market::config(&mut rpc, &program_id)?;
        println!("admin {}", c.admin);
        println!("series_count {}", c.series_count);
        println!("deployment_id {:02x?}", c.deployment_id);
        return Ok(());
    }
    if command == "show-series" {
        let s = market::series(&mut rpc, &program_id, &address(&args, "series")?)?;
        println!("base_mint {}", s.base_mint);
        println!("quote_mint {}", s.quote_mint);
        println!("base_lot_size {}", s.base_lot_size.get());
        println!("quote_lot_size {}", s.quote_lot_size.get());
        println!("max_quote_ttl {}", s.max_quote_ttl.get());
        println!("min_size {}", s.min_size.get());
        println!("paused {}", s.paused);
        return Ok(());
    }
    let payer = keys::load(args.req("payer")?)?;
    let signature = match command.as_str() {
        "init-config" => {
            let admin_key = keys::load(args.req("admin")?)?;
            let (deployment, sig) = admin::init_config(&mut rpc, &program_id, &admin_key, &payer)?;
            println!("admin {}", admin_key.pubkey());
            println!("deployment_id {deployment:02x?}");
            sig
        }
        "create-series" => {
            let admin_key = keys::load(args.req("admin")?)?;
            let series_args = CreateSeriesArgs {
                base_lot_size: args.parse_req("base-lot")?,
                quote_lot_size: args.parse_req("quote-lot")?,
                max_quote_ttl: args.parse_req("max-ttl")?,
                min_size: args.parse_req("min-size")?,
            };
            let (series, sig) = admin::create_series(
                &mut rpc,
                &program_id,
                &admin_key,
                &payer,
                &address(&args, "base")?,
                &address(&args, "quote")?,
                &series_args,
            )?;
            println!("series {series}");
            sig
        }
        "pause" | "resume" => {
            let admin_key = keys::load(args.req("admin")?)?;
            admin::set_paused(
                &mut rpc,
                &program_id,
                &admin_key,
                &payer,
                &address(&args, "series")?,
                command == "pause",
            )?
        }
        "set-admin" => {
            let admin_key = keys::load(args.req("admin")?)?;
            admin::set_admin(
                &mut rpc,
                &program_id,
                &admin_key,
                &payer,
                &address(&args, "new-admin")?,
            )?
        }
        "init-maker" => {
            let maker = keys::load(args.req("maker")?)?;
            println!("maker {}", maker.pubkey());
            admin::init_maker(&mut rpc, &program_id, &maker, &payer)?
        }
        "approve" => {
            let maker = keys::load(args.req("maker")?)?;
            admin::approve(
                &mut rpc,
                &program_id,
                &maker,
                &payer,
                &address(&args, "account")?,
                args.parse_req("amount")?,
            )?
        }
        "cancel-all" => {
            let maker = keys::load(args.req("maker")?)?;
            let new_min = quotebook_core::Nonce::new(args.parse_req("new-min")?);
            admin::cancel_all(&mut rpc, &program_id, &maker, &payer, new_min)?
        }
        "cancel-nonce" => {
            let maker = keys::load(args.req("maker")?)?;
            let nonce = quotebook_core::Nonce::new(args.parse_req("nonce")?);
            admin::cancel_nonce(&mut rpc, &program_id, &maker, &payer, nonce)?
        }
        other => return Err(CliError::Args(format!("unknown command {other:?}"))),
    };
    println!("signature {signature}");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
