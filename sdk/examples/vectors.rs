//! Writes the test vectors the TypeScript SDK is checked against (docs/PLAN-improvements.md,
//! I8): `cargo run -p quotebook-sdk --example vectors > ts/vectors/vectors.json`. Every value
//! comes from this crate and `quotebook-core`, the definitions the program itself uses.
//! `make vectors-check` regenerates the file and fails on any difference.
//!
//! Numbers are decimal strings (u64 and i64 exceed JavaScript's safe integers), bytes are
//! lowercase hex, addresses base58. Keys are fixed seeds, so Ed25519 signatures (deterministic,
//! RFC 8032) are reproducible in TypeScript byte for byte.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    missing_docs
)]

use std::fmt::Write as _;

use quotebook_sdk::core::accounts::{self, Config, MakerProfile, MakerState, Series};
use quotebook_sdk::core::event::FillEvent;
use quotebook_sdk::core::fee::{fee, MAX_FEE_BPS};
use quotebook_sdk::core::profile::ProfileArgs;
use quotebook_sdk::core::{
    sigverify, BaseAtoms, BaseLots, Nonce, NonceWindow, Quote, QuoteAtoms, QuoteLotsPerBaseLot,
    QuoteTerms, Seconds, SeriesId, Side, UnixTimestamp,
};
use quotebook_sdk::{ix, pda, SettleAccounts};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// The program id the vectors use (the LiteSVM tests' id).
const PROGRAM_ID: Address = Address::new_from_array([0x51; 32]);
const NOW: i64 = 1_700_000_000;

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn key(seed: u8) -> Keypair {
    Keypair::new_from_array([seed; 32])
}

/// A tiny JSON writer: objects of string/number-string/bool/nested values, in order.
enum J {
    S(String),
    B(bool),
    U(u64),
    A(Vec<J>),
    O(Vec<(&'static str, J)>),
}

fn s(v: impl ToString) -> J {
    J::S(v.to_string())
}

impl J {
    fn write(&self, out: &mut String, indent: usize) {
        let pad = "  ".repeat(indent);
        match self {
            J::S(v) => {
                let _ = write!(out, "\"{v}\"");
            }
            J::B(v) => {
                let _ = write!(out, "{v}");
            }
            J::U(v) => {
                let _ = write!(out, "{v}");
            }
            J::A(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&pad);
                    out.push_str("  ");
                    item.write(out, indent + 1);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad);
                out.push(']');
            }
            J::O(fields) => {
                out.push_str("{\n");
                for (i, (k, v)) in fields.iter().enumerate() {
                    let _ = write!(out, "{pad}  \"{k}\": ");
                    v.write(out, indent + 1);
                    out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad);
                out.push('}');
            }
        }
    }
}

fn terms_json(t: &QuoteTerms) -> J {
    J::O(vec![
        ("program_id", s(t.program_id)),
        ("deployment", s(hex(&t.deployment))),
        ("maker", s(t.maker)),
        ("series", s(t.series.address())),
        ("taker", s(t.taker)),
        ("side", J::U(u64::from(t.side.as_u8()))),
        ("price", s(t.price.get())),
        ("size", s(t.size.get())),
        ("issued_at", s(t.issued_at.get())),
        ("expires_at", s(t.expires_at.get())),
        ("nonce", s(t.nonce.get())),
    ])
}

fn ix_json(i: &solana_instruction::Instruction) -> J {
    J::O(vec![
        ("program_id", s(i.program_id)),
        (
            "accounts",
            J::A(
                i.accounts
                    .iter()
                    .map(|m| {
                        J::O(vec![
                            ("pubkey", s(m.pubkey)),
                            ("is_signer", J::B(m.is_signer)),
                            ("is_writable", J::B(m.is_writable)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("data", s(hex(&i.data))),
    ])
}

/// The fixed settle scenario: addresses for the TypeScript-built transaction that the
/// LiteSVM test `sdk/tests/ts_settle.rs` executes.
struct Scenario {
    maker: Keypair,
    taker: Keypair,
    base_mint: Address,
    quote_mint: Address,
    maker_base: Address,
    maker_quote: Address,
    taker_base: Address,
    taker_quote: Address,
    deployment: [u8; 16],
}

fn scenario() -> Scenario {
    Scenario {
        maker: key(11),
        taker: key(12),
        base_mint: key(21).pubkey(),
        quote_mint: key(22).pubkey(),
        maker_base: key(31).pubkey(),
        maker_quote: key(32).pubkey(),
        taker_base: key(33).pubkey(),
        taker_quote: key(34).pubkey(),
        deployment: [7; 16],
    }
}

fn token_program() -> Address {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap()
}

fn main() {
    let sc = scenario();
    let series = pda::series(&PROGRAM_ID, &sc.base_mint, &sc.quote_mint, 0).0;
    let accounts = SettleAccounts {
        taker: sc.taker.pubkey(),
        base_mint: sc.base_mint,
        quote_mint: sc.quote_mint,
        base_token_program: token_program(),
        quote_token_program: token_program(),
        maker_base: sc.maker_base,
        maker_quote: sc.maker_quote,
        taker_base: sc.taker_base,
        taker_quote: sc.taker_quote,
        fee_account: None,
    };
    // The same accounts on a fee series (ADR 0001): account 13 is the canonical fee account.
    let fee_account = pda::fee_account(&PROGRAM_ID, &sc.quote_mint, &token_program());
    let fee_accounts = SettleAccounts {
        fee_account: Some(fee_account),
        ..accounts
    };

    // Quotes: the scenario quote, then edge values.
    let base_terms = QuoteTerms {
        program_id: PROGRAM_ID,
        deployment: sc.deployment,
        maker: sc.maker.pubkey(),
        series: SeriesId::new(series),
        taker: sc.taker.pubkey(),
        side: Side::Bid,
        price: QuoteLotsPerBaseLot::new(250),
        size: BaseLots::new(5),
        issued_at: UnixTimestamp::new(NOW),
        expires_at: UnixTimestamp::new(NOW + 30),
        nonce: Nonce::new(1),
    };
    let all_terms = [
        (base_terms, 3u64, 0u8),
        (
            QuoteTerms {
                taker: Quote::OPEN_TAKER,
                side: Side::Ask,
                price: QuoteLotsPerBaseLot::new(u64::MAX),
                size: BaseLots::new(u64::MAX),
                issued_at: UnixTimestamp::new(i64::MIN),
                expires_at: UnixTimestamp::new(i64::MAX),
                nonce: Nonce::new(u64::MAX),
                ..base_terms
            },
            u64::MAX,
            7,
        ),
        (
            QuoteTerms {
                price: QuoteLotsPerBaseLot::new(1),
                size: BaseLots::new(1),
                issued_at: UnixTimestamp::new(-1),
                expires_at: UnixTimestamp::new(0),
                nonce: Nonce::new(0),
                ..base_terms
            },
            1,
            1,
        ),
    ];
    let quotes = J::A(
        all_terms
            .iter()
            .map(|(t, fill, sig_ix)| {
                let q = Quote::new(*t).unwrap();
                let sig: [u8; 64] = sc
                    .maker
                    .sign_message(&q.encode())
                    .as_ref()
                    .try_into()
                    .unwrap();
                let settle = ix::settle(&PROGRAM_ID, &q, &accounts, *sig_ix, BaseLots::new(*fill));
                let settle_fee = ix::settle(
                    &PROGRAM_ID,
                    &q,
                    &fee_accounts,
                    *sig_ix,
                    BaseLots::new(*fill),
                );
                J::O(vec![
                    ("terms", terms_json(t)),
                    ("encoded", s(hex(&q.encode()))),
                    ("signature", s(hex(&sig))),
                    (
                        "precompile_data",
                        s(hex(&sigverify::instruction_data(&q, &sig))),
                    ),
                    ("fill_size", s(fill)),
                    ("sig_ix_index", J::U(u64::from(*sig_ix))),
                    ("settle", ix_json(&settle)),
                    ("settle_with_fee", ix_json(&settle_fee)),
                ])
            })
            .collect(),
    );

    // PDAs.
    let maker = sc.maker.pubkey();
    let mut pdas = vec![{
        let (a, b) = pda::config(&PROGRAM_ID);
        J::O(vec![
            ("kind", s("config")),
            ("address", s(a)),
            ("bump", J::U(u64::from(b))),
        ])
    }];
    for index in [0u64, 1, 7, u64::MAX] {
        let (a, b) = pda::series(&PROGRAM_ID, &sc.base_mint, &sc.quote_mint, index);
        pdas.push(J::O(vec![
            ("kind", s("series")),
            ("base_mint", s(sc.base_mint)),
            ("quote_mint", s(sc.quote_mint)),
            ("index", s(index)),
            ("address", s(a)),
            ("bump", J::U(u64::from(b))),
        ]));
    }
    for (kind, (a, b)) in [
        ("maker_state", pda::maker_state(&PROGRAM_ID, &maker)),
        ("delegate", pda::delegate(&PROGRAM_ID, &maker)),
        ("profile", pda::profile(&PROGRAM_ID, &maker)),
    ] {
        pdas.push(J::O(vec![
            ("kind", s(kind)),
            ("maker", s(maker)),
            ("address", s(a)),
            ("bump", J::U(u64::from(b))),
        ]));
    }

    pdas.push(J::O(vec![
        ("kind", s("fee_account")),
        ("quote_mint", s(sc.quote_mint)),
        ("token_program", s(token_program())),
        ("address", s(fee_account)),
    ]));

    // Accounts.
    let config = Config {
        hdr: accounts::new_hdr::<Config>(pda::config(&PROGRAM_ID).1),
        admin: key(13).pubkey(),
        series_count: 9,
        deployment_id: sc.deployment,
    };
    let series_account = Series {
        hdr: accounts::new_hdr::<Series>(
            pda::series(&PROGRAM_ID, &sc.base_mint, &sc.quote_mint, 0).1,
        ),
        series_index: 0,
        deployment_id: sc.deployment,
        base_mint: sc.base_mint,
        quote_mint: sc.quote_mint,
        base_lot_size: BaseAtoms::new(100),
        quote_lot_size: QuoteAtoms::new(1),
        max_quote_ttl: Seconds::new(60),
        min_size: BaseLots::new(1),
        paused: 1,
        base_is_2022: 0,
        quote_is_2022: 1,
        _pad: [0; 5],
        fee_bps: 37u16.to_le_bytes(),
        _reserved: [0; 6],
    };
    let mut window = NonceWindow::new();
    for n in [1u64, 2, 70, 16_000] {
        window.consume(Nonce::new(n)).unwrap();
    }
    window.cancel_all(Nonce::new(2)).unwrap();
    let maker_state = MakerState {
        hdr: accounts::new_hdr::<MakerState>(pda::maker_state(&PROGRAM_ID, &maker).1),
        maker,
        fills: 4,
        delegate_bump: pda::delegate(&PROGRAM_ID, &maker).1,
        _pad: [0; 7],
        nonces: window,
    };
    // The expected `NonceWindow::check` result for nonces around every edge.
    let checks: Vec<J> = [
        0u64,
        1,
        2,
        3,
        69,
        70,
        71,
        15_999,
        16_000,
        16_001,
        16_385,
        16_386,
        1 << 40,
    ]
    .iter()
    .map(|n| {
        let result = match maker_state.nonces.check(Nonce::new(*n)) {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("{e:?}"),
        };
        J::O(vec![("nonce", s(n)), ("result", s(result))])
    })
    .collect();
    let mut profile: MakerProfile = bytemuck::Zeroable::zeroed();
    profile.hdr = accounts::new_hdr::<MakerProfile>(pda::profile(&PROGRAM_ID, &maker).1);
    profile.maker = maker;
    ProfileArgs::new(b"wss://maker.example:9000/rfq", b"Example desk")
        .unwrap()
        .write_to(&mut profile);
    quotebook_sdk::core::profile::check_canonical(&profile).unwrap();
    let accounts_json = J::O(vec![
        (
            "config",
            J::O(vec![
                ("data", s(hex(bytemuck::bytes_of(&config)))),
                ("bump", J::U(u64::from(config.hdr.bump))),
                ("admin", s(config.admin)),
                ("series_count", s(config.series_count)),
                ("deployment_id", s(hex(&config.deployment_id))),
            ]),
        ),
        (
            "series",
            J::O(vec![
                ("data", s(hex(bytemuck::bytes_of(&series_account)))),
                ("bump", J::U(u64::from(series_account.hdr.bump))),
                ("series_index", s(0)),
                ("deployment_id", s(hex(&series_account.deployment_id))),
                ("base_mint", s(series_account.base_mint)),
                ("quote_mint", s(series_account.quote_mint)),
                ("base_lot_size", s(100)),
                ("quote_lot_size", s(1)),
                ("max_quote_ttl", s(60)),
                ("min_size", s(1)),
                ("paused", J::B(true)),
                ("base_is_2022", J::B(false)),
                ("quote_is_2022", J::B(true)),
                ("fee_bps", J::U(37)),
            ]),
        ),
        (
            "maker_state",
            J::O(vec![
                ("data", s(hex(bytemuck::bytes_of(&maker_state)))),
                ("bump", J::U(u64::from(maker_state.hdr.bump))),
                ("maker", s(maker)),
                ("fills", s(maker_state.fills)),
                ("delegate_bump", J::U(u64::from(maker_state.delegate_bump))),
                ("window_base", s(maker_state.nonces.window_base().get())),
                (
                    "last_cancel_all",
                    s(maker_state.nonces.last_cancel_all().get()),
                ),
                ("checks", J::A(checks)),
            ]),
        ),
        (
            "maker_profile",
            J::O(vec![
                ("data", s(hex(bytemuck::bytes_of(&profile)))),
                ("bump", J::U(u64::from(profile.hdr.bump))),
                ("maker", s(maker)),
                ("endpoint", s("wss://maker.example:9000/rfq")),
                ("name", s("Example desk")),
            ]),
        ),
    ]);

    // Fees (ADR 0001): floor(quote_atoms × fee_bps / 10,000).
    let fees = J::A(
        [
            (0u64, 100u16),
            (99, 100),
            (100, 100),
            (10_000, 37),
            (1_000_000, 1),
            (u64::MAX, MAX_FEE_BPS),
            (u64::MAX, 0),
        ]
        .iter()
        .map(|(q, bps)| {
            J::O(vec![
                ("quote_atoms", s(q)),
                ("fee_bps", J::U(u64::from(*bps))),
                ("fee", s(fee(*q, *bps).unwrap())),
            ])
        })
        .collect(),
    );

    // Fill events.
    let events = [
        FillEvent {
            series,
            maker,
            taker: sc.taker.pubkey(),
            nonce: Nonce::new(1),
            side: Side::Bid,
            price: QuoteLotsPerBaseLot::new(250),
            fill_size: BaseLots::new(3),
            base_atoms: 300,
            quote_atoms: 750,
            fee: 0,
            unix_timestamp: UnixTimestamp::new(NOW),
        },
        FillEvent {
            series,
            maker,
            taker: sc.taker.pubkey(),
            nonce: Nonce::new(u64::MAX),
            side: Side::Ask,
            price: QuoteLotsPerBaseLot::new(u64::MAX),
            fill_size: BaseLots::new(u64::MAX),
            base_atoms: u64::MAX,
            quote_atoms: u64::MAX,
            fee: u64::MAX,
            unix_timestamp: UnixTimestamp::new(i64::MIN),
        },
    ];
    let events_json = J::A(
        events
            .iter()
            .map(|e| {
                J::O(vec![
                    ("series", s(e.series)),
                    ("maker", s(e.maker)),
                    ("taker", s(e.taker)),
                    ("nonce", s(e.nonce.get())),
                    ("side", J::U(u64::from(e.side.as_u8()))),
                    ("price", s(e.price.get())),
                    ("fill_size", s(e.fill_size.get())),
                    ("base_atoms", s(e.base_atoms)),
                    ("quote_atoms", s(e.quote_atoms)),
                    ("fee", s(e.fee)),
                    ("unix_timestamp", s(e.unix_timestamp.get())),
                    ("encoded", s(hex(&e.encode()))),
                ])
            })
            .collect(),
    );

    // Signatures: a valid one and the same with one bit flipped.
    let q = Quote::new(base_terms).unwrap();
    let sig: [u8; 64] = sc
        .maker
        .sign_message(&q.encode())
        .as_ref()
        .try_into()
        .unwrap();
    let mut bad = sig;
    bad[10] ^= 0x01;
    let sigs = J::A(
        [(sig, true), (bad, false)]
            .iter()
            .map(|(g, valid)| {
                J::O(vec![
                    ("message", s(hex(&q.encode()))),
                    ("pubkey", s(maker)),
                    ("signature", s(hex(g))),
                    ("valid", J::B(*valid)),
                ])
            })
            .collect(),
    );

    // The scenario the TypeScript SDK builds a transaction for.
    let scenario_json = J::O(vec![
        ("program_id", s(PROGRAM_ID)),
        ("maker_seed", s(hex(&[11u8; 32]))),
        ("maker", s(maker)),
        ("taker", s(sc.taker.pubkey())),
        ("base_mint", s(sc.base_mint)),
        ("quote_mint", s(sc.quote_mint)),
        ("token_program", s(token_program())),
        ("maker_base", s(sc.maker_base)),
        ("maker_quote", s(sc.maker_quote)),
        ("taker_base", s(sc.taker_base)),
        ("taker_quote", s(sc.taker_quote)),
        ("series", s(series)),
        ("terms", terms_json(&base_terms)),
        ("fill_size", s(3)),
    ]);

    let root = J::O(vec![
        ("program_id", s(PROGRAM_ID)),
        ("quotes", quotes),
        ("pdas", J::A(pdas)),
        ("accounts", accounts_json),
        ("fees", fees),
        ("fill_events", events_json),
        ("signatures", sigs),
        ("scenario", scenario_json),
    ]);
    let mut out = String::new();
    root.write(&mut out, 0);
    out.push('\n');
    print!("{out}");
}
