//! Replays the committed `sigverify` corpus through the oracle and reports how often each
//! outcome occurred. Fails if any rejection branch of `verified_quote` is never reached, so a
//! corpus that only exercises the first length check cannot pass (docs/PLAN-improvements.md,
//! I6). Run by `make fuzz-check`.

use std::collections::BTreeMap;
use std::path::Path;

use arbitrary::{Arbitrary, Unstructured};
use quotebook_core::QuotebookError as E;
use quotebook_fuzz::{check_sigverify, SigInput};

#[test]
fn sigverify_corpus_reaches_every_branch() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/sigverify");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let entries = std::fs::read_dir(&dir).expect("corpus/sigverify exists");
    for entry in entries {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        let u = Unstructured::new(&bytes);
        let Ok(input) = SigInput::arbitrary_take_rest(u) else {
            continue;
        };
        let key = match check_sigverify(&input.bytes()) {
            Ok(()) => "Ok".to_string(),
            Err(e) => format!("{e:?}"),
        };
        *counts.entry(key).or_default() += 1;
    }
    println!("sigverify corpus outcomes: {counts:#?}");
    let required = [
        "Ok".to_string(),
        format!("{:?}", E::MalformedSigVerify),
        format!("{:?}", E::SigVerifyNotSelfContained),
        format!("{:?}", E::QuoteLengthMismatch),
        format!("{:?}", E::WrongDomain),
        format!("{:?}", E::MalformedQuote),
        format!("{:?}", E::SignerMismatch),
    ];
    let missing: Vec<_> = required
        .iter()
        .filter(|k| !counts.contains_key(*k))
        .collect();
    assert!(missing.is_empty(), "corpus never reaches {missing:?}");
}
