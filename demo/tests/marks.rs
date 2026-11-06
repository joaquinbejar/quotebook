//! Strip marks against fixtures from `metalbook_maker::mark` (`fixtures/README.md`).

#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    missing_docs
)]

use std::collections::BTreeMap;
use std::str::FromStr;

use metalbook_core::{
    OptionKind, OuncesPerContract, SeriesKey, SeriesTerms, StrikeUsd, Underlying, UnixTimestamp,
};
use quotebook_demo::marks::{price_expiry, MarkError, MarkInputs};
use rust_decimal::Decimal;
use serde_json::Value;

const FIXTURES: &str = include_str!("fixtures/marks.json");

fn dec(v: &Value) -> Decimal {
    Decimal::from_str(v.as_str().unwrap()).unwrap()
}

fn terms(c: &Value) -> SeriesTerms {
    let underlying = match c["underlying"].as_str().unwrap() {
        "XAU" => Underlying::Xau,
        "XAG" => Underlying::Xag,
        other => panic!("{other}"),
    };
    let kind = match c["kind"].as_str().unwrap() {
        "call" => OptionKind::Call,
        "put" => OptionKind::Put,
        other => panic!("{other}"),
    };
    let key = SeriesKey::try_new(
        underlying,
        kind,
        StrikeUsd::new(c["strike"].as_u64().unwrap()),
        UnixTimestamp::new(c["expiry"].as_i64().unwrap()),
    )
    .unwrap();
    SeriesTerms::try_new(
        key,
        OuncesPerContract::try_new(u32::try_from(c["multiplier"].as_u64().unwrap()).unwrap())
            .unwrap(),
        StrikeUsd::new(c["cap"].as_u64().unwrap()),
    )
    .unwrap()
}

fn cases() -> Vec<Value> {
    let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
    fixtures["cases"].as_array().unwrap().clone()
}

fn inputs(c: &Value) -> MarkInputs {
    MarkInputs {
        spot: dec(&c["spot"]),
        vol: dec(&c["vol"]),
        rate: dec(&c["rate"]),
        carry: dec(&c["carry"]),
        now: c["now"].as_i64().unwrap(),
    }
}

#[test]
fn every_series_priced_alone_equals_its_fixture() {
    let cases = cases();
    assert_eq!(cases.len(), 162);
    for c in &cases {
        let marks = price_expiry(&[terms(c)], &inputs(c)).unwrap();
        assert_eq!(marks[0].value, dec(&c["value"]), "{c}");
        assert_eq!(marks[0].max_payout, dec(&c["max_payout"]), "{c}");
    }
}

#[test]
fn a_whole_strip_equals_its_fixtures_exactly() {
    // Every series sharing an underlying, spot, vol and time goes in one strip, as the board does.
    let cases = cases();
    let mut strips: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for c in &cases {
        let key = format!(
            "{} {} {} {}",
            c["underlying"], c["spot"], c["vol"], c["now"]
        );
        strips.entry(key).or_default().push(c);
    }
    assert_eq!(strips.len(), 36);
    let mut largest = 0;
    for strip in strips.values() {
        largest = largest.max(strip.len());
        let series: Vec<SeriesTerms> = strip.iter().map(|c| terms(c)).collect();
        let marks = price_expiry(&series, &inputs(strip[0])).unwrap();
        assert_eq!(marks.len(), strip.len());
        for (mark, c) in marks.iter().zip(strip) {
            assert_eq!(mark.value, dec(&c["value"]), "{c}");
            assert_eq!(mark.max_payout, dec(&c["max_payout"]), "{c}");
        }
    }
    assert_eq!(largest, 7, "the XAG strip mixes capped calls and a put");
}

#[test]
fn the_grid_covers_both_clamps_and_the_interior() {
    let cases = cases();
    let value = |c: &Value| dec(&c["value"]);
    assert!(cases.iter().any(|c| value(c) == Decimal::ZERO));
    assert!(cases
        .iter()
        .any(|c| value(c) > Decimal::ZERO && value(c) < dec(&c["max_payout"])));
    // A capped call deep in the money stays below its maximum, by discounting.
    assert!(cases.iter().all(|c| value(c) <= dec(&c["max_payout"])));
}

#[test]
fn an_empty_strip_is_empty() {
    let c = &cases()[0];
    assert_eq!(price_expiry(&[], &inputs(c)).unwrap(), Vec::new());
}

#[test]
fn a_strip_cannot_mix_expiries() {
    let cases = cases();
    let c = &cases[0];
    let mut other = c.clone();
    other["expiry"] = Value::from(c["expiry"].as_i64().unwrap() + 604_800);
    assert_eq!(
        price_expiry(&[terms(c), terms(&other)], &inputs(c)),
        Err(MarkError::MixedExpiry)
    );
}

#[test]
fn no_time_left_is_refused() {
    let c = &cases()[0];
    let mut at_expiry = inputs(c);
    at_expiry.now = c["expiry"].as_i64().unwrap();
    assert_eq!(
        price_expiry(&[terms(c)], &at_expiry),
        Err(MarkError::Expired)
    );
}
