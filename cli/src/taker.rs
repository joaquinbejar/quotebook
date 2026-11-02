//! Taker commands: request a quote, check it against the chain, settle it.

use quotebook_core::accounts::{AccountTag, MakerProfile, ProgramAccount};
use quotebook_core::BaseLots;
use quotebook_maker::client::{self, Answer, ClientError, Expectation};
use quotebook_maker::market::MarketInfo;
use quotebook_maker::protocol::{QuoteRequest, TakerSide};
use quotebook_sdk::{decode, pda, SettleAccounts};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;

use crate::market;
use crate::rpc::{clock, existing, send, Rpc};
use crate::CliError;

/// A request for `size` lots on `side` of `series`, bound to `taker`.
#[must_use]
pub fn request(
    id: u64,
    series: &Address,
    taker: &Address,
    side: TakerSide,
    size: BaseLots,
) -> QuoteRequest {
    QuoteRequest {
        request_id: id,
        series: series.to_string(),
        taker: taker.to_string(),
        taker_side: side,
        size_lots: size.get(),
    }
}

/// Asks the maker at `url`. Async: run it on a runtime, then settle outside it (the RPC
/// client is blocking).
///
/// # Errors
///
/// `Quote` if the maker refuses or the answer is malformed.
pub async fn ask(url: &str, request: &QuoteRequest) -> Result<Answer, CliError> {
    // A maker service is untrusted input and its URL may carry a key: every string in an
    // error from this exchange (transport text, protocol detail, the maker's own reject
    // message) is redacted here, at the boundary that knows the URL.
    let redactor = crate::error::Redactor::new(url);
    client::request_quote(url, request).await.map_err(|e| {
        CliError::Quote(match e {
            client::ClientError::Transport(m) => {
                client::ClientError::Transport(redactor.redact(&m))
            }
            client::ClientError::Protocol(m) => client::ClientError::Protocol(redactor.redact(&m)),
            client::ClientError::Rejected { reason, message } => client::ClientError::Rejected {
                reason,
                message: redactor.redact(&message),
            },
            other @ (client::ClientError::WouldFail(_)
            | client::ClientError::Mismatch(_)
            | client::ClientError::BadSignature) => other,
        })
    })
}

/// A maker listed in the registry (ADR 0002): its profile, decoded and validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The profile's maker: a quote from this endpoint must be signed by it.
    pub maker: Address,
    /// The profile's endpoint, `wss://…`, printable ASCII.
    pub endpoint: String,
    /// The profile's name, printable ASCII, possibly empty.
    pub name: String,
}

/// What [`discover`] found.
#[derive(Debug, Default)]
pub struct Registry {
    /// The valid profiles, allowed by the allowlist, sorted by maker.
    pub listings: Vec<Listing>,
    /// The addresses of profile-sized accounts that are not valid profiles, sorted.
    pub invalid: Vec<Address>,
    /// How many valid profiles the allowlist left out.
    pub not_allowed: usize,
}

/// Lists the maker registry: `getProgramAccounts` filtered by the profile size and tag, then
/// each account decoded and validated (owner, canonical encoding, address `["profile",
/// maker]`). Invalid accounts are reported, not used. With `allow`, only the listed makers are
/// kept.
///
/// # Errors
///
/// `Rpc`, also for an RPC that cannot list program accounts.
pub fn discover(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    allow: Option<&[Address]>,
) -> Result<Registry, CliError> {
    let size =
        u64::try_from(MakerProfile::LEN).map_err(|_| CliError::Account("profile size".into()))?;
    let accounts = rpc.program_accounts(program_id, size, AccountTag::MakerProfile as u8)?;
    let mut registry = Registry::default();
    for (address, account) in accounts {
        let listing = decode::maker_profile(program_id, &address, &account.owner, &account.data)
            .ok()
            .and_then(|p| {
                let text = |bytes: &[u8], len: u8| {
                    bytes
                        .get(..usize::from(len))
                        .and_then(|b| String::from_utf8(b.to_vec()).ok())
                };
                Some(Listing {
                    maker: p.maker,
                    endpoint: text(&p.endpoint, p.endpoint_len)?,
                    name: text(&p.name, p.name_len)?,
                })
            });
        match listing {
            None => registry.invalid.push(address),
            Some(l) if allow.is_some_and(|a| !a.contains(&l.maker)) => {
                registry.not_allowed = registry.not_allowed.saturating_add(1);
            }
            Some(l) => registry.listings.push(l),
        }
    }
    registry.listings.sort_by_key(|l| l.maker);
    registry.invalid.sort();
    Ok(registry)
}

/// Which makers a quote may come from: the identity binding of ADR 0002.
#[derive(Debug, Clone, Copy, Default)]
pub struct MakerFilter<'a> {
    /// For an endpoint found in the registry: the maker of the profile it came from.
    pub expected: Option<&'a Address>,
    /// The taker's allowlist, applied to the signed `quote.maker`.
    pub allow: Option<&'a [Address]>,
}

impl MakerFilter<'_> {
    /// Checks the signed `quote.maker` against the filter.
    ///
    /// # Errors
    ///
    /// `Quote(Mismatch)`.
    pub fn check(&self, maker: &Address) -> Result<(), CliError> {
        if self.expected.is_some_and(|e| e != maker) {
            return Err(ClientError::Mismatch("quote.maker is not the profile's maker").into());
        }
        if self.allow.is_some_and(|a| !a.contains(maker)) {
            return Err(ClientError::Mismatch("quote.maker is not on the allowlist").into());
        }
        Ok(())
    }
}

/// Checks `answer` against the series read from chain and the cluster clock: the maker filter,
/// the signature and every rule `settle` applies to the quote's terms, then the maker's token
/// accounts, which come unsigned (program, mint, owner `quote.maker`, state, and the delegate
/// and allowance on the paying side). Returns the market for [`settle`].
///
/// # Errors
///
/// `Quote` for the maker filter, or with the program error `settle` would return; the chain
/// reads.
#[allow(clippy::too_many_arguments)]
pub fn check(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    series: &Address,
    answer: &Answer,
    taker: &Address,
    side: TakerSide,
    size: BaseLots,
    makers: &MakerFilter<'_>,
) -> Result<MarketInfo, CliError> {
    makers.check(&answer.signed.quote.terms().maker)?;
    let market = market::market(rpc, program_id, series)?;
    let now = clock(rpc)?;
    client::verify(
        &answer.signed,
        &Expectation {
            market,
            taker: *taker,
            taker_side: side,
            size,
            now,
        },
    )?;
    check_maker_accounts(rpc, program_id, series, answer, &market, size)?;
    Ok(market)
}

/// The answers to one request from several makers, each paired with the profile maker its
/// endpoint came from.
pub type Asked = Vec<(Address, Result<Answer, CliError>)>;

/// Asks each `(profile maker, endpoint)` in turn. Async, like [`ask`].
pub async fn ask_each(endpoints: &[(Address, String)], request: &QuoteRequest) -> Asked {
    let mut asked = Vec::with_capacity(endpoints.len());
    for (maker, url) in endpoints {
        asked.push((*maker, ask(url, request).await));
    }
    asked
}

/// The result of [`choose`].
#[derive(Debug, Default)]
pub struct Choice {
    /// The best checked quote, its profile maker and the market for [`settle`].
    pub best: Option<(Address, Answer, MarketInfo)>,
    /// The profile makers whose answer failed, and why.
    pub rejected: Vec<(Address, CliError)>,
}

/// Checks every answer with [`check`], each bound to the maker of the profile its endpoint came
/// from and to `allow`, and keeps the best price for the taker: the lowest when buying, the
/// highest when selling; the first on a tie.
///
/// # Errors
///
/// None of its own: a failing answer goes to `rejected`.
#[allow(clippy::too_many_arguments)]
pub fn choose(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    series: &Address,
    asked: Asked,
    taker: &Address,
    side: TakerSide,
    size: BaseLots,
    allow: Option<&[Address]>,
) -> Choice {
    let mut choice = Choice::default();
    for (maker, answer) in asked {
        let filter = MakerFilter {
            expected: Some(&maker),
            allow,
        };
        let checked = answer.and_then(|a| {
            check(rpc, program_id, series, &a, taker, side, size, &filter).map(|m| (a, m))
        });
        match checked {
            Err(e) => choice.rejected.push((maker, e)),
            Ok((answer, market)) => {
                let price = answer.signed.quote.terms().price.get();
                let better = choice.best.as_ref().is_none_or(|(_, b, _)| {
                    let best = b.signed.quote.terms().price.get();
                    match side {
                        TakerSide::Buy => price < best,
                        TakerSide::Sell => price > best,
                    }
                });
                if better {
                    choice.best = Some((maker, answer, market));
                }
            }
        }
    }
    choice
}

/// What a maker token account is for in this fill.
#[derive(Debug, Clone, Copy)]
enum Leg {
    /// The maker pays this many atoms from it, through its delegate PDA.
    Pays(u64),
    /// The maker receives into it.
    Receives,
}

/// The maker's token accounts come with the answer, unsigned. Before building `settle`, check
/// each one on chain: the mint's token program owns it, it holds the series' mint, the
/// quote's maker owns it and it is initialized; and the one the maker pays from has the
/// maker's delegate PDA approved for at least the amount, with the balance to cover it.
fn check_maker_accounts(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    series: &Address,
    answer: &Answer,
    market: &MarketInfo,
    size: BaseLots,
) -> Result<(), CliError> {
    let t = answer.signed.quote.terms();
    let s = market::series(rpc, program_id, series)?;
    let overflow = || CliError::Account("fill amount overflows".into());
    let base = size
        .get()
        .checked_mul(market.base_lot_size)
        .ok_or_else(overflow)?;
    let quote = u128::from(t.price.get())
        .checked_mul(u128::from(size.get()))
        .and_then(|v| v.checked_mul(u128::from(market.quote_lot_size)))
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(overflow)?;
    let (base_leg, quote_leg) = match t.side {
        quotebook_core::Side::Ask => (Leg::Pays(base), Leg::Receives),
        quotebook_core::Side::Bid => (Leg::Receives, Leg::Pays(quote)),
    };
    let delegate = quotebook_sdk::pda::delegate(program_id, &t.maker).0;
    check_maker_account(
        rpc,
        "base",
        &answer.maker_base,
        &s.base_mint,
        &t.maker,
        &delegate,
        base_leg,
    )?;
    check_maker_account(
        rpc,
        "quote",
        &answer.maker_quote,
        &s.quote_mint,
        &t.maker,
        &delegate,
        quote_leg,
    )
}

fn check_maker_account(
    rpc: &mut dyn Rpc,
    which: &str,
    account: &Address,
    mint: &Address,
    maker: &Address,
    delegate: &Address,
    leg: Leg,
) -> Result<(), CliError> {
    let bad = |why: &str| CliError::Account(format!("maker {which} account {account}: {why}"));
    let token_program = existing(rpc, mint)?.owner;
    let a = rpc.account(account)?.ok_or_else(|| bad("does not exist"))?;
    if a.owner != token_program {
        return Err(bad("not owned by the mint's token program"));
    }
    let field = |range: std::ops::Range<usize>| a.data.get(range);
    if field(0..32) != Some(mint.as_ref()) {
        return Err(bad("holds another mint"));
    }
    if field(32..64) != Some(maker.as_ref()) {
        return Err(bad("not owned by the quote's maker"));
    }
    if a.data.get(108) != Some(&1) {
        return Err(bad("not initialized, or frozen"));
    }
    if let Leg::Pays(needed) = leg {
        let u64_at = |range: std::ops::Range<usize>| {
            field(range)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_le_bytes)
        };
        if u64_at(64..72).is_none_or(|amount| amount < needed) {
            return Err(bad("balance below the fill"));
        }
        let has_delegate = field(72..76) == Some(&1u32.to_le_bytes()[..])
            && field(76..108) == Some(delegate.as_ref());
        if !has_delegate {
            return Err(bad("the maker's delegate PDA is not approved"));
        }
        if u64_at(121..129).is_none_or(|allowance| allowance < needed) {
            return Err(bad("delegate allowance below the fill"));
        }
    }
    Ok(())
}

/// Sends `[ed25519 precompile, settle]` for a checked answer.
///
/// # Errors
///
/// The chain reads and the RPC errors, including the program's rejection.
#[allow(clippy::too_many_arguments)]
pub fn settle(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    answer: &Answer,
    taker: &Keypair,
    payer: &Keypair,
    taker_base: &Address,
    taker_quote: &Address,
    size: BaseLots,
) -> Result<Signature, CliError> {
    let series = market::series(
        rpc,
        program_id,
        &answer.signed.quote.terms().series.address(),
    )?;
    let base_token_program = existing(rpc, &series.base_mint)?.owner;
    let quote_token_program = existing(rpc, &series.quote_mint)?.owner;
    let accounts = SettleAccounts {
        taker: taker.pubkey(),
        base_mint: series.base_mint,
        quote_mint: series.quote_mint,
        base_token_program,
        quote_token_program,
        maker_base: answer.maker_base,
        maker_quote: answer.maker_quote,
        taker_base: *taker_base,
        taker_quote: *taker_quote,
        // A series with a fee needs the fee account: the Config PDA's canonical one (ADR 0001).
        fee_account: (series.fee_bps() > 0)
            .then(|| pda::fee_account(program_id, &series.quote_mint, &quote_token_program)),
    };
    let pair = quotebook_sdk::settle_instructions(
        program_id,
        &answer.signed.quote,
        &answer.signed.signature,
        &accounts,
        size,
        0,
    );
    send(rpc, &pair, payer, &[taker])
}
