//! A valid fill built against the Order Engine's layout, then forged one way at a time; each
//! forgery is refused with its reason. Also the fee budget's cases from the design.

use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

use super::*;
use crate::api::{QuoteType, RejectionReason};
use crate::desk::{Commit, SwapDecision};
use crate::programs::{SQUADS_V4, TOKEN_2022};
use crate::validate::{transaction_fee, Validated};

struct Case {
    rig: Rig,
    taker: Keypair,
    quote: QuoteResponse,
    tx: FillTx,
}

/// A desk with one quote: the taker buys 1 base token, exact-out.
fn case(name: &str) -> Case {
    case_with(rig(name))
}

fn case_with(rig: Rig) -> Case {
    let taker = Keypair::new();
    let quote = rig
        .desk
        .quote(&quote_request(
            "q1",
            &taker,
            true,
            QuoteType::ExactOut,
            1_000_000_000,
        ))
        .unwrap();
    let tx = FillTx::for_quote(&quote, &taker);
    Case {
        rig,
        taker,
        quote,
        tx,
    }
}

impl Case {
    fn run(&self, tx: &FillTx) -> Result<Validated, String> {
        self.run_req(&tx.request("q1", &self.taker))
    }

    fn run_req(&self, req: &SwapRequest) -> Result<Validated, String> {
        match self.rig.desk.swap(req, Commit::DryRun) {
            SwapDecision::WouldSign { validated, .. } => Ok(validated),
            SwapDecision::Reject(r) => Err(r.message),
        }
    }

    fn refused(&self, tx: &FillTx, expect: &str) {
        let got = self.run(tx).expect_err("expected a refusal");
        assert!(got.contains(expect), "expected {expect:?}, got {got:?}");
    }
}

#[test]
fn the_canonical_fill_passes_and_a_dry_run_reserves_nothing() {
    let c = case("valid");
    let v = c.run(&c.tx).unwrap();
    assert_eq!(v.cu_limit, 200_000);
    assert_eq!(v.cu_price, 10_000);
    // 2 signatures × 5,000 + ceil(200,000 × 10,000 / 1,000,000).
    assert_eq!(v.fee_lamports, 12_000);
    assert_eq!(v.expire_at, NOW + 55);
    assert_eq!(c.rig.desk.status().open_swaps, 0);
    assert_eq!(c.quote.maker, MAKER.to_string());
    // Exact-out of 1 base token: the taker pays above the mid.
    assert!(c.quote.amount_in.parse::<u64>().unwrap() > 150_000_000);
}

#[test]
fn forged_programs_are_refused() {
    let c = case("programs");
    let mut tx = c.tx.clone();
    tx.instructions.push(Ix {
        program: SQUADS_V4,
        accounts: vec![MAKER],
        data: vec![0],
    });
    c.refused(&tx, "unexpected program");

    let mut tx = c.tx.clone();
    let fill = tx.instructions[tx.fill_index()].clone();
    tx.instructions.push(fill);
    c.refused(&tx, "duplicated fill");

    let mut tx = c.tx.clone();
    tx.instructions
        .retain(|i| i.program != crate::programs::ORDER_ENGINE);
    c.refused(&tx, "missing fill");

    let mut tx = c.tx.clone();
    tx.instructions.push(Ix {
        program: SYSTEM,
        accounts: vec![c.taker.pubkey(), Address::new_from_array([66; 32])],
        data: vec![2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
    });
    c.refused(&tx, "unexpected transfer");

    let mut tx = c.tx.clone();
    tx.instructions.push(Ix {
        program: TOKEN_2022,
        accounts: vec![TAKER_BASE, BASE, TAKER_QUOTE, c.taker.pubkey()],
        data: vec![12, 1, 0, 0, 0, 0, 0, 0, 0, 9],
    });
    c.refused(&tx, "unexpected transfer");

    // Lighthouse: assertions pass, the memory write does not.
    let mut tx = c.tx.clone();
    tx.instructions.push(FillTx::lighthouse(5));
    assert!(c.run(&tx).is_ok());
    let mut tx = c.tx.clone();
    tx.instructions.push(FillTx::lighthouse(1));
    c.refused(&tx, "Lighthouse instruction");
}

#[test]
fn forged_accounts_and_associated_token_instructions_are_refused() {
    let c = case("accounts");
    let ata = |tx: &mut FillTx| {
        tx.instructions
            .iter()
            .position(|i| i.program == crate::programs::ASSOCIATED_TOKEN)
            .unwrap()
    };
    let mut tx = c.tx.clone();
    let i = ata(&mut tx);
    tx.instructions[i].accounts[0] = MAKER;
    c.refused(&tx, "must not pay");
    let mut tx = c.tx.clone();
    let i = ata(&mut tx);
    tx.instructions[i].data = vec![0];
    c.refused(&tx, "idempotent");

    // The fill's accounts, one at a time: [taker, maker, taker in, maker in, taker out,
    // maker out, input mint, input program, output mint, output program, system].
    let swap = |slot: usize, with: Address, expect: &str| {
        let mut tx = c.tx.clone();
        let i = tx.fill_index();
        tx.instructions[i].accounts[slot] = with;
        c.refused(&tx, expect);
    };
    let other = Address::new_from_array([99; 32]);
    swap(0, other, "taker does not match");
    swap(1, other, "maker does not match");
    swap(3, other, "maker input account");
    swap(5, other, "maker output account");
    swap(6, other, "input mint");
    swap(8, other, "output mint");
    swap(7, other, "input token program");
    swap(9, other, "output token program");
    swap(10, other, "system program");
    // Anchor's `None` for an optional token account: a native-SOL path.
    swap(2, crate::programs::ORDER_ENGINE, "native SOL");

    let mut tx = c.tx.clone();
    let i = tx.fill_index();
    tx.instructions[i].accounts.push(other);
    c.refused(&tx, "11 static accounts");
}

#[test]
fn forged_fill_data_is_refused() {
    let c = case("data");
    let edit = |f: &dyn Fn(&mut crate::programs::Fill), expect: &str| {
        let mut tx = c.tx.clone();
        tx.edit_fill(|fill| f(fill));
        c.refused(&tx, expect);
    };
    edit(&|f| f.output_amount += 1, "output amount");
    edit(&|f| f.input_amount -= 1, "input amount");
    edit(&|f| f.fee_bps = 3, "fee bps");
    edit(&|f| f.flags = 0, "flags");
    edit(&|f| f.flags = 3, "flags");
    edit(&|f| f.integrator_fee_bps = 1, "integrator fee");
    // Too close to land (20 s), and past the quote's life plus skew.
    edit(&|f| f.expire_at = NOW + 19, "fill expiry");
    edit(&|f| f.expire_at = NOW + 66, "fill expiry");
    edit(&|f| f.expire_at = NOW - 1, "fill expiry");

    // The toolkit README's 3-byte payload, and a wrong discriminator.
    let mut tx = c.tx.clone();
    let i = tx.fill_index();
    tx.instructions[i].data.truncate(35);
    c.refused(&tx, "37-byte payload");
    let mut tx = c.tx.clone();
    tx.instructions[i].data[0] ^= 1;
    c.refused(&tx, "37-byte payload");
}

#[test]
fn signer_rules() {
    let c = case("signers");
    // The fee payer is not the maker: the maker would not even be a signer.
    let mut tx = c.tx.clone();
    tx.fee_payer = Address::new_from_array([98; 32]);
    c.refused(&tx, "fee payer is not the maker");

    // The taker is not a signer.
    let mut tx = c.tx.clone();
    tx.other_signers = vec![Address::new_from_array([97; 32])];
    c.refused(&tx, "taker is not among the signers");

    // An extra required signature is priced, then refused by the signer count.
    let mut tx = c.tx.clone();
    tx.other_signers = vec![
        Address::new_from_array([96; 32]),
        Address::new_from_array([95; 32]),
        c.taker.pubkey(),
    ];
    c.refused(&tx, "expected 2 or 3 signers, got 4");

    // Three signers with an ATA payer is allowed.
    let mut tx = c.tx.clone();
    tx.other_signers = vec![Address::new_from_array([94; 32]), c.taker.pubkey()];
    let v = c.run(&tx).unwrap();
    assert_eq!(v.fee_lamports, 3 * 5_000 + 2_000);

    // A signature by someone else in the taker's slot.
    let forger = Keypair::new();
    let mut req = c.tx.request("q1", &c.taker);
    let mut bad = c.tx.build(&c.taker);
    bad.signatures[1] =
        <[u8; 64]>::try_from(forger.sign_message(&bad.message_bytes).as_ref()).unwrap();
    req.transaction = bad.to_base64();
    let SwapDecision::Reject(r) = c.rig.desk.swap(&req, Commit::DryRun) else {
        panic!("a forged signature must be refused");
    };
    assert_eq!(r.known, Some(RejectionReason::SignatureVerificationFailed));

    // The maker's slot already filled.
    let mut signed = c.tx.build(&c.taker);
    signed.signatures[0] = [1; 64];
    req.transaction = signed.to_base64();
    assert!(c.run_req(&req).unwrap_err().contains("already filled"));
}

#[test]
fn lookup_tables_and_malformed_transactions_are_refused() {
    let c = case("lookups");
    let mut tx = c.tx.clone();
    tx.lookups.push(crate::tx::LookupTable {
        table: Address::new_from_array([93; 32]),
        writable: vec![0],
        readonly: vec![],
    });
    c.refused(&tx, "lookup tables");

    let mut req = c.tx.request("q1", &c.taker);
    req.transaction = "not base64!".into();
    assert!(c.run_req(&req).unwrap_err().contains("not valid base64"));
    let mut bytes = c.tx.build(&c.taker).serialize();
    bytes.push(0);
    use base64::Engine as _;
    req.transaction = base64::engine::general_purpose::STANDARD.encode(&bytes);
    assert!(c.run_req(&req).unwrap_err().contains("trailing"));
}

fn cb(tx: &mut FillTx, which: u8) -> usize {
    tx.instructions
        .iter()
        .position(|i| i.program == COMPUTE_BUDGET && i.data[0] == which)
        .unwrap()
}

#[test]
fn compute_budget_instructions() {
    let c = case("cb");
    let mut tx = c.tx.clone();
    let i = cb(&mut tx, 3);
    let price = tx.instructions[i].clone();
    tx.instructions.push(price);
    c.refused(&tx, "price set twice");

    let mut tx = c.tx.clone();
    let i = cb(&mut tx, 2);
    let limit = tx.instructions[i].clone();
    tx.instructions.push(limit);
    c.refused(&tx, "limit set twice");

    for (tag, data) in [
        (1u8, vec![1, 0, 0, 1, 0]),
        (4, vec![4, 0, 0, 1, 0]),
        (0, vec![0]),
    ] {
        let mut tx = c.tx.clone();
        tx.instructions.insert(
            0,
            Ix {
                program: COMPUTE_BUDGET,
                accounts: vec![],
                data,
            },
        );
        c.refused(
            &tx,
            &format!("compute budget instruction (tag Some({tag}))"),
        );
    }

    let mut tx = c.tx.clone();
    let i = cb(&mut tx, 2);
    tx.instructions.remove(i);
    c.refused(&tx, "missing compute unit limit");
    let mut tx = c.tx.clone();
    let i = cb(&mut tx, 3);
    tx.instructions.remove(i);
    c.refused(&tx, "missing compute unit price");

    // A limit or price with the wrong length is not a limit or price.
    let mut tx = c.tx.clone();
    let i = cb(&mut tx, 3);
    tx.instructions[i].data.push(0);
    c.refused(&tx, "compute budget instruction");
}

fn with_cb(c: &Case, limit: u32, price: u64) -> FillTx {
    let mut tx = c.tx.clone();
    let i = cb(&mut tx, 2);
    tx.instructions[i].data = ComputeBudget::SetComputeUnitLimit(limit).encode();
    let i = cb(&mut tx, 3);
    tx.instructions[i].data = ComputeBudget::SetComputeUnitPrice(price).encode();
    tx
}

#[test]
fn each_fee_cap_exceeded_by_one() {
    let c = case("caps");
    // At the caps: 400,000 CU at 1,000,000 µlamports is 400,000 lamports, plus 10,000.
    assert_eq!(
        c.run(&with_cb(&c, 400_000, 1_000_000))
            .unwrap()
            .fee_lamports,
        410_000
    );
    c.refused(
        &with_cb(&c, 400_001, 1),
        "limit 400001 above the 400000 cap",
    );
    c.refused(
        &with_cb(&c, 1, 1_000_001),
        "price 1000001 above the 1000000 cap",
    );
    c.refused(&with_cb(&c, 1, u64::MAX), "above the 1000000 cap");

    // The total: a cap one lamport below the fee refuses, at the fee passes.
    let mut cfg = config("fee-cap");
    cfg.budget.max_fee_lamports = 12_000 - 1;
    // `/quote` checks the floor against the per-transaction cap; keep it satisfiable.
    let c2 = case_with(rig_with(cfg, Start::Fresh));
    c2.refused(&c2.tx, "fee 12000 lamports above the 11999 cap");
    let mut cfg = config("fee-cap-ok");
    cfg.budget.max_fee_lamports = 12_000;
    let c3 = case_with(rig_with(cfg, Start::Fresh));
    assert_eq!(c3.run(&c3.tx).unwrap().fee_lamports, 12_000);
}

#[test]
fn fee_arithmetic_is_u128_and_checked() {
    // The product overflows u64 (1.4e20) but the fee fits: exact, not wrapped.
    assert_eq!(
        transaction_fee(2, 5_000, 1_400_000, 100_000_000_000_000),
        Some(140_000_000_000_000 + 10_000)
    );
    // Rounded up: 1 CU at 1 µlamport is 1 lamport.
    assert_eq!(transaction_fee(2, 5_000, 1, 1), Some(10_001));
    assert_eq!(transaction_fee(2, 5_000, 0, 0), Some(10_000));
    // Too large for u64 after the division: refused, never wrapped or panicking.
    assert_eq!(transaction_fee(3, u64::MAX, u32::MAX, u64::MAX), None);
    assert_eq!(transaction_fee(2, 5_000, u32::MAX, u64::MAX), None);

    // Through the validator, with caps that let it get that far.
    let mut cfg = config("overflow");
    cfg.budget.max_cu_limit = u32::MAX;
    cfg.budget.max_cu_price = u64::MAX;
    cfg.budget.max_fee_lamports = u64::MAX;
    cfg.limits.max_fees_per_hour = u64::MAX;
    cfg.limits.sol_floor = 0;
    let r = rig_with(cfg, Start::Fresh);
    {
        let mut s = r.chain.snapshot.lock().unwrap();
        s.sol_lamports = u64::MAX;
        s.slot += 1;
    }
    r.desk.reconcile(&r.chain).unwrap();
    let mut c = case_with(r);
    c.tx = with_cb(&c, u32::MAX, u64::MAX);
    c.refused(&c.tx, "fee overflows");
}
