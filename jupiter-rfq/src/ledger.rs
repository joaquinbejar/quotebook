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

/// A failed ledger read or write.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("debt ledger: {0}")]
pub struct LedgerError(pub String);

/// Where the fee debt is kept.
pub trait DebtLedger: Send + Sync {
    /// The debt last written.
    ///
    /// # Errors
    ///
    /// A missing, unreadable or malformed ledger.
    fn load(&self) -> Result<Debts, LedgerError>;

    /// Replaces the debt with `debts`, durably, before returning.
    ///
    /// # Errors
    ///
    /// A failed write; the previous contents then stand.
    fn store(&self, debts: &[(Address, u64)]) -> Result<(), LedgerError>;
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerFile {
    debts: Vec<LedgerEntry>,
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

/// The JSON text of `debts`.
///
/// # Errors
///
/// A mint listed twice.
pub fn to_json(debts: &[(Address, u64)]) -> Result<String, LedgerError> {
    let file = LedgerFile {
        debts: normalize(debts)?
            .into_iter()
            .map(|(mint, atoms)| LedgerEntry {
                mint: mint.to_string(),
                atoms,
            })
            .collect(),
    };
    serde_json::to_string_pretty(&file).map_err(|e| LedgerError(e.to_string()))
}

/// The debts in a ledger's JSON text.
///
/// # Errors
///
/// Malformed JSON, an unknown field, a bad mint or a mint listed twice.
pub fn from_json(text: &str) -> Result<Debts, LedgerError> {
    let file: LedgerFile = serde_json::from_str(text).map_err(|e| LedgerError(e.to_string()))?;
    let mut debts = Vec::with_capacity(file.debts.len());
    for e in file.debts {
        let mint = Address::from_str(&e.mint)
            .map_err(|_| LedgerError(format!("{:?}: not an address", e.mint)))?;
        debts.push((mint, e.atoms));
    }
    normalize(&debts)
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
    fn load(&self) -> Result<Debts, LedgerError> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| LedgerError(format!("{}: {e}", self.path.display())))?;
        from_json(&text)
    }

    fn store(&self, debts: &[(Address, u64)]) -> Result<(), LedgerError> {
        let text = to_json(debts)?;
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
    debts: Arc<Mutex<Debts>>,
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

    /// What was last stored.
    #[must_use]
    pub fn debts(&self) -> Debts {
        self.debts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
    fn load(&self) -> Result<Debts, LedgerError> {
        if flag(&self.fail_load) {
            return Err(LedgerError("load failed".into()));
        }
        Ok(self.debts())
    }

    fn store(&self, debts: &[(Address, u64)]) -> Result<(), LedgerError> {
        if flag(&self.fail_store) {
            return Err(LedgerError("store failed".into()));
        }
        let debts = normalize(debts)?;
        *self
            .debts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = debts;
        Ok(())
    }
}
