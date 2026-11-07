//! The risk book's view of devnet: [`RpcChainView`], the RPC implementation of
//! `quotebook_maker::risk::ChainView` (`docs/PLAN-phase2.md`, D1 and D3).
//!
//! # Snapshot
//!
//! One `getMultipleAccounts` at `finalized` reads the Clock sysvar, the maker's `MakerState` and
//! every maker token account, so all of them are from the one slot the RPC returns. Every account
//! is checked before it counts:
//!
//! - the Clock: owned by the sysvar program, long enough, and its slot not above the read's;
//! - `MakerState`: owned by the quotebook program, a valid `MakerState`, for this maker;
//! - a token account: owned by SPL Token or Token-2022, initialized, of the expected mint, owned
//!   by the maker. Only an allowance to the maker's delegate PDA counts as `delegated_amount`; a
//!   frozen account counts as nothing usable. An absent account has nothing available.
//!
//! Anything else fails the whole read, so the book refuses rather than quote on a wrong account.
//!
//! # Fill history
//!
//! [`ChainView::fill_times_since`] walks `getSignaturesForAddress` on the `MakerState` at
//! `finalized`, newest first, page by page, and reads each successful transaction with
//! `getTransaction`, until a block time at or below `since`. It counts every `settle` of the
//! quotebook program, inner instructions included. A missing transaction, one without metadata or
//! inner instructions, an undecodable instruction, or more pages than allowed is an error: the
//! book then refuses for the hour. Errors carry no RPC URL (`quotebook_cli::rpc::Cluster` redacts
//! them).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use quotebook_cli::rpc::{Rpc, SignatureInfo};
use quotebook_core::accounts::{self, MakerState};
use quotebook_core::UnixTimestamp;
use quotebook_maker::risk::{
    settle_count, ChainError, ChainSnapshot, ChainView, TokenAccountState,
};
use quotebook_maker::server::Inventory;
use rust_decimal::Decimal;
use serde_json::Value;
use solana_account::Account;
use solana_address::Address;

/// SPL Token.
pub const TOKEN_PROGRAM: Address =
    solana_address::address!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022.
pub const TOKEN_2022_PROGRAM: Address =
    solana_address::address!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// The Clock sysvar.
pub const CLOCK_SYSVAR: Address =
    solana_address::address!("SysvarC1ock11111111111111111111111111111111");
/// The sysvar program, owner of every sysvar.
pub const SYSVAR_PROGRAM: Address =
    solana_address::address!("Sysvar1111111111111111111111111111111111111");

/// `getMultipleAccounts` takes at most this many addresses.
pub const MAX_ACCOUNTS_PER_READ: usize = 100;
/// Signatures per `getSignaturesForAddress` page, the RPC's maximum.
pub const HISTORY_PAGE: usize = 1_000;
/// Pages walked before the history counts as unreadable.
pub const MAX_HISTORY_PAGES: usize = 10;

/// The SPL token account layout: 165 bytes, also the base of a Token-2022 account.
const TOKEN_ACCOUNT_LEN: usize = 165;
/// Token-2022's `AccountType::Account`, the byte after the base layout when extensions follow.
const TOKEN_2022_ACCOUNT_TYPE: u8 = 2;

fn err(what: impl Into<String>) -> ChainError {
    ChainError(what.into())
}

/// A maker token account the snapshot reads, and the mint it must hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchedAccount {
    /// The token account.
    pub address: Address,
    /// Its mint.
    pub mint: Address,
}

/// The maker's balances from the latest accepted snapshot, shared with the pricing inventory.
#[derive(Debug, Default)]
pub struct Balances(Mutex<HashMap<Address, u64>>);

impl Balances {
    fn store(&self, accounts: &[(Address, TokenAccountState)]) {
        if let Ok(mut map) = self.0.lock() {
            map.clear();
            map.extend(accounts.iter().map(|(a, s)| (*a, s.amount)));
        }
    }

    /// The balance of `account`, in atoms; `None` before a snapshot.
    #[must_use]
    pub fn amount(&self, account: &Address) -> Option<u64> {
        self.0.lock().ok()?.get(account).copied()
    }
}

/// A market's position for the pricing model: the base account's finalized balance in tokens.
#[derive(Debug, Clone)]
pub struct BalanceInventory {
    /// The shared balances.
    pub balances: Arc<Balances>,
    /// The maker's base account.
    pub account: Address,
    /// The base mint's decimals.
    pub decimals: u8,
}

impl Inventory for BalanceInventory {
    fn inventory(&self) -> Decimal {
        // Before the first snapshot nothing is quoted anyway (the book has no snapshot).
        let atoms = self.balances.amount(&self.account).unwrap_or(0);
        let mut value = Decimal::from(atoms);
        // Scale down by the decimals; an out-of-range scale leaves the raw atoms, which only
        // moves the skew, inside its bps cap.
        if value.set_scale(u32::from(self.decimals)).is_err() {
            value = Decimal::from(atoms);
        }
        value
    }
}

/// [`ChainView`] over an RPC. The RPC is behind a lock, since the book's reads take `&self`.
pub struct RpcChainView<R> {
    rpc: Mutex<R>,
    program_id: Address,
    maker: Address,
    maker_state: Address,
    delegate: Address,
    accounts: Vec<WatchedAccount>,
    balances: Arc<Balances>,
    max_pages: usize,
    page: usize,
}

impl<R> std::fmt::Debug for RpcChainView<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcChainView")
            .field("maker", &self.maker)
            .field("accounts", &self.accounts)
            .finish_non_exhaustive()
    }
}

impl<R: Rpc> RpcChainView<R> {
    /// A view of `maker` on `program_id`, reading `accounts`.
    ///
    /// # Errors
    ///
    /// When the accounts do not fit one `getMultipleAccounts`.
    pub fn new(
        rpc: R,
        program_id: Address,
        maker: Address,
        accounts: Vec<WatchedAccount>,
    ) -> Result<Self, ChainError> {
        if accounts.len().saturating_add(2) > MAX_ACCOUNTS_PER_READ {
            return Err(err("too many token accounts for one read"));
        }
        Ok(Self {
            rpc: Mutex::new(rpc),
            program_id,
            maker,
            maker_state: quotebook_sdk::pda::maker_state(&program_id, &maker).0,
            delegate: quotebook_sdk::pda::delegate(&program_id, &maker).0,
            accounts,
            balances: Arc::new(Balances::default()),
            max_pages: MAX_HISTORY_PAGES,
            page: HISTORY_PAGE,
        })
    }

    /// Smaller pages and a lower page limit, for tests of the walk.
    #[must_use]
    pub fn with_paging(mut self, page: usize, max_pages: usize) -> Self {
        self.page = page.max(1);
        self.max_pages = max_pages;
        self
    }

    /// The balances of the latest accepted snapshot.
    #[must_use]
    pub fn balances(&self) -> Arc<Balances> {
        Arc::clone(&self.balances)
    }

    /// The maker's `MakerState` address.
    #[must_use]
    pub const fn maker_state(&self) -> Address {
        self.maker_state
    }

    /// Edits the RPC in place, for tests that change the chain under a running view.
    #[cfg(test)]
    pub(crate) fn with_rpc(&self, f: impl FnOnce(&mut R)) {
        if let Ok(mut rpc) = self.rpc.lock() {
            f(&mut rpc);
        }
    }

    fn rpc(&self) -> Result<std::sync::MutexGuard<'_, R>, ChainError> {
        self.rpc.lock().map_err(|_| err("rpc lock poisoned"))
    }

    fn read(&self) -> Result<ChainSnapshot, ChainError> {
        let mut addresses = vec![CLOCK_SYSVAR, self.maker_state];
        addresses.extend(self.accounts.iter().map(|a| a.address));
        let (slot, got) = self
            .rpc()?
            .finalized_accounts(&addresses)
            .map_err(|e| err(e.to_string()))?;
        if got.len() != addresses.len() {
            return Err(err(
                "getMultipleAccounts returned the wrong number of accounts",
            ));
        }
        let mut got = got.into_iter();
        let clock = parse_clock(got.next().flatten().as_ref(), slot)?;
        let state =
            parse_maker_state(got.next().flatten().as_ref(), &self.program_id, &self.maker)?;
        let mut accounts = Vec::with_capacity(self.accounts.len());
        for (watched, account) in self.accounts.iter().zip(got) {
            let state =
                parse_token_account(account.as_ref(), watched, &self.maker, &self.delegate)?;
            accounts.push((watched.address, state));
        }
        Ok(ChainSnapshot {
            slot,
            clock,
            window: state.nonces,
            accounts,
        })
    }
}

impl<R: Rpc + Send> ChainView for RpcChainView<R> {
    fn snapshot(&self) -> Result<ChainSnapshot, ChainError> {
        let snapshot = self.read()?;
        self.balances.store(&snapshot.accounts);
        Ok(snapshot)
    }

    fn fill_times_since(
        &self,
        since: UnixTimestamp,
    ) -> Result<Vec<Option<UnixTimestamp>>, ChainError> {
        let since = since.get();
        let mut times = Vec::new();
        let mut before: Option<String> = None;
        for _ in 0..self.max_pages {
            let page: Vec<SignatureInfo> = self
                .rpc()?
                .finalized_signatures(&self.maker_state, before.as_deref(), self.page)
                .map_err(|e| err(e.to_string()))?;
            for entry in &page {
                if entry.block_time.is_some_and(|t| t <= since) {
                    return Ok(times);
                }
                if entry.failed {
                    continue;
                }
                let tx = self
                    .rpc()?
                    .finalized_transaction(&entry.signature)
                    .map_err(|e| err(e.to_string()))?
                    .ok_or_else(|| err("a listed transaction is missing"))?;
                let fills = settles_in(&tx, &self.program_id)?;
                let time = entry.block_time.or(fills.block_time);
                if time.is_some_and(|t| t <= since) {
                    return Ok(times);
                }
                let count = usize::try_from(fills.count).map_err(|_| err("too many settles"))?;
                times.extend(std::iter::repeat_n(time.map(UnixTimestamp::new), count));
            }
            if page.len() < self.page {
                return Ok(times);
            }
            before = Some(
                page.last()
                    .map(|e| e.signature.clone())
                    .ok_or_else(|| err("empty full page"))?,
            );
        }
        Err(err("fill history longer than the page limit"))
    }
}

fn le_u64(data: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    let bytes: [u8; 8] = data.get(at..end)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

fn address_at(data: &[u8], at: usize) -> Option<Address> {
    let end = at.checked_add(32)?;
    let bytes: [u8; 32] = data.get(at..end)?.try_into().ok()?;
    Some(Address::new_from_array(bytes))
}

/// The Clock's `unix_timestamp`, checked against the read's slot.
pub(crate) fn parse_clock(
    account: Option<&Account>,
    slot: u64,
) -> Result<UnixTimestamp, ChainError> {
    let account = account.ok_or_else(|| err("clock sysvar missing"))?;
    if account.owner != SYSVAR_PROGRAM {
        return Err(err("clock sysvar has the wrong owner"));
    }
    let clock_slot = le_u64(&account.data, 0).ok_or_else(|| err("clock sysvar too short"))?;
    let unix = le_u64(&account.data, 32).ok_or_else(|| err("clock sysvar too short"))?;
    if clock_slot > slot {
        return Err(err("clock slot above the read's slot"));
    }
    let unix = i64::from_le_bytes(unix.to_le_bytes());
    Ok(UnixTimestamp::new(unix))
}

fn parse_maker_state(
    account: Option<&Account>,
    program_id: &Address,
    maker: &Address,
) -> Result<MakerState, ChainError> {
    let account = account.ok_or_else(|| err("MakerState missing: run init-maker"))?;
    if account.owner != *program_id {
        return Err(err("MakerState has the wrong owner"));
    }
    let state: MakerState =
        accounts::decode(&account.data).ok_or_else(|| err("not a MakerState"))?;
    if state.maker != *maker {
        return Err(err("MakerState of another maker"));
    }
    Ok(state)
}

/// What one token account makes available. See the module documentation.
///
/// # Errors
///
/// A wrong owner program, length, mint, owner or state.
pub fn parse_token_account(
    account: Option<&Account>,
    watched: &WatchedAccount,
    maker: &Address,
    delegate: &Address,
) -> Result<TokenAccountState, ChainError> {
    let Some(account) = account else {
        return Ok(TokenAccountState::default());
    };
    let what = |problem: &str| err(format!("token account {}: {problem}", watched.address));
    let data = &account.data;
    if account.owner == TOKEN_PROGRAM {
        if data.len() != TOKEN_ACCOUNT_LEN {
            return Err(what("wrong length"));
        }
    } else if account.owner == TOKEN_2022_PROGRAM {
        if data.len() < TOKEN_ACCOUNT_LEN {
            return Err(what("too short"));
        }
        if data.len() > TOKEN_ACCOUNT_LEN
            && data.get(TOKEN_ACCOUNT_LEN) != Some(&TOKEN_2022_ACCOUNT_TYPE)
        {
            return Err(what("not a token account"));
        }
    } else {
        return Err(what("not owned by a token program"));
    }
    if address_at(data, 0) != Some(watched.mint) {
        return Err(what("wrong mint"));
    }
    if address_at(data, 32) != Some(*maker) {
        return Err(what("not owned by the maker"));
    }
    let amount = le_u64(data, 64).ok_or_else(|| what("too short"))?;
    let delegate_tag = data
        .get(72..76)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| what("too short"))?;
    let approved = match delegate_tag {
        0 => false,
        1 => address_at(data, 76) == Some(*delegate),
        _ => return Err(what("invalid delegate option")),
    };
    let usable = match data.get(108) {
        Some(1) => true,
        Some(2) => false,
        _ => return Err(what("not initialized")),
    };
    let delegated = le_u64(data, 121).ok_or_else(|| what("too short"))?;
    Ok(TokenAccountState {
        amount,
        delegated_amount: if approved && usable { delegated } else { 0 },
    })
}

/// The `settle`s of one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settles {
    /// The transaction's block time, if the node has it.
    pub block_time: Option<i64>,
    /// How many `settle` instructions of the program it holds, inner ones included; 0 if it
    /// failed.
    pub count: u64,
}

/// Counts the `settle`s of `program_id` in a `getTransaction` result (JSON encoding).
///
/// # Errors
///
/// A missing field, an account index out of range or data that is not base58.
pub fn settles_in(tx: &Value, program_id: &Address) -> Result<Settles, ChainError> {
    let block_time = match tx.get("blockTime") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .ok_or_else(|| err("blockTime is not an integer"))?,
        ),
    };
    let meta = tx
        .get("meta")
        .filter(|m| !m.is_null())
        .ok_or_else(|| err("transaction without metadata"))?;
    if meta.get("err").is_some_and(|e| !e.is_null()) {
        return Ok(Settles {
            block_time,
            count: 0,
        });
    }
    let message = tx
        .get("transaction")
        .and_then(|t| t.get("message"))
        .ok_or_else(|| err("transaction without a message"))?;
    let mut keys = addresses(message.get("accountKeys"))?;
    if let Some(loaded) = meta.get("loadedAddresses").filter(|l| !l.is_null()) {
        keys.extend(addresses(loaded.get("writable"))?);
        keys.extend(addresses(loaded.get("readonly"))?);
    }
    let mut instructions: Vec<(Address, Vec<u8>)> = Vec::new();
    for ix in array(message.get("instructions"))? {
        instructions.push(instruction(ix, &keys)?);
    }
    let inner = meta
        .get("innerInstructions")
        .filter(|i| !i.is_null())
        .ok_or_else(|| err("inner instructions not recorded"))?;
    for group in array(Some(inner))? {
        for ix in array(group.get("instructions"))? {
            instructions.push(instruction(ix, &keys)?);
        }
    }
    let count = settle_count(
        program_id,
        instructions.iter().map(|(p, d)| (p, d.as_slice())),
    );
    Ok(Settles { block_time, count })
}

fn array(value: Option<&Value>) -> Result<&Vec<Value>, ChainError> {
    value
        .and_then(Value::as_array)
        .ok_or_else(|| err("expected an array"))
}

fn addresses(value: Option<&Value>) -> Result<Vec<Address>, ChainError> {
    array(value)?
        .iter()
        .map(|v| {
            v.as_str()
                .and_then(|s| s.parse::<Address>().ok())
                .ok_or_else(|| err("not an address"))
        })
        .collect()
}

fn instruction(ix: &Value, keys: &[Address]) -> Result<(Address, Vec<u8>), ChainError> {
    let index = ix
        .get("programIdIndex")
        .and_then(Value::as_u64)
        .and_then(|i| usize::try_from(i).ok())
        .ok_or_else(|| err("instruction without programIdIndex"))?;
    let program = *keys
        .get(index)
        .ok_or_else(|| err("programIdIndex out of range"))?;
    let data = ix
        .get("data")
        .and_then(Value::as_str)
        .and_then(bs58_decode)
        .ok_or_else(|| err("instruction data is not base58"))?;
    Ok((program, data))
}

const BS58_ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Decodes base58 (Bitcoin alphabet); `None` for any other character.
#[must_use]
pub fn bs58_decode(text: &str) -> Option<Vec<u8>> {
    // Little-endian base-256 digits of the number read so far.
    let mut bytes: Vec<u8> = Vec::new();
    for c in text.bytes() {
        let digit = BS58_ALPHABET.iter().position(|a| *a == c)?;
        let mut carry = u32::try_from(digit).ok()?;
        for b in &mut bytes {
            let v = u32::from(*b).checked_mul(58)?.checked_add(carry)?;
            *b = u8::try_from(v.checked_rem(256)?).ok()?;
            carry = v.checked_div(256)?;
        }
        while carry > 0 {
            bytes.push(u8::try_from(carry.checked_rem(256)?).ok()?);
            carry = carry.checked_div(256)?;
        }
    }
    // Each leading '1' is a leading zero byte.
    let zeros = text.bytes().take_while(|c| *c == b'1').count();
    bytes.extend(std::iter::repeat_n(0, zeros));
    bytes.reverse();
    Some(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::disallowed_macros,
        clippy::indexing_slicing,
        clippy::integer_division,
        clippy::unwrap_used
    )]

    use std::collections::VecDeque;

    use quotebook_cli::CliError;
    use quotebook_core::accounts::{new_hdr, MakerState};
    use quotebook_core::{Nonce, NonceWindow};
    use serde_json::json;

    use super::*;

    pub(crate) const PROGRAM: Address = Address::new_from_array([7; 32]);
    pub(crate) const MAKER: Address = Address::new_from_array([8; 32]);
    pub(crate) const USDC: Address = Address::new_from_array([9; 32]);
    pub(crate) const BASE: Address = Address::new_from_array([10; 32]);
    pub(crate) const USDC_ACCOUNT: Address = Address::new_from_array([11; 32]);
    pub(crate) const BASE_ACCOUNT: Address = Address::new_from_array([12; 32]);
    const SETTLE: u8 = quotebook_core::instruction::Tag::Settle as u8;

    fn bs58_encode(bytes: &[u8]) -> String {
        let mut digits: Vec<u8> = Vec::new();
        for b in bytes {
            let mut carry = u32::from(*b);
            for d in &mut digits {
                let v = u32::from(*d) * 256 + carry;
                *d = (v % 58) as u8;
                carry = v / 58;
            }
            while carry > 0 {
                digits.push((carry % 58) as u8);
                carry /= 58;
            }
        }
        let zeros = bytes.iter().take_while(|b| **b == 0).count();
        let mut out: String = "1".repeat(zeros);
        out.extend(
            digits
                .iter()
                .rev()
                .map(|d| BS58_ALPHABET[usize::from(*d)] as char),
        );
        out
    }

    #[test]
    fn base58_round_trips() {
        for bytes in [
            vec![],
            vec![0],
            vec![0, 0, 1],
            vec![SETTLE],
            vec![SETTLE, 0, 0, 255, 17],
            (0..=255u8).collect::<Vec<_>>(),
        ] {
            assert_eq!(bs58_decode(&bs58_encode(&bytes)).unwrap(), bytes);
        }
        // A known address: the bytes of the Clock sysvar.
        assert_eq!(
            bs58_decode("SysvarC1ock11111111111111111111111111111111").unwrap(),
            CLOCK_SYSVAR.to_bytes().to_vec()
        );
        assert_eq!(bs58_decode("0OIl"), None);
    }

    pub(crate) fn token_account(mint: Address, owner: Address, amount: u64) -> Vec<u8> {
        let mut data = vec![0u8; TOKEN_ACCOUNT_LEN];
        data[0..32].copy_from_slice(mint.as_ref());
        data[32..64].copy_from_slice(owner.as_ref());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data[108] = 1;
        data
    }

    pub(crate) fn approve(data: &mut [u8], delegate: Address, amount: u64) {
        data[72..76].copy_from_slice(&1u32.to_le_bytes());
        data[76..108].copy_from_slice(delegate.as_ref());
        data[121..129].copy_from_slice(&amount.to_le_bytes());
    }

    pub(crate) fn account(owner: Address, data: Vec<u8>) -> Account {
        Account {
            lamports: 1,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        }
    }

    pub(crate) fn maker_state_address() -> Address {
        quotebook_sdk::pda::maker_state(&PROGRAM, &MAKER).0
    }

    pub(crate) fn delegate() -> Address {
        quotebook_sdk::pda::delegate(&PROGRAM, &MAKER).0
    }

    pub(crate) fn watched(address: Address, mint: Address) -> WatchedAccount {
        WatchedAccount { address, mint }
    }

    #[test]
    fn a_token_account_counts_only_the_delegate_pda_allowance() {
        let w = watched(USDC_ACCOUNT, USDC);
        let mut data = token_account(USDC, MAKER, 5_000_000);
        let read = |data: &Vec<u8>, owner| {
            parse_token_account(Some(&account(owner, data.clone())), &w, &MAKER, &delegate())
        };
        assert_eq!(
            read(&data, TOKEN_PROGRAM).unwrap(),
            TokenAccountState {
                amount: 5_000_000,
                delegated_amount: 0
            },
            "no delegate"
        );
        approve(&mut data, delegate(), 4_000_000);
        assert_eq!(
            read(&data, TOKEN_PROGRAM).unwrap().delegated_amount,
            4_000_000
        );
        assert_eq!(
            read(&data, TOKEN_2022_PROGRAM).unwrap().delegated_amount,
            4_000_000
        );
        let mut other = data.clone();
        approve(&mut other, Address::new_from_array([99; 32]), 4_000_000);
        assert_eq!(
            read(&other, TOKEN_PROGRAM).unwrap().delegated_amount,
            0,
            "another delegate"
        );
        let mut frozen = data.clone();
        frozen[108] = 2;
        assert_eq!(
            read(&frozen, TOKEN_PROGRAM).unwrap(),
            TokenAccountState {
                amount: 5_000_000,
                delegated_amount: 0
            },
            "frozen"
        );
        assert_eq!(
            parse_token_account(None, &w, &MAKER, &delegate()).unwrap(),
            TokenAccountState::default(),
            "absent"
        );
    }

    #[test]
    fn a_wrong_token_account_fails_the_read() {
        let w = watched(USDC_ACCOUNT, USDC);
        let read = |owner, data: Vec<u8>| {
            parse_token_account(Some(&account(owner, data)), &w, &MAKER, &delegate())
        };
        let good = token_account(USDC, MAKER, 1);
        assert!(
            read(Address::new_from_array([1; 32]), good.clone()).is_err(),
            "owner program"
        );
        assert!(
            read(TOKEN_PROGRAM, token_account(BASE, MAKER, 1)).is_err(),
            "mint"
        );
        assert!(
            read(TOKEN_PROGRAM, token_account(USDC, USDC, 1)).is_err(),
            "owner"
        );
        let mut uninit = good.clone();
        uninit[108] = 0;
        assert!(read(TOKEN_PROGRAM, uninit).is_err(), "uninitialized");
        let mut long = good.clone();
        long.push(0);
        assert!(
            read(TOKEN_PROGRAM, long.clone()).is_err(),
            "SPL Token length"
        );
        assert!(read(TOKEN_2022_PROGRAM, long).is_err(), "2022 account type");
        let mut ext = good.clone();
        ext.extend([TOKEN_2022_ACCOUNT_TYPE, 0, 0]);
        assert!(
            read(TOKEN_2022_PROGRAM, ext).is_ok(),
            "2022 with extensions"
        );
        let mut tag = good;
        tag[72] = 2;
        assert!(read(TOKEN_PROGRAM, tag).is_err(), "delegate option");
    }

    pub(crate) fn maker_state(maker: Address, window: NonceWindow) -> Vec<u8> {
        let state = MakerState {
            hdr: new_hdr::<MakerState>(255),
            maker,
            fills: 0,
            delegate_bump: 255,
            _pad: [0; 7],
            nonces: window,
        };
        bytemuck::bytes_of(&state).to_vec()
    }

    pub(crate) fn clock(slot: u64, unix: i64) -> Vec<u8> {
        let mut data = vec![0u8; 40];
        data[0..8].copy_from_slice(&slot.to_le_bytes());
        data[32..40].copy_from_slice(&unix.to_le_bytes());
        data
    }

    /// A chain held in memory: the accounts at one slot, signature pages and transactions.
    #[derive(Default)]
    pub(crate) struct FakeRpc {
        pub(crate) slot: u64,
        pub(crate) accounts: HashMap<Address, Account>,
        pub(crate) fail: bool,
        pub(crate) signatures: Vec<SignatureInfo>,
        pub(crate) transactions: HashMap<String, Value>,
        pub(crate) pages: Arc<Mutex<VecDeque<Option<String>>>>,
    }

    impl Rpc for FakeRpc {
        fn account(&mut self, _: &Address) -> Result<Option<Account>, CliError> {
            Err(CliError::Rpc("confirmed reads unused".into()))
        }
        fn latest_blockhash(&mut self) -> Result<solana_hash::Hash, CliError> {
            Err(CliError::Rpc("unused".into()))
        }
        fn send(
            &mut self,
            _: &solana_transaction::Transaction,
        ) -> Result<solana_signature::Signature, CliError> {
            Err(CliError::Rpc("unused".into()))
        }
        fn genesis_hash(&mut self) -> Result<solana_hash::Hash, CliError> {
            Err(CliError::Rpc("unused".into()))
        }
        fn finalized_accounts(
            &mut self,
            addresses: &[Address],
        ) -> Result<(u64, Vec<Option<Account>>), CliError> {
            if self.fail {
                return Err(CliError::Rpc("down".into()));
            }
            Ok((
                self.slot,
                addresses
                    .iter()
                    .map(|a| self.accounts.get(a).cloned())
                    .collect(),
            ))
        }
        fn finalized_signatures(
            &mut self,
            _: &Address,
            before: Option<&str>,
            limit: usize,
        ) -> Result<Vec<SignatureInfo>, CliError> {
            if let Ok(mut pages) = self.pages.lock() {
                pages.push_back(before.map(str::to_owned));
            }
            let start = before.map_or(0, |b| {
                self.signatures
                    .iter()
                    .position(|s| s.signature == b)
                    .map_or(self.signatures.len(), |i| i + 1)
            });
            Ok(self
                .signatures
                .iter()
                .skip(start)
                .take(limit)
                .cloned()
                .collect())
        }
        fn finalized_transaction(&mut self, signature: &str) -> Result<Option<Value>, CliError> {
            Ok(self.transactions.get(signature).cloned())
        }
    }

    pub(crate) fn chain(window: NonceWindow) -> FakeRpc {
        let mut rpc = FakeRpc {
            slot: 1_000,
            ..FakeRpc::default()
        };
        rpc.accounts.insert(
            CLOCK_SYSVAR,
            account(SYSVAR_PROGRAM, clock(1_000, 1_800_000_000)),
        );
        rpc.accounts.insert(
            quotebook_sdk::pda::maker_state(&PROGRAM, &MAKER).0,
            account(PROGRAM, maker_state(MAKER, window)),
        );
        let mut usdc = token_account(USDC, MAKER, 3_000_000);
        approve(&mut usdc, delegate(), 4_000_000);
        rpc.accounts
            .insert(USDC_ACCOUNT, account(TOKEN_PROGRAM, usdc));
        rpc
    }

    pub(crate) fn view(rpc: FakeRpc) -> RpcChainView<FakeRpc> {
        RpcChainView::new(
            rpc,
            PROGRAM,
            MAKER,
            vec![watched(USDC_ACCOUNT, USDC), watched(BASE_ACCOUNT, BASE)],
        )
        .unwrap()
    }

    #[test]
    fn a_snapshot_reads_one_slot_and_checks_every_account() {
        let mut window = NonceWindow::new();
        window.consume(Nonce::new(3)).unwrap();
        let v = view(chain(window));
        let snapshot = v.snapshot().unwrap();
        assert_eq!(snapshot.slot, 1_000);
        assert_eq!(snapshot.clock.get(), 1_800_000_000);
        assert_eq!(snapshot.window, window);
        assert_eq!(
            snapshot.accounts,
            vec![
                (
                    USDC_ACCOUNT,
                    TokenAccountState {
                        amount: 3_000_000,
                        delegated_amount: 4_000_000
                    }
                ),
                (BASE_ACCOUNT, TokenAccountState::default()),
            ]
        );
        assert_eq!(v.balances().amount(&USDC_ACCOUNT), Some(3_000_000));
        let inventory = BalanceInventory {
            balances: v.balances(),
            account: USDC_ACCOUNT,
            decimals: 6,
        };
        assert_eq!(inventory.inventory(), Decimal::new(3, 0));
    }

    #[test]
    fn a_bad_snapshot_is_an_error() {
        let window = NonceWindow::new();
        let ms = quotebook_sdk::pda::maker_state(&PROGRAM, &MAKER).0;

        let mut rpc = chain(window);
        rpc.fail = true;
        assert!(view(rpc).snapshot().is_err(), "rpc down");

        let mut rpc = chain(window);
        rpc.accounts.remove(&ms);
        assert!(view(rpc).snapshot().is_err(), "no MakerState");

        let mut rpc = chain(window);
        rpc.accounts
            .insert(ms, account(PROGRAM, maker_state(USDC, window)));
        assert!(view(rpc).snapshot().is_err(), "another maker");

        let mut rpc = chain(window);
        rpc.accounts
            .insert(ms, account(USDC, maker_state(MAKER, window)));
        assert!(view(rpc).snapshot().is_err(), "MakerState owner");

        let mut rpc = chain(window);
        rpc.accounts
            .insert(CLOCK_SYSVAR, account(SYSVAR_PROGRAM, clock(1_001, 1)));
        assert!(view(rpc).snapshot().is_err(), "clock ahead of the read");

        let mut rpc = chain(window);
        rpc.accounts
            .insert(CLOCK_SYSVAR, account(PROGRAM, clock(1_000, 1)));
        assert!(view(rpc).snapshot().is_err(), "clock owner");

        let mut rpc = chain(window);
        rpc.accounts.insert(
            USDC_ACCOUNT,
            account(TOKEN_PROGRAM, token_account(BASE, MAKER, 1)),
        );
        assert!(view(rpc).snapshot().is_err(), "wrong mint");
    }

    fn sig(n: usize, block_time: Option<i64>, failed: bool) -> SignatureInfo {
        SignatureInfo {
            signature: format!("sig{n}"),
            slot: 10_000 - n as u64,
            block_time,
            failed,
        }
    }

    /// A transaction whose top-level instructions call `programs` with `data`, and whose inner
    /// instructions do the same with `inner`.
    fn tx(block_time: Option<i64>, top: &[(u8, Vec<u8>)], inner: &[(u8, Vec<u8>)]) -> Value {
        let keys: Vec<String> = [MAKER, PROGRAM, USDC]
            .iter()
            .map(ToString::to_string)
            .collect();
        let ixs = |list: &[(u8, Vec<u8>)]| {
            list.iter()
                .map(|(i, d)| json!({"programIdIndex": i, "accounts": [], "data": bs58_encode(d)}))
                .collect::<Vec<_>>()
        };
        json!({
            "blockTime": block_time,
            "slot": 1,
            "meta": {
                "err": null,
                "innerInstructions": [{"index": 0, "instructions": ixs(inner)}],
                "loadedAddresses": {"writable": [], "readonly": []}
            },
            "transaction": {
                "signatures": ["x"],
                "message": {"accountKeys": keys, "instructions": ixs(top)}
            }
        })
    }

    #[test]
    fn settles_are_counted_top_level_and_inner() {
        let t = tx(
            Some(5),
            &[(1, vec![SETTLE, 1]), (1, vec![SETTLE]), (2, vec![SETTLE])],
            &[(1, vec![SETTLE, 9]), (1, vec![SETTLE.wrapping_add(1)])],
        );
        assert_eq!(
            settles_in(&t, &PROGRAM).unwrap(),
            Settles {
                block_time: Some(5),
                count: 3
            }
        );
        let mut failed = t.clone();
        failed["meta"]["err"] = json!({"InstructionError": [0, "Custom"]});
        assert_eq!(settles_in(&failed, &PROGRAM).unwrap().count, 0);

        let mut no_inner = t.clone();
        no_inner["meta"]["innerInstructions"] = Value::Null;
        assert!(settles_in(&no_inner, &PROGRAM).is_err());
        let mut no_meta = t.clone();
        no_meta["meta"] = Value::Null;
        assert!(settles_in(&no_meta, &PROGRAM).is_err());
        let mut bad_index = t.clone();
        bad_index["transaction"]["message"]["instructions"][0]["programIdIndex"] = json!(9);
        assert!(settles_in(&bad_index, &PROGRAM).is_err());
        let mut bad_data = t;
        bad_data["transaction"]["message"]["instructions"][0]["data"] = json!("0OIl");
        assert!(settles_in(&bad_data, &PROGRAM).is_err());
    }

    #[test]
    fn a_program_from_a_lookup_table_counts() {
        let mut t = tx(Some(5), &[(3, vec![SETTLE])], &[]);
        assert!(
            settles_in(&t, &PROGRAM).is_err(),
            "index past the static keys"
        );
        t["meta"]["loadedAddresses"]["readonly"] = json!([PROGRAM.to_string()]);
        assert_eq!(settles_in(&t, &PROGRAM).unwrap().count, 1);
    }

    fn history(entries: Vec<(SignatureInfo, Value)>) -> FakeRpc {
        let mut rpc = chain(NonceWindow::new());
        for (s, t) in entries {
            rpc.transactions.insert(s.signature.clone(), t);
            rpc.signatures.push(s);
        }
        rpc
    }

    #[test]
    fn the_history_walk_stops_at_the_hour_and_counts_every_settle() {
        let since = 1_000;
        let two = tx(Some(1_500), &[(1, vec![SETTLE]), (1, vec![SETTLE])], &[]);
        let rpc = history(vec![
            (sig(0, None, false), tx(None, &[(1, vec![SETTLE])], &[])),
            (sig(1, Some(1_500), false), two),
            (sig(2, Some(1_400), true), Value::Null),
            (
                sig(3, Some(1_300), false),
                tx(Some(1_300), &[(1, vec![0])], &[]),
            ),
            (
                sig(4, Some(1_000), false),
                tx(Some(1_000), &[(1, vec![SETTLE])], &[]),
            ),
        ]);
        let times = view(rpc)
            .fill_times_since(UnixTimestamp::new(since))
            .unwrap();
        assert_eq!(
            times,
            vec![
                None,
                Some(UnixTimestamp::new(1_500)),
                Some(UnixTimestamp::new(1_500))
            ],
            "no blockTime counts inside; the failed one is not read; the walk stops at `since`"
        );
    }

    #[test]
    fn the_history_walk_pages_and_fails_closed() {
        let entries: Vec<_> = (0..5)
            .map(|n| {
                let t = 2_000 - i64::try_from(n).unwrap();
                (
                    sig(n, Some(t), false),
                    tx(Some(t), &[(1, vec![SETTLE])], &[]),
                )
            })
            .collect();
        let rpc = history(entries.clone());
        let pages = Arc::clone(&rpc.pages);
        let times = view(rpc)
            .with_paging(2, 10)
            .fill_times_since(UnixTimestamp::new(0))
            .unwrap();
        assert_eq!(times.len(), 5);
        assert_eq!(
            pages.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            vec![None, Some("sig1".into()), Some("sig3".into())],
            "each page starts below the last one"
        );

        assert!(
            view(history(entries.clone()))
                .with_paging(2, 2)
                .fill_times_since(UnixTimestamp::new(0))
                .is_err(),
            "more history than the page limit"
        );

        let mut missing = history(entries.clone());
        missing.transactions.remove("sig2");
        assert!(
            view(missing)
                .fill_times_since(UnixTimestamp::new(0))
                .is_err(),
            "a listed transaction the node does not return"
        );

        let mut undecodable = history(entries);
        undecodable
            .transactions
            .insert("sig1".into(), json!({"blockTime": 1, "meta": null}));
        assert!(
            view(undecodable)
                .fill_times_since(UnixTimestamp::new(0))
                .is_err(),
            "a transaction that does not decode"
        );
    }
}
