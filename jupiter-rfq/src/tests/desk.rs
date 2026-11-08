//! The desk: single-use quotes, reservations and their release, the fee budget under the lock,
//! the restart, and the limits at `/quote` and `/swap`.

use std::sync::Barrier;

use solana_keypair::Keypair;

use super::*;
use crate::api::{QuoteType, RejectionReason};
use crate::desk::{CollectionError, Commit, Decline, SwapDecision, HOUR_SECS};

fn quote(r: &Rig, id: &str, taker: &Keypair) -> QuoteResponse {
    r.desk
        .quote(&quote_request(
            id,
            taker,
            true,
            QuoteType::ExactOut,
            1_000_000_000,
        ))
        .unwrap()
}

fn reserve(
    r: &Rig,
    req: &SwapRequest,
) -> Result<crate::validate::Validated, crate::desk::Rejection> {
    match r.desk.swap(req, Commit::Reserve) {
        SwapDecision::WouldSign { validated, .. } => Ok(validated),
        SwapDecision::Reject(x) => Err(x),
    }
}

/// A quote and its fill with a large fee: 300,000 CU at 1,000,000 µlamports, 310,000 lamports.
fn expensive(r: &Rig, id: &str, taker: &Keypair) -> SwapRequest {
    let q = quote(r, id, taker);
    let mut tx = FillTx::for_quote(&q, taker);
    for ix in &mut tx.instructions {
        if ix.program == COMPUTE_BUDGET {
            ix.data = match ix.data[0] {
                2 => ComputeBudget::SetComputeUnitLimit(300_000).encode(),
                _ => ComputeBudget::SetComputeUnitPrice(1_000_000).encode(),
            };
        }
    }
    tx.request(id, taker)
}

fn set_sol(r: &Rig, sol: u64) {
    let mut s = r.chain.snapshot.lock().unwrap();
    s.sol_lamports = sol;
    s.slot += 1;
    drop(s);
    r.desk.reconcile(&r.chain).unwrap();
}

#[test]
fn a_quote_id_is_single_use_and_a_retry_is_idempotent() {
    let r = rig("single-use");
    let taker = Keypair::new();
    let q = quote(&r, "a", &taker);
    let tx = FillTx::for_quote(&q, &taker);
    let req = tx.request("a", &taker);
    let first = reserve(&r, &req).unwrap();
    assert_eq!(r.desk.status().open_swaps, 1);
    // The same message again: the same answer, no second reservation.
    assert_eq!(reserve(&r, &req).unwrap(), first);
    assert_eq!(r.desk.status().open_swaps, 1);
    // Another transaction for the same quote.
    let mut other = tx.clone();
    other.instructions.push(FillTx::lighthouse(6));
    let err = reserve(&r, &other.request("a", &taker)).unwrap_err();
    assert_eq!(err.message, "quote already used");
    // The same message for another quote id.
    let mut moved = req.clone();
    moved.quote_id = "b".into();
    assert!(reserve(&r, &moved)
        .unwrap_err()
        .message
        .contains("another quote"));
    // And the id cannot be quoted again.
    assert_eq!(
        r.desk.quote(&quote_request(
            "a",
            &taker,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::QuoteIdUsed)
    );
}

#[test]
fn unknown_expired_and_anonymous_quotes_are_refused() {
    let r = rig("quotes");
    let taker = Keypair::new();
    let q = quote(&r, "a", &taker);
    let tx = FillTx::for_quote(&q, &taker);
    assert_eq!(
        reserve(&r, &tx.request("zzz", &taker)).unwrap_err().message,
        "unknown quote"
    );

    let mut anon = quote_request("anon", &taker, true, QuoteType::ExactOut, 1_000_000_000);
    anon.taker = None;
    r.desk.quote(&anon).unwrap();
    assert!(reserve(&r, &tx.request("anon", &taker))
        .unwrap_err()
        .message
        .contains("no taker"));

    // A receiver other than the taker is declined at `/quote`.
    let mut recv = quote_request("recv", &taker, true, QuoteType::ExactOut, 1_000_000_000);
    recv.receiver = Some(MAKER.to_string());
    assert_eq!(r.desk.quote(&recv), Err(Decline::Receiver));

    r.clock.set(NOW + 56);
    r.chain.at(NOW + 56);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(
        reserve(&r, &tx.request("a", &taker)).unwrap_err().message,
        "quote expired"
    );
}

#[test]
fn landed_failed_and_expired_reservations_are_released() {
    let r = rig("release");
    let (t1, t2, t3) = (Keypair::new(), Keypair::new(), Keypair::new());
    let mut ids = Vec::new();
    for (id, t) in [("a", &t1), ("b", &t2), ("c", &t3)] {
        let q = quote(&r, id, t);
        let v = reserve(&r, &FillTx::for_quote(&q, t).request(id, t)).unwrap();
        ids.push((v.taker_signature, v.fee_lamports, q));
    }
    assert_eq!(r.desk.status().open_swaps, 3);
    assert_eq!(r.desk.status().reserved_fees, 3 * 12_000);

    // a lands, b fails: both fees spent, one fill, the fee liability accrued once.
    r.chain.land(ids[0].0, 12_000, NOW + 5, false);
    r.chain.land(ids[1].0, 12_000, NOW + 5, true);
    r.chain.at(NOW + 10);
    r.desk.reconcile(&r.chain).unwrap();
    let s = r.desk.status();
    assert_eq!(s.open_swaps, 1);
    assert_eq!(s.fills_last_hour, 1);
    assert_eq!(s.fees_last_hour, 24_000);
    // ceil(1,000,000,000 × 2 / 10,000) base atoms on the base mint.
    assert_eq!(s.accrued_fees, vec![(BASE, 200_000)]);

    // c is absent: still held at its expiry (fill passes at equality), released past it.
    r.chain.at(NOW + 55);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(r.desk.status().open_swaps, 1);
    r.chain.at(NOW + 56);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(r.desk.status().open_swaps, 0);
    assert_eq!(r.desk.status().fees_last_hour, 24_000);

    // An hour later the window is empty.
    r.clock.set(NOW + 3_700);
    r.chain.at(NOW + 3_700);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(r.desk.status().fees_last_hour, 0);
    assert_eq!(r.desk.status().fills_last_hour, 0);
}

#[test]
fn two_concurrent_swaps_cannot_share_the_same_lamports() {
    let r = rig("concurrent");
    // Quotes check the floor against the 415,000 per-transaction cap; each fill costs 310,000,
    // so the wallet covers one fill above the floor, not two.
    set_sol(&r, FLOOR + 415_000);
    let (t1, t2) = (Keypair::new(), Keypair::new());
    let a = expensive(&r, "a", &t1);
    let b = expensive(&r, "b", &t2);
    let barrier = Barrier::new(2);
    let results: Vec<bool> = std::thread::scope(|s| {
        let hs: Vec<_> = [&a, &b]
            .into_iter()
            .map(|req| {
                let barrier = &barrier;
                let desk = &r.desk;
                s.spawn(move || {
                    barrier.wait();
                    matches!(
                        desk.swap(req, Commit::Reserve),
                        SwapDecision::WouldSign { .. }
                    )
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(results.iter().filter(|ok| **ok).count(), 1, "{results:?}");
    assert_eq!(r.desk.status().reserved_fees, 310_000);
}

#[test]
fn the_sol_floor_and_the_hourly_fees_are_exact() {
    // Two 310,000-lamport fills need 620,000 above the floor: one lamport short refuses.
    for (sol, ok) in [(FLOOR + 620_000, true), (FLOOR + 619_999, false)] {
        let r = rig(&format!("floor-{ok}"));
        set_sol(&r, sol);
        let (t1, t2) = (Keypair::new(), Keypair::new());
        let a = expensive(&r, "a", &t1);
        let b = expensive(&r, "b", &t2);
        reserve(&r, &a).unwrap();
        let second = reserve(&r, &b);
        assert_eq!(second.is_ok(), ok, "{second:?}");
        if !ok {
            assert!(second.unwrap_err().message.contains("sol floor"));
        }
    }
    for (cap, ok) in [(620_000, true), (619_999, false)] {
        let mut cfg = config(&format!("hour-{ok}"));
        cfg.limits.max_fees_per_hour = cap;
        let r = rig_with(cfg, Start::Fresh);
        set_sol(&r, u64::MAX / 2);
        let (t1, t2) = (Keypair::new(), Keypair::new());
        let a = expensive(&r, "a", &t1);
        let b = expensive(&r, "b", &t2);
        reserve(&r, &a).unwrap();
        let second = reserve(&r, &b);
        assert_eq!(second.is_ok(), ok, "{second:?}");
        if !ok {
            assert!(second.unwrap_err().message.contains("fees per hour"));
        }
    }
}

#[test]
fn a_failed_transaction_keeps_its_fee_as_spent() {
    let mut cfg = config("failed-fee");
    cfg.limits.max_fees_per_hour = 415_000 + 310_000;
    let r = rig_with(cfg, Start::Fresh);
    set_sol(&r, u64::MAX / 2);
    let t1 = Keypair::new();
    let a = expensive(&r, "a", &t1);
    let v = reserve(&r, &a).unwrap();
    r.chain.land(v.taker_signature, 310_000, NOW + 2, true);
    r.chain.at(NOW + 3);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(r.desk.status().open_swaps, 0);
    assert_eq!(r.desk.status().fills_last_hour, 0);
    assert_eq!(r.desk.status().fees_last_hour, 310_000);
    // The spent fee still counts: 310,000 + the 415,000 stand-in fits exactly; one more
    // 310,000 fill does not.
    let t2 = Keypair::new();
    let b = expensive(&r, "b", &t2);
    reserve(&r, &b).unwrap();
    let t3 = Keypair::new();
    assert!(matches!(
        r.desk.quote(&quote_request(
            "c",
            &t3,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::Limit("fees per hour"))
    ));
}

#[test]
fn inventory_size_and_open_swap_limits() {
    let r = rig("inventory");
    let t = Keypair::new();
    // The taker buys more base than one quote may pay out.
    assert_eq!(
        r.desk.quote(&quote_request(
            "big",
            &t,
            true,
            QuoteType::ExactOut,
            5_000_000_001
        )),
        Err(Decline::Limit("size per quote"))
    );
    // Two quotes of 5 base tokens each against 10 held, plus the fee liability: the first
    // reserves, the second is short at `/swap`, with the reason Jupiter knows.
    let (t1, t2) = (Keypair::new(), Keypair::new());
    let qa = r
        .desk
        .quote(&quote_request(
            "a",
            &t1,
            true,
            QuoteType::ExactOut,
            5_000_000_000,
        ))
        .unwrap();
    let qb = r
        .desk
        .quote(&quote_request(
            "b",
            &t2,
            true,
            QuoteType::ExactOut,
            5_000_000_000,
        ))
        .unwrap();
    reserve(&r, &FillTx::for_quote(&qa, &t1).request("a", &t1)).unwrap();
    let err = reserve(&r, &FillTx::for_quote(&qb, &t2).request("b", &t2)).unwrap_err();
    assert_eq!(err.known, Some(RejectionReason::InsufficientBalance));
    // And `/quote` now declines the same size.
    assert_eq!(
        r.desk.quote(&quote_request(
            "c",
            &t2,
            true,
            QuoteType::ExactOut,
            5_000_000_000
        )),
        Err(Decline::Limit("inventory"))
    );

    let mut cfg = config("open");
    cfg.limits.max_open_swaps = 1;
    let r = rig_with(cfg, Start::Fresh);
    let qa = quote(&r, "a", &t1);
    reserve(&r, &FillTx::for_quote(&qa, &t1).request("a", &t1)).unwrap();
    assert_eq!(
        r.desk.quote(&quote_request(
            "b",
            &t2,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::Limit("open swaps"))
    );
}

#[test]
fn fills_per_hour_count_landed_and_reserved() {
    let mut cfg = config("fills");
    cfg.limits.max_fills_per_hour = 2;
    let r = rig_with(cfg, Start::Fresh);
    let (t1, t2, t3) = (Keypair::new(), Keypair::new(), Keypair::new());
    let qa = quote(&r, "a", &t1);
    let v = reserve(&r, &FillTx::for_quote(&qa, &t1).request("a", &t1)).unwrap();
    r.chain.land(v.taker_signature, 12_000, NOW + 1, false);
    r.chain.at(NOW + 2);
    r.desk.reconcile(&r.chain).unwrap();
    let qb = quote(&r, "b", &t2);
    reserve(&r, &FillTx::for_quote(&qb, &t2).request("b", &t2)).unwrap();
    // One landed and one reserved: the hour is full.
    assert_eq!(
        r.desk.quote(&quote_request(
            "c",
            &t3,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::Limit("fills per hour"))
    );
}

#[test]
fn halt_stale_state_and_failed_reads_refuse() {
    let r = rig("halt");
    let t = Keypair::new();
    let q = quote(&r, "a", &t);
    let req = FillTx::for_quote(&q, &t).request("a", &t);
    std::fs::write(&r.desk.config().limits.halt_file, "").unwrap();
    assert_eq!(
        r.desk
            .quote(&quote_request("b", &t, true, QuoteType::ExactOut, 1)),
        Err(Decline::Halted)
    );
    assert_eq!(reserve(&r, &req).unwrap_err().message, "halted");
    assert!(r.desk.status().halted);
    std::fs::remove_file(&r.desk.config().limits.halt_file).unwrap();

    // A stale spot.
    r.spot_age.store(31, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        r.desk.quote(&quote_request(
            "b",
            &t,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::Spot)
    );
    r.spot_age.store(0, std::sync::atomic::Ordering::SeqCst);

    // A snapshot older than 60 s by its Clock.
    r.clock.set(NOW + 61);
    assert!(matches!(
        r.desk.quote(&quote_request(
            "b",
            &t,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::Unavailable("snapshot too old"))
    ));

    // A failed read refuses and releases nothing; a slot going backwards too.
    reserve(&r, &req).unwrap_err();
    r.clock.set(NOW);
    let v = reserve(&r, &req).unwrap();
    *r.chain.fail.lock().unwrap() = true;
    assert!(r.desk.reconcile(&r.chain).is_err());
    assert!(matches!(
        r.desk.quote(&quote_request(
            "b",
            &t,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::Unavailable(_))
    ));
    assert_eq!(r.desk.status().open_swaps, 1);
    *r.chain.fail.lock().unwrap() = false;
    r.chain.snapshot.lock().unwrap().slot = 0;
    r.chain.land(v.taker_signature, 12_000, NOW, false);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(
        r.desk.status().open_swaps,
        1,
        "a slot going backwards releases nothing"
    );
}

#[test]
fn a_restart_waits_out_the_previous_run_and_rebuilds_the_hour() {
    let t = Keypair::new();
    // The rebuild works: the history's fills count against the hour.
    let mut cfg = config("restart-ok");
    cfg.limits.max_fills_per_hour = 2;
    let r = rig_with(cfg, Start::Resume { started_at: NOW });
    assert_eq!(
        r.desk.quote(&quote_request(
            "a",
            &t,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::NotYet(NOW + 65))
    );
    *r.chain.history.lock().unwrap() = Ok(vec![
        Spent {
            at: NOW - 100,
            fee_lamports: 5_000,
            fill: true,
        },
        Spent {
            at: NOW - 50,
            fee_lamports: 5_000,
            fill: true,
        },
    ]);
    r.clock.set(NOW + 65);
    r.chain.at(NOW + 65);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(r.desk.status().fills_last_hour, 2);
    assert_eq!(
        r.desk.quote(&quote_request(
            "a",
            &t,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::Limit("fills per hour"))
    );

    // The rebuild fails: nothing is quoted or co-signed for 3,600 s of chain Clock.
    let r = rig_with(config("restart-fail"), Start::Resume { started_at: NOW });
    *r.chain.history.lock().unwrap() = Err(ChainError("no history".into()));
    r.clock.set(NOW + 65);
    r.chain.at(NOW + 65);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(
        r.desk.quote(&quote_request(
            "a",
            &t,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        )),
        Err(Decline::NotYet(NOW + 3_600))
    );
    r.clock.set(NOW + 3_599);
    r.chain.at(NOW + 3_599);
    r.desk.reconcile(&r.chain).unwrap();
    assert!(r
        .desk
        .quote(&quote_request(
            "a",
            &t,
            true,
            QuoteType::ExactOut,
            1_000_000_000
        ))
        .is_err());
    r.clock.set(NOW + 3_600);
    r.chain.at(NOW + 3_600);
    r.desk.reconcile(&r.chain).unwrap();
    let q = r.desk.quote(&quote_request(
        "a",
        &t,
        true,
        QuoteType::ExactOut,
        1_000_000_000,
    ));
    assert!(q.is_ok(), "{q:?}");
}

#[test]
fn malformed_quote_requests() {
    let r = rig("malformed");
    let t = Keypair::new();
    let mut q = quote_request("a", &t, true, QuoteType::ExactIn, 1);
    for bad in ["", "0", "-1", "1.5", "+1", "99999999999999999999999"] {
        q.amount = bad.into();
        assert_eq!(
            r.desk.quote(&q),
            Err(Decline::BadRequest("amount")),
            "{bad:?}"
        );
    }
    let mut q = quote_request("a", &t, true, QuoteType::ExactIn, 1_000_000);
    q.token_out = "fake3KUxqvJ5erXobKTYFtL2BpTgGzy7B9AcRcXeCwWvFM".into();
    assert_eq!(r.desk.quote(&q), Err(Decline::Unsupported));
    let mut q = quote_request("a", &t, true, QuoteType::ExactIn, 1_000_000);
    q.fee_bps = 10_001;
    assert!(matches!(r.desk.quote(&q), Err(Decline::BadRequest(_))));
    let mut q = quote_request("a", &t, true, QuoteType::ExactIn, 1_000_000);
    q.quote_id = "x".repeat(129);
    assert!(matches!(r.desk.quote(&q), Err(Decline::BadRequest(_))));
}

fn set_base(r: &Rig, atoms: u64) {
    let mut s = r.chain.snapshot.lock().unwrap();
    s.balances = vec![(BASE, atoms), (QUOTE, 2_000_000_000)];
    s.slot += 1;
    drop(s);
    r.desk.reconcile(&r.chain).unwrap();
}

/// Reserves a 1,000,000,000 base atom fill and lands it: 200,000 base atoms of fee debt.
fn land_one(r: &Rig, id: &str, taker: &Keypair) {
    let q = quote(r, id, taker);
    let v = reserve(r, &FillTx::for_quote(&q, taker).request(id, taker)).unwrap();
    r.chain.land(v.taker_signature, 12_000, NOW + 5, false);
    r.chain.at(NOW + 10);
    r.desk.reconcile(&r.chain).unwrap();
}

fn base_out(id: &str, taker: &Keypair) -> QuoteRequest {
    quote_request(id, taker, true, QuoteType::ExactOut, 1_000_000_000)
}

#[test]
fn uncollected_fee_debt_is_held_against_inventory() {
    let r = rig("debt-held");
    let t = Keypair::new();
    land_one(&r, "a", &t);
    assert_eq!(r.desk.status().accrued_fees, vec![(BASE, 200_000)]);
    // Codex's case: 1,000,399,999 held less 200,000 owed is 1,000,199,999, one atom short of
    // 1,000,000,000 out plus its 200,000 liability.
    set_base(&r, 1_000_399_999);
    assert_eq!(
        r.desk.quote(&base_out("b", &t)),
        Err(Decline::Limit("inventory"))
    );
    set_base(&r, 1_000_400_000);
    let q = r.desk.quote(&base_out("c", &t)).unwrap();
    // And at `/swap`: quoted with room, then one atom less.
    set_base(&r, 1_000_399_999);
    let err = reserve(&r, &FillTx::for_quote(&q, &t).request("c", &t)).unwrap_err();
    assert_eq!(err.known, Some(RejectionReason::InsufficientBalance));
    assert_eq!(r.desk.status().open_swaps, 0);
}

#[test]
fn fee_debt_survives_a_restart_and_only_a_recorded_collection_releases_it() {
    let ledger = MemoryLedger::new();
    let r = rig_with_ledger(config("debt-restart"), Start::Fresh, ledger.clone());
    let t = Keypair::new();
    land_one(&r, "a", &t);
    assert_eq!(ledger.debts(), vec![(BASE, 200_000)], "written at once");
    drop(r);

    // A new desk over the same ledger: the chain alone could not rebuild this.
    let r = rig_with_ledger(config("debt-restart"), Start::Fresh, ledger.clone());
    assert_eq!(r.desk.status().accrued_fees, vec![(BASE, 200_000)]);
    set_base(&r, 1_000_200_000);
    assert_eq!(
        r.desk.quote(&base_out("b", &t)),
        Err(Decline::Limit("inventory"))
    );

    assert_eq!(
        r.desk.record_collection(BASE, 200_000, "  "),
        Err(CollectionError::Evidence)
    );
    assert_eq!(
        r.desk.record_collection(BASE, 200_001, "invoice 7"),
        Err(CollectionError::MoreThanOwed {
            collected: 200_001,
            owed: 200_000
        })
    );
    // A release that cannot be written releases nothing.
    ledger.fail_store(true);
    assert!(matches!(
        r.desk.record_collection(BASE, 200_000, "invoice 7"),
        Err(CollectionError::Write(_))
    ));
    assert_eq!(r.desk.status().accrued_fees, vec![(BASE, 200_000)]);
    assert_eq!(ledger.debts(), vec![(BASE, 200_000)]);
    ledger.fail_store(false);

    assert_eq!(
        r.desk.record_collection(BASE, 50_000, "invoice 7"),
        Ok(150_000)
    );
    assert_eq!(ledger.debts(), vec![(BASE, 150_000)]);
    assert_eq!(r.desk.record_collection(BASE, 150_000, "invoice 8"), Ok(0));
    assert!(ledger.debts().is_empty());
    assert!(r.desk.status().accrued_fees.is_empty());
    assert!(r.desk.quote(&base_out("b", &t)).is_ok());
}

#[test]
fn an_unreadable_or_unwritten_ledger_refuses() {
    let t = Keypair::new();
    let ledger = MemoryLedger::new();
    ledger.fail_load(true);
    let r = rig_with_ledger(config("ledger-unreadable"), Start::Fresh, ledger.clone());
    assert_eq!(
        r.desk.quote(&base_out("a", &t)),
        Err(Decline::Unavailable("debt ledger unreadable"))
    );
    assert_eq!(
        r.desk.record_collection(BASE, 1, "invoice 1"),
        Err(CollectionError::NotLoaded)
    );
    // Never written over: the unread ledger keeps whatever it holds.
    ledger.fail_load(false);
    r.chain.at(NOW + 1);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(
        r.desk.quote(&base_out("a", &t)),
        Err(Decline::Unavailable("debt ledger unreadable"))
    );

    // An accrual that cannot be written is kept in memory and refuses until a write succeeds.
    let ledger = MemoryLedger::new();
    let r = rig_with_ledger(config("ledger-write"), Start::Fresh, ledger.clone());
    let q = quote(&r, "a", &t);
    let v = reserve(&r, &FillTx::for_quote(&q, &t).request("a", &t)).unwrap();
    ledger.fail_store(true);
    r.chain.land(v.taker_signature, 12_000, NOW + 5, false);
    r.chain.at(NOW + 10);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(r.desk.status().accrued_fees, vec![(BASE, 200_000)]);
    assert_eq!(
        r.desk.status().ledger_failure,
        Some("debt ledger write failed")
    );
    assert_eq!(
        r.desk.quote(&base_out("b", &t)),
        Err(Decline::Unavailable("debt ledger write failed"))
    );
    ledger.fail_store(false);
    r.chain.at(NOW + 20);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(ledger.debts(), vec![(BASE, 200_000)]);
    assert_eq!(r.desk.status().ledger_failure, None);
    assert!(r.desk.quote(&base_out("b", &t)).is_ok());
}

#[test]
fn a_retry_after_release_gets_the_terminal_answer_not_a_new_permission() {
    let r = rig("retry-release");
    let (t1, t2, t3) = (Keypair::new(), Keypair::new(), Keypair::new());
    let mut reqs = Vec::new();
    for (id, t) in [("a", &t1), ("b", &t2), ("c", &t3)] {
        let q = quote(&r, id, t);
        let req = FillTx::for_quote(&q, t).request(id, t);
        let v = reserve(&r, &req).unwrap();
        reqs.push((req, v));
    }
    // a lands, b fails, c is never seen and expires: Codex's case is c.
    r.chain
        .land(reqs[0].1.taker_signature, 12_000, NOW + 5, false);
    r.chain
        .land(reqs[1].1.taker_signature, 12_000, NOW + 5, true);
    let expire_at = reqs[2].1.expire_at;
    r.chain.at(expire_at + 1);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(r.desk.status().open_swaps, 0);
    let fees = r.desk.status().reserved_fees;
    for ((req, _), outcome) in reqs.iter().zip([
        "finalized",
        "finalized as failed",
        "expired unlanded and released",
    ]) {
        let err = reserve(&r, req).unwrap_err();
        assert_eq!(
            err.message,
            format!("transaction already {outcome}; not co-signed again")
        );
        // The HTTP layer's dry run gets the same answer.
        assert!(matches!(
            r.desk.swap(req, Commit::DryRun),
            SwapDecision::Reject(_)
        ));
    }
    assert_eq!(r.desk.status().open_swaps, 0, "nothing reserved again");
    assert_eq!(r.desk.status().reserved_fees, fees);

    // An hour past expiry the terminal answer is dropped; the consumed quote still refuses.
    r.chain.at(expire_at + HOUR_SECS + 1);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(
        reserve(&r, &reqs[2].0).unwrap_err().message,
        "quote already used"
    );
}

#[test]
fn a_retry_while_reserved_is_co_signed_only_while_it_can_still_land() {
    let r = rig("retry-close");
    let t = Keypair::new();
    let q = quote(&r, "a", &t);
    let req = FillTx::for_quote(&q, &t).request("a", &t);
    let first = reserve(&r, &req).unwrap();
    assert_eq!(reserve(&r, &req).unwrap(), first, "safe: the same answer");
    // 20 s must be left to land: at expire_at - 19 the retry is refused, the reservation kept.
    r.clock.set(first.expire_at - 19);
    r.chain.at(first.expire_at - 19);
    r.desk.reconcile(&r.chain).unwrap();
    assert_eq!(
        reserve(&r, &req).unwrap_err().message,
        "too close to expire_at to co-sign again"
    );
    assert_eq!(r.desk.status().open_swaps, 1);
    // Halted: refused too.
    std::fs::write(&r.desk.config().limits.halt_file, b"").unwrap();
    assert_eq!(reserve(&r, &req).unwrap_err().message, "halted");
    std::fs::remove_file(&r.desk.config().limits.halt_file).unwrap();
}
