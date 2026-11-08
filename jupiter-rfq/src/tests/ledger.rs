//! The file ledger: a round trip, and every way a ledger is refused rather than read as empty.

use super::*;
use crate::ledger::{from_json, DebtLedger, FileLedger};

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "jupiter-rfq-ledger-{}-{name}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn a_file_ledger_round_trips_and_a_missing_file_is_refused() {
    let path = scratch("round-trip");
    let ledger = FileLedger::new(&path);
    assert!(ledger.load().is_err(), "a missing file is never no debt");
    std::fs::write(&path, r#"{"debts": []}"#).unwrap();
    assert_eq!(ledger.load().unwrap(), vec![]);
    ledger
        .store(&[(QUOTE, 7), (BASE, 200_000), (MAKER, 0)])
        .unwrap();
    // Sorted by mint, zero entries dropped, no temporary file left.
    let mut want = vec![(BASE, 200_000), (QUOTE, 7)];
    want.sort_by_key(|(m, _)| m.to_string());
    assert_eq!(ledger.load().unwrap(), want);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    assert!(!PathBuf::from(tmp).exists());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_malformed_ledger_is_refused() {
    let base = BASE.to_string();
    for bad in [
        String::new(),
        "{}".to_owned(),
        r#"{"debts": [{"mint": "nope", "atoms": 1}]}"#.to_owned(),
        format!(r#"{{"debts": [{{"mint": "{base}", "atoms": -1}}]}}"#),
        format!(r#"{{"debts": [{{"mint": "{base}", "atoms": 1, "extra": 0}}]}}"#),
        format!(
            r#"{{"debts": [{{"mint": "{base}", "atoms": 1}}, {{"mint": "{base}", "atoms": 2}}]}}"#
        ),
    ] {
        assert!(from_json(&bad).is_err(), "{bad:?}");
    }
    let store = FileLedger::new(scratch("duplicate"));
    assert!(store.store(&[(BASE, 1), (BASE, 2)]).is_err());
}
