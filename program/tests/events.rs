//! I7: the fill event `settle` logs, and its provenance through `quotebook-sdk::events`, in
//! LiteSVM with the real program and a test-only probe program (test-programs/probe) that
//! spoofs the bytes and calls `settle` by CPI.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::result_large_err,
    missing_docs
)]

mod common;

use std::path::PathBuf;

use common::*;
use quotebook_core::event::FillEvent;
use quotebook_core::{BaseLots, Nonce, QuoteLotsPerBaseLot, QuoteTerms, Side, UnixTimestamp};
use quotebook_sdk::events::{fill_events, Commitment, EventsError, Extraction, TransactionLogs};
use solana_address::Address;
use solana_instruction::{AccountMeta, Instruction};
use solana_signer::Signer;

const PROBE: Address = Address::new_from_array([0x9b; 32]);

fn with_probe(spec: MarketSpec) -> Market {
    let mut m = Market::with(spec);
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/deploy/quotebook_probe.so");
    m.env
        .svm
        .add_program_from_file(PROBE, &path)
        .unwrap_or_else(|e| panic!("load {}: {e}; run `make test-programs`", path.display()));
    m
}

fn events_of(logs: &[String], succeeded: bool) -> Result<Extraction, EventsError> {
    fill_events(
        &PROGRAM_ID,
        &TransactionLogs {
            succeeded,
            commitment: Commitment::Finalized,
            logs,
        },
        Commitment::Finalized,
    )
}

fn complete(x: Result<Extraction, EventsError>) -> Vec<FillEvent> {
    match x {
        Ok(Extraction::Complete(v)) => v.into_iter().map(|e| e.event).collect(),
        other => panic!("expected a complete extraction, got {other:?}"),
    }
}

/// The event `settle` must report for `terms` filled by `m.taker` at `fill` lots, with the
/// harness's lot sizes (base 1,000, quote 10).
fn expected(m: &Market, terms: &QuoteTerms, fill: u64) -> FillEvent {
    FillEvent {
        series: m.series,
        maker: terms.maker,
        taker: m.taker.pubkey(),
        nonce: terms.nonce,
        side: terms.side,
        price: terms.price,
        fill_size: BaseLots::new(fill),
        base_atoms: fill * 1_000,
        quote_atoms: terms.price.get() * fill * 10,
        fee: 0,
        unix_timestamp: UnixTimestamp::new(NOW),
    }
}

#[test]
fn test_settle_emits_one_event_on_both_sides_and_token_programs() {
    for spec in [
        MarketSpec::default(),
        MarketSpec {
            base_2022: true,
            quote_2022: true,
            ..MarketSpec::default()
        },
    ] {
        for side in [Side::Bid, Side::Ask] {
            let mut m = Market::with(spec.clone());
            let terms = QuoteTerms {
                side,
                price: QuoteLotsPerBaseLot::new(7),
                nonce: Nonce::new(3),
                ..m.terms()
            };
            let meta = m.settle(terms, 4).unwrap();
            let data_lines = meta
                .logs
                .iter()
                .filter(|l| l.starts_with("Program data: "))
                .count();
            assert_eq!(data_lines, 1, "{}", meta.pretty_logs());
            assert_eq!(
                complete(events_of(&meta.logs, true)),
                vec![expected(&m, &terms, 4)]
            );
        }
    }
}

#[test]
fn test_failed_settle_emits_no_event() {
    let mut m = Market::new();
    let failed = m.settle(m.terms(), 11).unwrap_err(); // above the quote's size 10
    assert!(
        !failed
            .meta
            .logs
            .iter()
            .any(|l| l.starts_with("Program data: ")),
        "{}",
        failed.meta.pretty_logs()
    );
}

#[test]
fn test_settle_then_failing_instruction_yields_no_event() {
    let mut m = with_probe(MarketSpec::default());
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(m.terms(), &maker);
    let settle = m.settle_ix(0, 2);
    // Tag 0xff is not a probe instruction: it fails after settle succeeded.
    let fail = Instruction::new_with_bytes(PROBE, &[0xff], vec![]);
    let failed = m.send_as_taker(&[pre, settle, fail]).unwrap_err();
    // The event line is in the logs, although the transaction rolled back...
    assert!(failed
        .meta
        .logs
        .iter()
        .any(|l| l.starts_with("Program data: ")));
    // ...and fill_events refuses the transaction.
    assert_eq!(
        events_of(&failed.meta.logs, false),
        Err(EventsError::TransactionFailed)
    );
    assert_eq!(m.env.maker_state(&m.maker.pubkey()).fills, 0);
}

#[test]
fn test_spoofed_emitter_is_rejected() {
    let mut m = with_probe(MarketSpec::default());
    let fake = expected(&m, &m.terms(), 10);
    let mut data = vec![0u8];
    data.extend_from_slice(&fake.encode());
    let spoof = Instruction::new_with_bytes(PROBE, &data, vec![]);
    let meta = m.env.send(&[spoof], &[]).unwrap();
    // The bytes decode as a fill event, but the probe logged them, not quotebook.
    let line = meta
        .logs
        .iter()
        .find_map(|l| l.strip_prefix("Program data: "))
        .unwrap();
    assert!(!line.is_empty());
    assert_eq!(complete(events_of(&meta.logs, true)), vec![]);
}

/// `settle` called by the probe through CPI (depth 2). With `log_after`, the probe then logs
/// the same bytes in its own frame, which must not count.
fn settle_by_cpi(log_after: bool) {
    let mut m = with_probe(MarketSpec::default());
    let terms = m.terms();
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(terms, &maker);
    let settle = m.settle_ix(0, 5);
    let mut data = vec![if log_after { 2 } else { 1 }];
    data.extend_from_slice(&settle.data);
    let mut accounts = vec![AccountMeta::new_readonly(PROGRAM_ID, false)];
    accounts.extend(settle.accounts.iter().cloned());
    let probe = Instruction::new_with_bytes(PROBE, &data, accounts);
    let meta = m.send_as_taker(&[pre, probe]).unwrap();
    assert!(
        meta.logs
            .iter()
            .any(|l| l == &format!("Program {PROGRAM_ID} invoke [2]")),
        "{}",
        meta.pretty_logs()
    );
    assert_eq!(
        complete(events_of(&meta.logs, true)),
        vec![expected(&m, &terms, 5)]
    );
}

#[test]
fn test_settle_by_cpi_at_depth_two_yields_its_event() {
    settle_by_cpi(false);
}

#[test]
fn test_probe_logging_the_same_bytes_after_the_cpi_is_rejected() {
    // The probe's data for tag 2 is the settle instruction data, not an event, so log the
    // event bytes in a second spoofing instruction of the same transaction instead.
    settle_by_cpi(true);
    let mut m = with_probe(MarketSpec::default());
    let terms = m.terms();
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(terms, &maker);
    let settle = m.settle_ix(0, 5);
    let mut spoof_data = vec![0u8];
    spoof_data.extend_from_slice(&expected(&m, &terms, 5).encode());
    let spoof = Instruction::new_with_bytes(PROBE, &spoof_data, vec![]);
    let meta = m.send_as_taker(&[pre, settle, spoof]).unwrap();
    let data_lines = meta
        .logs
        .iter()
        .filter(|l| l.starts_with("Program data: "))
        .count();
    assert_eq!(data_lines, 2);
    assert_eq!(
        complete(events_of(&meta.logs, true)),
        vec![expected(&m, &terms, 5)]
    );
}

/// The cost of the two ways to emit 168 bytes (I7: "the PR records the CU of both"), measured
/// with the probe: `sol_log_data` directly, and a self-CPI that logs them in the callee (the
/// shape of Anchor's `emit_cpi`, minus its event-authority PDA check).
#[test]
fn test_event_emission_cost_log_data_vs_self_cpi() {
    let mut m = with_probe(MarketSpec::default());
    let event = expected(&m, &m.terms(), 1).encode();
    let mut direct = vec![0u8];
    direct.extend_from_slice(&event);
    let log = m
        .env
        .send(&[Instruction::new_with_bytes(PROBE, &direct, vec![])], &[])
        .unwrap()
        .compute_units_consumed;
    let mut via_cpi = vec![1u8];
    via_cpi.extend_from_slice(&direct);
    let cpi = m
        .env
        .send(
            &[Instruction::new_with_bytes(
                PROBE,
                &via_cpi,
                vec![AccountMeta::new_readonly(PROBE, false)],
            )],
            &[],
        )
        .unwrap()
        .compute_units_consumed;
    println!("emit 168 bytes: sol_log_data {log} CU, self-CPI {cpi} CU");
    assert!(cpi > log);
}
