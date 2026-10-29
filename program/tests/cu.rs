//! Measures the compute units of every instruction in LiteSVM and produces
//! `docs/benchmarks.md`.
//!
//! - Every key and mint address is fixed, so PDA bump searches, and with them the CU numbers,
//!   are deterministic; the measurement runs twice in fresh VMs and must match.
//! - With `QUOTEBOOK_BENCH_OUT=<path>` the table is written there (`make bench` uses
//!   `docs/benchmarks.md`); it is always printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::disallowed_macros,
    clippy::panic_in_result_fn,
    clippy::result_large_err,
    missing_docs
)]

mod common;

use std::cell::Cell;
use std::fmt::Write as _;

thread_local! {
    /// Fees, in lamports, of the precompile-only transaction and of a settle transaction.
    static FEES: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

use common::*;
use quotebook_core::{Nonce, Side, WINDOW_BITS};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

struct Row {
    name: &'static str,
    note: &'static str,
    cu: u64,
}

fn fixed_mint(env: &mut Env, seed: u8, owner: Address, tlv: &[u8]) -> Address {
    let address = Address::new_from_array([seed; 32]);
    env.set(address, owner, mint_data(tlv));
    address
}

fn measure() -> Vec<Row> {
    let mut env = Env::new();
    // Fixed admin and payer so every run sees the same accounts.
    env.admin = Keypair::new_from_array([1; 32]);
    env.payer = Keypair::new_from_array([2; 32]);
    env.svm
        .airdrop(&env.admin.pubkey(), 10_000_000_000)
        .unwrap();
    env.svm
        .airdrop(&env.payer.pubkey(), 100_000_000_000)
        .unwrap();
    let authority = env.admin.pubkey();
    env.set_upgrade_authority(Some(authority));

    let mut rows = Vec::new();
    let cu = env.init_config().compute_units_consumed;
    rows.push(Row {
        name: "init_config",
        note: "upgrade-authority gate + config PDA search + create",
        cu,
    });

    let base = fixed_mint(&mut env, 10, token_program(), &[]);
    let quote = fixed_mint(&mut env, 11, token_program(), &[]);
    let cu = env
        .create_series_with(&base, &quote)
        .1
        .compute_units_consumed;
    rows.push(Row {
        name: "create_series",
        note: "two SPL Token mints",
        cu,
    });

    let ext = [tlv(18, &[0; 64]), tlv(19, &[0; 90])].concat();
    let base = fixed_mint(&mut env, 12, token_2022_program(), &ext);
    let quote = fixed_mint(&mut env, 13, token_2022_program(), &[]);
    let cu = env
        .create_series_with(&base, &quote)
        .1
        .compute_units_consumed;
    rows.push(Row {
        name: "create_series",
        note: "Token-2022 base with MetadataPointer + TokenMetadata",
        cu,
    });

    let maker = Keypair::new_from_array([3; 32]);
    env.svm.airdrop(&maker.pubkey(), 1_000_000_000).unwrap();
    let ix = init_maker_ix(&maker.pubkey(), &env.payer.pubkey());
    let cu = env.send(&[ix], &[&maker]).unwrap().compute_units_consumed;
    rows.push(Row {
        name: "init_maker",
        note: "maker and delegate PDA searches + create (2,120 bytes)",
        cu,
    });

    let state = maker_state_address(&maker.pubkey());
    let run = |env: &mut Env, tag: u8, value: u64| {
        let ix = cancel_ix(tag, &maker.pubkey(), &state, value);
        env.send(&[ix], &[&maker]).unwrap().compute_units_consumed
    };
    let cu = run(&mut env, tag::CANCEL_NONCE, 5);
    rows.push(Row {
        name: "cancel_nonce",
        note: "inside the window, no slide",
        cu,
    });
    let cu = run(&mut env, tag::CANCEL_NONCE, WINDOW_BITS + 63);
    rows.push(Row {
        name: "cancel_nonce",
        note: "slides 64 nonces (one word)",
        cu,
    });
    let cu = run(&mut env, tag::CANCEL_ALL, 64 + 100);
    rows.push(Row {
        name: "cancel_all",
        note: "slides 100 nonces",
        cu,
    });
    // Worst partial slide: 16,383 nonces from an unaligned base, wrapping the ring.
    let cu = run(&mut env, tag::CANCEL_ALL, 164 + WINDOW_BITS - 1);
    rows.push(Row {
        name: "cancel_all",
        note: "slides 16,383 nonces, unaligned, wrapping (worst case)",
        cu,
    });
    let cu = run(&mut env, tag::CANCEL_ALL, 10 * WINDOW_BITS);
    rows.push(Row {
        name: "cancel_all",
        note: "slides a whole window or more (clears the ring)",
        cu,
    });
    let admin = env.admin.insecure_clone();
    let (series, _) = env.create_series();
    let ix = set_series_paused_ix(&admin.pubkey(), &series, 1);
    let cu = env.send(&[ix], &[&admin]).unwrap().compute_units_consumed;
    rows.push(Row {
        name: "set_series_paused",
        note: "config and series PDA re-derivation",
        cu,
    });
    let ix = set_config_ix(&admin.pubkey(), &Address::new_from_array([9; 32]));
    let cu = env.send(&[ix], &[&admin]).unwrap().compute_units_consumed;
    rows.push(Row {
        name: "set_config",
        note: "config PDA re-derivation",
        cu,
    });

    // settle: the common case, then the worst case per token program. The worst case is a
    // fill whose nonce slides the window by 16,383 nonces from an unaligned base (the largest
    // ring clear), on whichever side costs more; for Token-2022 every mint carries the allowed
    // extensions and every token account the account extensions settle walks.
    // Every allowlisted mint extension, taken from the program's own list so a new entry is
    // measured too; values at realistic sizes (unknown types get 64 bytes).
    let heavy_mint: Vec<u8> = quotebook_program::mint::ALLOWED_EXTENSIONS
        .iter()
        .flat_map(|&kind| {
            let value = match kind {
                6 => vec![1], // DefaultAccountState = Initialized
                3 => vec![0; 32],
                10 => vec![0; 52],
                19 => vec![0; 200],
                21 => vec![0; 80],
                23 => vec![0; 72],
                25 => vec![0; 56],
                _ => vec![0; 64],
            };
            tlv(kind, &value)
        })
        .collect();
    let heavy_account = [tlv(7, &[]), tlv(11, &[0]), tlv(8, &[0])].concat();
    let token_2022 = MarketSpec {
        base_2022: true,
        quote_2022: true,
        base_mint_ext: heavy_mint.clone(),
        quote_mint_ext: heavy_mint,
        maker_base_ext: heavy_account.clone(),
        maker_quote_ext: heavy_account.clone(),
        taker_base_ext: heavy_account.clone(),
        taker_quote_ext: heavy_account,
    };
    let settle = |spec: MarketSpec, side: Side, slide: bool| -> (u64, u64) {
        let mut m = Market::with(spec);
        let mut t = m.terms();
        t.side = side;
        if slide {
            let maker = m.maker.insecure_clone();
            let state = maker_state_address(&maker.pubkey());
            m.env
                .send(
                    &[cancel_ix(tag::CANCEL_ALL, &maker.pubkey(), &state, 37)],
                    &[&maker],
                )
                .unwrap();
            t.nonce = Nonce::new(37 + 2 * WINDOW_BITS - 1);
        }
        let meta = m.settle(t, 5).unwrap();
        (meta.compute_units_consumed, meta.fee)
    };
    for (note, side) in [
        ("SPL Token both legs, maker bid", Side::Bid),
        ("SPL Token both legs, maker ask", Side::Ask),
    ] {
        rows.push(Row {
            name: "settle",
            note,
            cu: settle(MarketSpec::default(), side, false).0,
        });
    }
    let worst = |spec: &MarketSpec| {
        [Side::Bid, Side::Ask]
            .into_iter()
            .map(|side| settle(spec.clone(), side, true).0)
            .max()
            .unwrap()
    };
    rows.push(Row {
        name: "settle",
        note: "SPL Token, worst case (16,383-nonce unaligned slide, costlier side)",
        cu: worst(&MarketSpec::default()),
    });
    rows.push(Row {
        name: "settle",
        note: "Token-2022 both legs, plain accounts, maker bid",
        cu: settle(
            MarketSpec {
                base_2022: true,
                quote_2022: true,
                ..MarketSpec::default()
            },
            Side::Bid,
            false,
        )
        .0,
    });
    rows.push(Row {
        name: "settle",
        note: "Token-2022, worst case (as SPL, plus every allowlisted mint extension and three account extensions)",
        cu: worst(&token_2022),
    });

    // The precompile alone, and what it adds to the fee.
    let m = Market::new();
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(m.terms(), &maker);
    let mut env = m.env;
    let meta = env.send(&[pre], &[]).unwrap();
    rows.push(Row {
        name: "ed25519 precompile",
        note: "one signature over a 208-byte quote, alone in a transaction",
        cu: meta.compute_units_consumed,
    });
    let (_, settle_fee) = settle(MarketSpec::default(), Side::Bid, false);
    FEES.with(|f| f.set((meta.fee, settle_fee)));
    rows
}

fn table(rows: &[Row]) -> String {
    let mut out = String::new();
    writeln!(out, "# quotebook compute-unit costs").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Generated by `make bench` (`program/tests/cu.rs`) in LiteSVM. Each figure is the \
         `compute_units_consumed` of a transaction holding only that instruction (for \
         `settle`, also the ed25519 precompile instruction before it), including the system \
         and token program CPIs. Keys and mint addresses of the creation rows are fixed, so \
         the PDA bump searches (about 1,500 CU per attempt) are the same on every run; with \
         other keys a creation can cost a few thousand CU more. `settle` does no bump search, \
         so its cost does not depend on the keys. The precompile itself consumes no compute \
         units (measured below); it costs one signature in the transaction fee."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Instruction | Case | CU |").unwrap();
    writeln!(out, "| --- | --- | ---: |").unwrap();
    for r in rows {
        writeln!(out, "| `{}` | {} | {} |", r.name, r.note, r.cu).unwrap();
    }
    let (precompile_fee, settle_fee) = FEES.with(Cell::get);
    // The text below claims 5,000 lamports per signature, the precompile's included.
    assert_eq!((precompile_fee, settle_fee), (2 * 5_000, 3 * 5_000));
    writeln!(out).unwrap();
    writeln!(
        out,
        "The precompile's 0 CU is not free: the runtime charges each precompile signature like \
         a transaction signature. A precompile-only transaction signed by its fee payer pays \
         {precompile_fee} lamports (two signatures); a `settle` transaction signed by the fee \
         payer and the taker pays {settle_fee} lamports (three signatures). Priority fees come \
         on top, priced per compute unit the transaction requests."
    )
    .unwrap();
    writeln!(out).unwrap();
    out.push_str(MATCHBOOK);
    out
}

/// Cross-repo citation (docs/PLAN.md, M4), not a run in this VM.
const MATCHBOOK: &str = "## Comparison with matchbook (cited, not run here)

matchbook's own LiteSVM bench, `docs/benchmarks.md` at matchbook commit `72efa6d` (branch
`redesign`), reports:

| matchbook operation | CU |
| --- | ---: |
| IOC, 1 fill | 1,660 |
| Swap from a wallet, no account, 1 fill | 4,487 |

Reproduce in matchbook: `cargo build-sbf --manifest-path program/Cargo.toml` then
`cargo test -p matchbook-program --test bench -- --nocapture`.

The designs differ, so the numbers answer different questions. matchbook is an order book:
both sides deposit into the program first, and an IOC fill moves balances inside its own
accounts without a token CPI; its wallet swap adds the token transfers. quotebook has no
deposits and no book: a `settle` verifies a maker-signed RFQ quote and moves both legs with
two token-program CPIs from the parties' wallets, so the nearest matchbook figure is the
wallet swap. This is a citation from another repository's bench, not a same-VM measurement.
";

#[test]
fn test_cu_table_is_deterministic() {
    let first = measure();
    let second = measure();
    for (a, b) in first.iter().zip(&second) {
        assert_eq!(a.cu, b.cu, "{} ({}) is not deterministic", a.name, a.note);
    }
    let out = table(&first);
    println!("{out}");
    if let Ok(path) = std::env::var("QUOTEBOOK_BENCH_OUT") {
        std::fs::write(&path, &out).unwrap();
    }
}
