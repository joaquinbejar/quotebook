//! The wire types against Jupiter's published schema and the acceptance suite's payloads
//! (`tests/fixtures/`, toolkit `3aa4fce`, MIT).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    missing_docs
)]

use jupiter_rfq::api::{
    Protocol, QuoteRequest, QuoteResponse, QuoteType, RejectionReason, SwapRequest, SwapResponse,
    SwapState,
};
use jupiter_rfq::tx::Transaction;
use serde_json::{json, Map, Value};

const OPENAPI: &str = include_str!("fixtures/openapi.json");
const ACCEPTANCE: &str = include_str!("fixtures/acceptance.json");

fn schemas() -> Map<String, Value> {
    let v: Value = serde_json::from_str(OPENAPI).unwrap();
    v["components"]["schemas"].as_object().unwrap().clone()
}

/// An object made of each property's first example, typed as the schema says.
fn example(schema: &Value, all: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for (name, prop) in schema["properties"].as_object().unwrap() {
        let value = if let Some(r) = prop.get("$ref").and_then(Value::as_str) {
            let target = &all[r.trim_start_matches("#/components/schemas/")];
            target["enum"][0].clone()
        } else if let Some(ex) = prop.get("examples").and_then(|e| e.get(0)) {
            let ty = prop["type"].clone();
            let is_int =
                ty == "integer" || ty.as_array().is_some_and(|a| a.contains(&json!("integer")));
            match (is_int, ex) {
                (true, Value::String(s)) => json!(s.parse::<u64>().unwrap()),
                _ => ex.clone(),
            }
        } else {
            continue;
        };
        out.insert(name.clone(), value);
    }
    Value::Object(out)
}

fn required(schema: &Value) -> Vec<String> {
    schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

/// Every required key is present and every key is one the schema knows.
fn conforms(value: &Value, schema: &Value) {
    let obj = value.as_object().unwrap();
    for key in required(schema) {
        assert!(obj.contains_key(&key), "missing required {key} in {value}");
    }
    let props = schema["properties"].as_object().unwrap();
    for key in obj.keys() {
        assert!(props.contains_key(key), "{key} is not in the schema");
    }
}

#[test]
fn request_examples_parse_and_responses_conform() {
    let all = schemas();
    let q: QuoteRequest = serde_json::from_value(example(&all["QuoteRequest"], &all)).unwrap();
    assert_eq!(q.quote_type, QuoteType::ExactIn);
    assert_eq!(q.protocol, Protocol::V1);
    // Round trip keeps the schema's keys only.
    conforms(&serde_json::to_value(&q).unwrap(), &all["QuoteRequest"]);

    let s: SwapRequest = serde_json::from_value(example(&all["SwapRequest"], &all)).unwrap();
    conforms(&serde_json::to_value(&s).unwrap(), &all["SwapRequest"]);
    // The schema's own example transaction is not valid base64: 841 characters, so its
    // padding is wrong. The strict decoder refuses it; the acceptance suite's transaction
    // (below) is the one that decodes.
    assert_eq!(s.transaction.len(), 841);
    assert_eq!(
        Transaction::from_base64(&s.transaction),
        Err(jupiter_rfq::tx::ParseError::Base64)
    );

    let qr: QuoteResponse = serde_json::from_value(example(&all["QuoteResponse"], &all)).unwrap();
    conforms(&serde_json::to_value(&qr).unwrap(), &all["QuoteResponse"]);
    let sr: SwapResponse = serde_json::from_value(example(&all["SwapResponse"], &all)).unwrap();
    conforms(&serde_json::to_value(&sr).unwrap(), &all["SwapResponse"]);
}

#[test]
fn enums_match_the_schema_and_enums_rs() {
    let all = schemas();
    let names = |k: &str| all[k]["enum"].clone();
    assert_eq!(
        names("QuoteType"),
        json!([QuoteType::ExactIn, QuoteType::ExactOut])
    );
    assert_eq!(names("Protocol"), json!([Protocol::V1]));
    assert_eq!(
        names("SwapState"),
        json!([SwapState::Accepted, SwapState::Rejected])
    );
    // The aliases the toolkit accepts.
    for s in ["\"exactIn\"", "\"exact_in\"", "\"ExactIn\""] {
        assert_eq!(
            serde_json::from_str::<QuoteType>(s).unwrap(),
            QuoteType::ExactIn
        );
    }
    // The state the schema lacks and the acceptance suite expects.
    let fixtures: Value = serde_json::from_str(ACCEPTANCE).unwrap();
    assert_eq!(
        serde_json::to_value(SwapState::RejectedWithReason(
            RejectionReason::InsufficientBalance
        ))
        .unwrap(),
        fixtures["swap_rejected_with_reason"]["state"]
    );
    for (reason, wire) in [
        (
            RejectionReason::InsufficientBalanceForAtaCreation,
            "insufficientBalanceForAtaCreation",
        ),
        (
            RejectionReason::SignatureVerificationFailed,
            "signatureVerificationFailed",
        ),
        (RejectionReason::BotActivityDetected, "botActivityDetected"),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(wire));
    }
}

#[test]
fn the_acceptance_payloads_parse() {
    let fixtures: Value = serde_json::from_str(ACCEPTANCE).unwrap();
    let q: QuoteRequest = serde_json::from_value(fixtures["quote_exact_in"].clone()).unwrap();
    assert_eq!(q.fee_bps, 2);
    assert_eq!(q.suggested_prioritization_fees, Some(10_000));
    let _: QuoteRequest = serde_json::from_value(fixtures["quote_unsupported"].clone()).unwrap();
    let s: SwapRequest = serde_json::from_value(fixtures["swap"].clone()).unwrap();
    let tx = Transaction::from_base64(&s.transaction).unwrap();
    // [compute limit, compute price, create ATA, fill]; both signature slots empty.
    assert_eq!(tx.message.instructions.len(), 4);
    assert!(tx.signatures.iter().all(|sig| sig.iter().all(|b| *b == 0)));
    let fill = &tx.message.instructions[3];
    assert_eq!(
        tx.message.key(fill.program_index).unwrap(),
        &jupiter_rfq::programs::ORDER_ENGINE
    );
    // 32 bytes of arguments and the older 3-byte payload: the prototype requires 5.
    assert_eq!(fill.data.len(), 35);
    assert_eq!(jupiter_rfq::programs::Fill::decode(&fill.data), None);
}

#[test]
fn the_shipped_acceptance_config_loads() {
    let c = jupiter_rfq::config::ConfigFile::from_json(include_str!("../config/acceptance.json"))
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(c.quote_deadline, std::time::Duration::from_millis(200));
    assert_eq!(c.desk.markets.len(), 1);
}
