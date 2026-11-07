//! The fill-transaction validator and its fee budget (docs/design/jupiter-rfq.md, "The
//! fill-transaction validator" and "The fee budget").
//!
//! The maker would sign, as fee payer, a transaction it did not build, so this check is what
//! protects the wallet and it is total: every instruction is matched against an allowlist, and
//! anything else is refused with its reason. It reimplements the toolkit's
//! `validate_fill_sanitized_message` (`order-engine-sdk/src/fill.rs` at `3aa4fce`) from its
//! source, stricter where the design says so:
//!
//! - the fee payer is the maker, with 2 or 3 signers and the taker among them after the first;
//!   the maker's slot is still unsigned and the taker's signature verifies over the message;
//! - Compute Budget: `SetComputeUnitLimit` and `SetComputeUnitPrice` exactly once each, bounded
//!   by the [`FeeBudget`]; every other Compute Budget instruction is refused;
//! - Associated Token: idempotent create only, not paid by the maker;
//! - Lighthouse: assertions 5, 6, 9 and 10 only;
//! - Order Engine: exactly one `fill`, 37 bytes, whose taker, maker, mints, maker token
//!   accounts, token programs, amounts, `expire_at` and payload match the stored quote;
//! - System and token transfers are refused: they serve a separate receiver or an integrator
//!   fee, which the prototype does not quote;
//! - address lookup tables, native-SOL paths and Squads-wrapped transactions are refused.

use solana_address::Address;
use solana_signature::Signature;

use crate::api::QuoteType;
use crate::programs::{
    ComputeBudget, Fill, ASSOCIATED_TOKEN, COMPUTE_BUDGET, FILL_ACCOUNTS, FLAG_EXACT_OUT,
    LIGHTHOUSE, LIGHTHOUSE_ALLOWED, ORDER_ENGINE, SYSTEM, TOKEN, TOKEN_2022,
};
use crate::tx::Transaction;

/// Micro-lamports per lamport.
const MICRO_LAMPORTS: u128 = 1_000_000;

/// What the fill must match: the stored quote and the maker's own accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    /// The maker's wallet, the fee payer.
    pub maker: Address,
    /// The quote's taker.
    pub taker: Address,
    /// The mint the taker pays.
    pub input_mint: Address,
    /// The mint the maker pays.
    pub output_mint: Address,
    /// Atoms the taker pays.
    pub input_amount: u64,
    /// Atoms the maker pays.
    pub output_amount: u64,
    /// The maker's token account that receives the input.
    pub maker_input_account: Address,
    /// The maker's token account that pays the output.
    pub maker_output_account: Address,
    /// Jupiter's fee in bps, from the quote request.
    pub fee_bps: u16,
    /// The quote's type, which `flags` bit 0 repeats.
    pub quote_type: QuoteType,
    /// The earliest `expire_at` the quote allows.
    pub expire_at_min: i64,
    /// The latest `expire_at` the quote allows.
    pub expire_at_max: i64,
    /// The wall clock now.
    pub now: i64,
    /// `expire_at` must be at least this far past `now`, so the fill can still land.
    pub min_landing_secs: i64,
}

/// The fee caps (docs/design/jupiter-rfq.md, "The fee budget"). All come from config; none has
/// a built-in default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeBudget {
    /// The largest `SetComputeUnitLimit`.
    pub max_cu_limit: u32,
    /// The largest `SetComputeUnitPrice`, micro-lamports per CU.
    pub max_cu_price: u64,
    /// The most one transaction may cost the maker, lamports.
    pub max_fee_lamports: u64,
    /// The base fee per signature, lamports (5,000 today).
    pub lamports_per_signature: u64,
}

/// A fill the validator accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validated {
    /// The compute unit limit.
    pub cu_limit: u32,
    /// The compute unit price, micro-lamports per CU.
    pub cu_price: u64,
    /// What the transaction costs the maker, lamports, success or failure.
    pub fee_lamports: u64,
    /// The fill's `expire_at`.
    pub expire_at: i64,
    /// The taker's signature, which identifies the transaction until the maker signs.
    pub taker_signature: [u8; 64],
}

/// Why a transaction is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    /// The message uses address lookup tables.
    #[error("address lookup tables are not supported")]
    LookupTables,
    /// The fee payer is not the maker.
    #[error("the fee payer is not the maker")]
    FeePayer,
    /// Not 2 or 3 signers.
    #[error("expected 2 or 3 signers, got {0}")]
    SignerCount(u8),
    /// The taker is not a signer after the fee payer.
    #[error("the taker is not among the signers")]
    TakerNotSigner,
    /// The maker's signature slot is not empty.
    #[error("the maker's signature slot is already filled")]
    MakerSlotSigned,
    /// The taker's signature does not verify over the message.
    #[error("the taker's signature does not verify")]
    TakerSignature,
    /// A program outside the allowlist.
    #[error("unexpected program {0}")]
    UnexpectedProgram(Address),
    /// A Compute Budget instruction other than a limit or a price.
    #[error("unexpected compute budget instruction (tag {0:?})")]
    ComputeBudgetInstruction(Option<u8>),
    /// A second `SetComputeUnitLimit`.
    #[error("compute unit limit set twice")]
    DuplicateCuLimit,
    /// A second `SetComputeUnitPrice`.
    #[error("compute unit price set twice")]
    DuplicateCuPrice,
    /// No `SetComputeUnitLimit`.
    #[error("missing compute unit limit")]
    MissingCuLimit,
    /// No `SetComputeUnitPrice`.
    #[error("missing compute unit price")]
    MissingCuPrice,
    /// The limit is above the cap.
    #[error("compute unit limit {limit} above the {max} cap")]
    CuLimitAbove {
        /// The limit set.
        limit: u32,
        /// The cap.
        max: u32,
    },
    /// The price is above the cap.
    #[error("compute unit price {price} above the {max} cap")]
    CuPriceAbove {
        /// The price set.
        price: u64,
        /// The cap.
        max: u64,
    },
    /// The fee does not fit `u64`.
    #[error("the transaction fee overflows")]
    FeeOverflow,
    /// The fee is above the per-transaction cap.
    #[error("transaction fee {fee} lamports above the {max} cap")]
    FeeAbove {
        /// The fee.
        fee: u64,
        /// The cap.
        max: u64,
    },
    /// An associated token instruction other than idempotent create.
    #[error("only idempotent associated token account creation is allowed")]
    AssociatedTokenInstruction,
    /// The maker pays for an account creation.
    #[error("the maker must not pay for an account creation")]
    MakerPaysAccount,
    /// A Lighthouse instruction outside the assertions.
    #[error("Lighthouse instruction {0:?} is not allowed")]
    LighthouseInstruction(Option<u8>),
    /// A System or token transfer, which only a separate receiver or an integrator fee needs.
    #[error("unexpected transfer instruction from {0}")]
    UnexpectedTransfer(Address),
    /// A second `fill`.
    #[error("duplicated fill instruction")]
    DuplicateFill,
    /// No `fill`.
    #[error("missing fill instruction")]
    MissingFill,
    /// Not a 37-byte `fill` with the right discriminator.
    #[error("not a fill instruction with a 37-byte payload")]
    FillData,
    /// The `fill` does not have exactly its 11 accounts, all static.
    #[error("fill accounts are not the expected 11 static accounts")]
    FillAccounts,
    /// A native-SOL path (an optional token account left out).
    #[error("native SOL fills are not supported")]
    NativeSol,
    /// A `fill` field differs from the quote.
    #[error("fill {0} does not match the quote")]
    Mismatch(&'static str),
    /// `expire_at` is outside the quote's window or too close to land.
    #[error("fill expiry {expire_at} is outside [{min}, {max}] or too close to now")]
    Expiry {
        /// The fill's `expire_at`.
        expire_at: i64,
        /// The earliest allowed.
        min: i64,
        /// The latest allowed.
        max: i64,
    },
}

/// `signatures × lamports_per_signature + ceil(cu_limit × cu_price / 1,000,000)`, in `u128`
/// with every step checked. `None` if it does not fit `u64`.
#[must_use]
pub fn transaction_fee(
    signatures: u8,
    lamports_per_signature: u64,
    cu_limit: u32,
    cu_price: u64,
) -> Option<u64> {
    let base = u128::from(signatures).checked_mul(u128::from(lamports_per_signature))?;
    let priority = u128::from(cu_limit)
        .checked_mul(u128::from(cu_price))?
        .checked_add(MICRO_LAMPORTS.checked_sub(1)?)?
        .checked_div(MICRO_LAMPORTS)?;
    u64::try_from(base.checked_add(priority)?).ok()
}

fn static_key(tx: &Transaction, index: Option<&u8>) -> Option<Address> {
    tx.message.key(*index?).copied()
}

/// Checks `tx` against the quote and the fee budget.
///
/// # Errors
///
/// The first rule that fails, as an [`Invalid`].
pub fn validate(
    tx: &Transaction,
    expected: &Expected,
    budget: &FeeBudget,
) -> Result<Validated, Invalid> {
    let message = &tx.message;
    if !message.lookups.is_empty() {
        return Err(Invalid::LookupTables);
    }
    if message.fee_payer() != Some(&expected.maker) {
        return Err(Invalid::FeePayer);
    }
    let signers = message.header.num_required_signatures;
    if signers != 2 && signers != 3 {
        return Err(Invalid::SignerCount(signers));
    }
    // 2 signers: [maker, taker]; 3: [maker, ATA payer, taker].
    let taker_index = message
        .account_keys
        .iter()
        .take(usize::from(signers))
        .skip(1)
        .position(|k| k == &expected.taker)
        .and_then(|i| i.checked_add(1))
        .ok_or(Invalid::TakerNotSigner)?;
    if tx
        .signatures
        .first()
        .is_some_and(|s| s.iter().any(|b| *b != 0))
    {
        return Err(Invalid::MakerSlotSigned);
    }
    let taker_signature = *tx
        .signatures
        .get(taker_index)
        .ok_or(Invalid::TakerSignature)?;
    if !Signature::from(taker_signature).verify(expected.taker.as_ref(), &tx.message_bytes) {
        return Err(Invalid::TakerSignature);
    }

    let mut cu_limit = None;
    let mut cu_price = None;
    let mut fill_found = false;
    let mut expire_at = 0i64;
    for ix in &message.instructions {
        let program = message
            .key(ix.program_index)
            .copied()
            .ok_or(Invalid::FillAccounts)?;
        if program == COMPUTE_BUDGET {
            match ComputeBudget::decode(&ix.data) {
                ComputeBudget::SetComputeUnitLimit(v) => {
                    if cu_limit.replace(v).is_some() {
                        return Err(Invalid::DuplicateCuLimit);
                    }
                }
                ComputeBudget::SetComputeUnitPrice(v) => {
                    if cu_price.replace(v).is_some() {
                        return Err(Invalid::DuplicateCuPrice);
                    }
                }
                ComputeBudget::Other(tag) => return Err(Invalid::ComputeBudgetInstruction(tag)),
            }
        } else if program == ASSOCIATED_TOKEN {
            if ix.data != [1] {
                return Err(Invalid::AssociatedTokenInstruction);
            }
            let payer = static_key(tx, ix.accounts.first()).ok_or(Invalid::FillAccounts)?;
            if payer == expected.maker {
                return Err(Invalid::MakerPaysAccount);
            }
        } else if program == LIGHTHOUSE {
            let d = ix.data.first().copied();
            if !d.is_some_and(|d| LIGHTHOUSE_ALLOWED.contains(&d)) {
                return Err(Invalid::LighthouseInstruction(d));
            }
        } else if program == ORDER_ENGINE {
            if fill_found {
                return Err(Invalid::DuplicateFill);
            }
            fill_found = true;
            expire_at = check_fill(tx, ix, expected)?;
        } else if program == SYSTEM || program == TOKEN || program == TOKEN_2022 {
            return Err(Invalid::UnexpectedTransfer(program));
        } else {
            return Err(Invalid::UnexpectedProgram(program));
        }
    }
    if !fill_found {
        return Err(Invalid::MissingFill);
    }
    let cu_limit = cu_limit.ok_or(Invalid::MissingCuLimit)?;
    let cu_price = cu_price.ok_or(Invalid::MissingCuPrice)?;
    if cu_limit > budget.max_cu_limit {
        return Err(Invalid::CuLimitAbove {
            limit: cu_limit,
            max: budget.max_cu_limit,
        });
    }
    if cu_price > budget.max_cu_price {
        return Err(Invalid::CuPriceAbove {
            price: cu_price,
            max: budget.max_cu_price,
        });
    }
    let fee = transaction_fee(signers, budget.lamports_per_signature, cu_limit, cu_price)
        .ok_or(Invalid::FeeOverflow)?;
    if fee > budget.max_fee_lamports {
        return Err(Invalid::FeeAbove {
            fee,
            max: budget.max_fee_lamports,
        });
    }
    Ok(Validated {
        cu_limit,
        cu_price,
        fee_lamports: fee,
        expire_at,
        taker_signature,
    })
}

fn check_fill(
    tx: &Transaction,
    ix: &crate::tx::CompiledInstruction,
    expected: &Expected,
) -> Result<i64, Invalid> {
    let fill = Fill::decode(&ix.data).ok_or(Invalid::FillData)?;
    if ix.accounts.len() != FILL_ACCOUNTS {
        return Err(Invalid::FillAccounts);
    }
    let keys = ix
        .accounts
        .iter()
        .map(|i| static_key(tx, Some(i)))
        .collect::<Option<Vec<_>>>()
        .ok_or(Invalid::FillAccounts)?;
    let [taker, maker, taker_in, maker_in, taker_out, maker_out, input_mint, input_program, output_mint, output_program, system] =
        keys.as_slice()
    else {
        return Err(Invalid::FillAccounts);
    };
    if [taker_in, maker_in, taker_out, maker_out]
        .iter()
        .any(|a| **a == ORDER_ENGINE)
    {
        return Err(Invalid::NativeSol);
    }
    let checks: [(bool, &'static str); 11] = [
        (*taker == expected.taker, "taker"),
        (*maker == expected.maker, "maker"),
        (*input_mint == expected.input_mint, "input mint"),
        (*output_mint == expected.output_mint, "output mint"),
        (
            *maker_in == expected.maker_input_account,
            "maker input account",
        ),
        (
            *maker_out == expected.maker_output_account,
            "maker output account",
        ),
        (
            *input_program == TOKEN || *input_program == TOKEN_2022,
            "input token program",
        ),
        (
            *output_program == TOKEN || *output_program == TOKEN_2022,
            "output token program",
        ),
        (*system == SYSTEM, "system program"),
        (fill.input_amount == expected.input_amount, "input amount"),
        (
            fill.output_amount == expected.output_amount,
            "output amount",
        ),
    ];
    if let Some((_, what)) = checks.iter().find(|(ok, _)| !ok) {
        return Err(Invalid::Mismatch(what));
    }
    if fill.fee_bps != expected.fee_bps {
        return Err(Invalid::Mismatch("fee bps"));
    }
    let want_flags = match expected.quote_type {
        QuoteType::ExactIn => 0,
        QuoteType::ExactOut => FLAG_EXACT_OUT,
    };
    if fill.flags != want_flags {
        return Err(Invalid::Mismatch("flags"));
    }
    if fill.integrator_fee_bps != 0 {
        return Err(Invalid::Mismatch("integrator fee"));
    }
    let landing = expected.now.saturating_add(expected.min_landing_secs);
    if fill.expire_at < expected.expire_at_min
        || fill.expire_at > expected.expire_at_max
        || fill.expire_at < landing
    {
        return Err(Invalid::Expiry {
            expire_at: fill.expire_at,
            min: expected.expire_at_min.max(landing),
            max: expected.expire_at_max,
        });
    }
    Ok(fill.expire_at)
}
