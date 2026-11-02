//! Replays the committed corpora through their oracles and reports how often each outcome
//! occurred. Fails if an accepting path or a rejection branch is never reached, so a corpus
//! that only exercises the first length check cannot pass (docs/PLAN-improvements.md, I6).
//! Run by `make fuzz-check`.

use std::collections::BTreeMap;
use std::path::Path;

use arbitrary::{Arbitrary, Unstructured};
use quotebook_core::QuotebookError as E;
use quotebook_fuzz::{
    check_accounts, check_quote_decode, check_sigverify, AccountInput, QuoteInput, SigInput,
};

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

fn replay<I: for<'a> Arbitrary<'a>>(
    target: &str,
    mut outcome: impl FnMut(&I) -> String,
) -> BTreeMap<String, usize> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join(target);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).expect("corpus exists") {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        let Ok(input) = I::arbitrary_take_rest(Unstructured::new(&bytes)) else {
            continue;
        };
        *counts.entry(outcome(&input)).or_default() += 1;
    }
    println!("{target} corpus outcomes: {counts:#?}");
    counts
}

#[test]
fn quote_decode_corpus_reaches_accept_and_every_rejection() {
    let counts = replay::<QuoteInput>("quote_decode", |i| check_quote_decode(&i.bytes()));
    let required = [
        "Ok".to_string(),
        format!("{:?}", E::QuoteLengthMismatch),
        format!("{:?}", E::WrongDomain),
        format!("{:?}", E::MalformedQuote),
    ];
    let missing: Vec<_> = required
        .iter()
        .filter(|k| !counts.contains_key(*k))
        .collect();
    assert!(missing.is_empty(), "corpus never reaches {missing:?}");
}

#[test]
fn accounts_decode_corpus_accepts_every_type() {
    let counts = replay::<AccountInput>("accounts_decode", |i| {
        check_accounts(&i.bytes()).to_string()
    });
    for k in ["Config", "Series", "MakerState", "None"] {
        assert!(counts.contains_key(k), "corpus never reaches {k}");
    }
}

#[test]
fn instruction_args_corpus_accepts_every_decoder() {
    use quotebook_core::instruction::{
        decode_deployment_id, decode_new_admin, decode_paused, decode_u64, CreateSeriesArgs,
        SettleArgs, Tag,
    };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/instruction_args");
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).expect("corpus exists") {
        let d = std::fs::read(entry.unwrap().path()).unwrap();
        let hits = [
            ("Tag", Tag::split(&d).is_ok()),
            ("CreateSeriesArgs", CreateSeriesArgs::decode(&d).is_ok()),
            ("SettleArgs", SettleArgs::decode(&d).is_ok()),
            ("u64", decode_u64(&d).is_ok()),
            ("deployment_id", decode_deployment_id(&d).is_ok()),
            ("new_admin", decode_new_admin(&d).is_ok()),
            ("paused", decode_paused(&d).is_ok()),
        ];
        for (k, hit) in hits {
            if hit {
                *counts.entry(k).or_default() += 1;
            }
        }
    }
    println!("instruction_args corpus accepting paths: {counts:#?}");
    for k in [
        "Tag",
        "CreateSeriesArgs",
        "SettleArgs",
        "u64",
        "deployment_id",
        "new_admin",
        "paused",
    ] {
        assert!(counts.contains_key(k), "corpus never accepts {k}");
    }
}
