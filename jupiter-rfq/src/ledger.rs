//! The fee debt ledger.
//!
//! Jupiter's fee on a landed fill is owed by the maker until it is collected, so the desk holds
//! it against the output mint's balance like a reservation. The chain cannot rebuild it after a
//! restart (a [`crate::desk::Spent`] carries no amounts, and collection happens off the desk), so
//! the debt is kept in a durable ledger:
//!
//! - it is loaded when the desk starts; a ledger that cannot be read, or a missing file, refuses
//!   every quote and swap (it is never taken as empty);
//! - every accrual is written before the desk quotes again; a failed write keeps the debt in
//!   memory and refuses until a write succeeds;
//! - every fill reserved for co-signing is written here **before** the desk answers that it would
//!   sign, in the same file as the debt, and reloaded at start. A fill that lands across a restart
//!   therefore still resolves through the normal landed or expired paths and accrues its fee once;
//!   a failed write refuses the swap;
//! - debt is released only by [`crate::desk::Desk::record_collection`], with the operator's
//!   evidence of collection, and only once that release is written.
//!
//! [`FileLedger`] is the durable one: a small JSON file, written to a temporary file, synced and
//! renamed. [`MemoryLedger`] is for tests and is not durable.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use solana_address::Address;

/// Debt per output mint, atoms.
pub type Debts = Vec<(Address, u64)>;

/// A fill reserved for co-signing, not yet landed or expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFill {
    /// The transaction, by the taker's signature.
    pub id: [u8; 64],
    /// The mint paid out.
    pub output_mint: Address,
    /// Atoms paid out.
    pub amount_out: u64,
    /// The fee it accrues if it lands, atoms.
    pub fee_liability: u64,
    /// What it costs the maker, lamports.
    pub fee_lamports: u64,
    /// The fill's `expire_at`.
    pub expire_at: i64,
}

/// What the ledger holds, written as one unit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LedgerState {
    /// Uncollected fee debt per mint.
    pub debts: Debts,
    /// Fills reserved and unresolved.
    pub pending: Vec<PendingFill>,
}

/// A failed ledger read or write.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("debt ledger: {0}")]
pub struct LedgerError(pub String);

/// Where the fee debt is kept.
pub trait DebtLedger: Send + Sync {
    /// What was last written.
    ///
    /// # Errors
    ///
    /// A missing, unreadable or malformed ledger.
    fn load(&self) -> Result<LedgerState, LedgerError>;

    /// Replaces the contents with `state`, durably, before returning.
    ///
    /// # Errors
    ///
    /// A failed write; the previous contents then stand.
    fn store(&self, state: &LedgerState) -> Result<(), LedgerError>;
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerFile {
    debts: Vec<LedgerEntry>,
    #[serde(default)]
    pending: Vec<PendingEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingEntry {
    /// The signature in lower-case hex.
    id: String,
    mint: String,
    amount_out: u64,
    fee_liability: u64,
    fee_lamports: u64,
    expire_at: i64,
}

fn hex(id: &[u8; 64]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Result<[u8; 64], LedgerError> {
    let bad = || LedgerError(format!("{s:?}: not a 64-byte hex signature"));
    if s.len() != 128 || !s.is_ascii() {
        return Err(bad());
    }
    let mut out = [0u8; 64];
    for (slot, pair) in out.iter_mut().zip(s.as_bytes().as_chunks::<2>().0) {
        let text = std::str::from_utf8(pair).map_err(|_| bad())?;
        *slot = u8::from_str_radix(text, 16).map_err(|_| bad())?;
    }
    Ok(out)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerEntry {
    mint: String,
    atoms: u64,
}

/// Debts with one entry per mint and no zero entry, sorted by mint.
///
/// # Errors
///
/// A mint listed twice.
pub fn normalize(debts: &[(Address, u64)]) -> Result<Debts, LedgerError> {
    let mut seen: HashMap<Address, u64> = HashMap::new();
    for (mint, atoms) in debts {
        if seen.insert(*mint, *atoms).is_some() {
            return Err(LedgerError(format!("{mint} listed twice")));
        }
    }
    let mut out: Debts = seen.into_iter().filter(|(_, a)| *a > 0).collect();
    out.sort_by_key(|(m, _)| m.to_string());
    Ok(out)
}

/// The JSON text of `state`.
///
/// # Errors
///
/// A mint listed twice.
pub fn to_json(state: &LedgerState) -> Result<String, LedgerError> {
    let file = LedgerFile {
        debts: normalize(&state.debts)?
            .into_iter()
            .map(|(mint, atoms)| LedgerEntry {
                mint: mint.to_string(),
                atoms,
            })
            .collect(),
        pending: state
            .pending
            .iter()
            .map(|p| PendingEntry {
                id: hex(&p.id),
                mint: p.output_mint.to_string(),
                amount_out: p.amount_out,
                fee_liability: p.fee_liability,
                fee_lamports: p.fee_lamports,
                expire_at: p.expire_at,
            })
            .collect(),
    };
    serde_json::to_string_pretty(&file).map_err(|e| LedgerError(e.to_string()))
}

fn mint_of(text: &str) -> Result<Address, LedgerError> {
    Address::from_str(text).map_err(|_| LedgerError(format!("{text:?}: not an address")))
}

/// The state in a ledger's JSON text.
///
/// # Errors
///
/// Malformed JSON, an unknown field, a bad mint or signature, a mint or a fill listed twice.
pub fn from_json(text: &str) -> Result<LedgerState, LedgerError> {
    let file: LedgerFile = serde_json::from_str(text).map_err(|e| LedgerError(e.to_string()))?;
    let mut debts = Vec::with_capacity(file.debts.len());
    for e in file.debts {
        debts.push((mint_of(&e.mint)?, e.atoms));
    }
    let mut pending: Vec<PendingFill> = Vec::with_capacity(file.pending.len());
    for e in file.pending {
        let id = from_hex(&e.id)?;
        if pending.iter().any(|p| p.id == id) {
            return Err(LedgerError(format!("{} listed twice", e.id)));
        }
        pending.push(PendingFill {
            id,
            output_mint: mint_of(&e.mint)?,
            amount_out: e.amount_out,
            fee_liability: e.fee_liability,
            fee_lamports: e.fee_lamports,
            expire_at: e.expire_at,
        });
    }
    Ok(LedgerState {
        debts: normalize(&debts)?,
        pending,
    })
}

/// A ledger in a JSON file. The file must exist: create it with `{"debts": []}` before the
/// first start, so a lost file is never read as no debt.
#[derive(Debug, Clone)]
pub struct FileLedger {
    path: PathBuf,
}

impl FileLedger {
    /// The ledger at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn temporary(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".tmp");
        PathBuf::from(name)
    }
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => std::fs::File::open(dir)?.sync_all(),
        None => std::fs::File::open(".")?.sync_all(),
    }
}

impl DebtLedger for FileLedger {
    fn load(&self) -> Result<LedgerState, LedgerError> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| LedgerError(format!("{}: {e}", self.path.display())))?;
        from_json(&text)
    }

    fn store(&self, state: &LedgerState) -> Result<(), LedgerError> {
        let text = to_json(state)?;
        let tmp = self.temporary();
        let io = |e: std::io::Error| LedgerError(format!("{}: {e}", self.path.display()));
        let mut file = std::fs::File::create(&tmp).map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.write_all(b"\n").map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        std::fs::rename(&tmp, &self.path).map_err(io)?;
        sync_dir(&self.path).map_err(io)
    }
}

/// A ledger in memory, shared between its clones. **Not durable:** for tests, where two desks
/// over one `MemoryLedger` stand for a restart.
#[derive(Debug, Clone, Default)]
pub struct MemoryLedger {
    state: Arc<Mutex<LedgerState>>,
    fail_load: Arc<Mutex<bool>>,
    fail_store: Arc<Mutex<bool>>,
}

fn flag(m: &Mutex<bool>) -> bool {
    *m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl MemoryLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn current(&self) -> LedgerState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The debt last stored.
    #[must_use]
    pub fn debts(&self) -> Debts {
        self.current().debts
    }

    /// The fills last stored.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingFill> {
        self.current().pending
    }

    /// Makes every load fail, or not.
    pub fn fail_load(&self, fail: bool) {
        *self
            .fail_load
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fail;
    }

    /// Makes every store fail, or not.
    pub fn fail_store(&self, fail: bool) {
        *self
            .fail_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fail;
    }
}

impl DebtLedger for MemoryLedger {
    fn load(&self) -> Result<LedgerState, LedgerError> {
        if flag(&self.fail_load) {
            return Err(LedgerError("load failed".into()));
        }
        Ok(self.current())
    }

    fn store(&self, state: &LedgerState) -> Result<(), LedgerError> {
        if flag(&self.fail_store) {
            return Err(LedgerError("store failed".into()));
        }
        let debts = normalize(&state.debts)?;
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = LedgerState {
            debts,
            pending: state.pending.clone(),
        };
        Ok(())
    }
}
