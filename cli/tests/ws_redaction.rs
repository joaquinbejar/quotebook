//! A maker service is untrusted: whatever text it sends back must not carry the secret in its
//! URL out through the taker's errors (Display or Debug).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    missing_docs
)]

use futures_util::{SinkExt, StreamExt};
use quotebook_cli::taker;
use quotebook_core::BaseLots;
use quotebook_maker::protocol::TakerSide;
use solana_address::Address;
use tokio_tungstenite::tungstenite::Message;

const KEY: &str = "SYNTHETIC-WS-KEY";

/// A WebSocket server that answers the first frame with `reply` and returns its URL, with the
/// key as the path.
async fn server(reply: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/{KEY}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = ws.next().await;
        ws.send(Message::text(reply)).await.unwrap();
        let _ = ws.close(None).await;
    });
    url
}

async fn ask(reply: String) -> String {
    let url = server(reply).await;
    let request = taker::request(
        1,
        &Address::new_from_array([1; 32]),
        &Address::new_from_array([2; 32]),
        TakerSide::Buy,
        BaseLots::new(1),
    );
    let err = taker::ask(&url, &request).await.unwrap_err();
    format!("{err} {err:?}")
}

/// Codex's repro: the maker rejects with the key as its message.
#[tokio::test]
async fn test_reject_message_is_redacted() {
    let reply = format!(
        r#"{{"type":"reject","request_id":1,"reason":"internal","message":"{KEY} at ws://127.0.0.1:1/{KEY}"}}"#
    );
    let shown = ask(reply).await;
    assert!(!shown.contains(KEY), "{shown}");
    assert!(shown.contains("Rejected"), "{shown}");
}

/// Malformed answers carrying the key: not JSON, and a quote with garbage fields.
#[tokio::test]
async fn test_protocol_errors_are_redacted() {
    for reply in [
        format!("not json {KEY}"),
        format!(
            r#"{{"type":"quote","request_id":1,"quote":"{KEY}","signature":"00","maker_base":"x","maker_quote":"y"}}"#
        ),
        format!(
            r#"{{"type":"quote","request_id":9,"quote":"{KEY}","signature":"00","maker_base":"x","maker_quote":"y"}}"#
        ),
    ] {
        let shown = ask(reply).await;
        assert!(!shown.contains(KEY), "{shown}");
    }
}
