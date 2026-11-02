//! Fill events from a transaction's log lines (docs/PLAN-improvements.md, I7).
//!
//! `settle` logs a [`FillEvent`] with `sol_log_data`, which the runtime writes as
//! `Program data: <base64>`. That line alone proves nothing:
//! - any program can log the same bytes;
//! - a successful `settle` followed by a failing later instruction leaves the line in the logs,
//!   although the transaction rolled back.
//!
//! [`fill_events`] yields an event only when all of these hold:
//! - the transaction succeeded, at the commitment the caller requires (`finalized` by default
//!   for indexers);
//! - the line sits in a frame of the configured quotebook program. The invocation stack is
//!   replayed from `Program <id> invoke [n]` and `Program <id> success|failed` lines, at any
//!   depth, so `settle` called by CPI works. A foreign program's line is never accepted, even
//!   when nested inside quotebook's frame;
//! - that frame **and every ancestor frame** closed with `success`. Events are buffered per
//!   frame, and a failed frame drops its own and its descendants' events.
//!
//! An unbalanced stack, a frame left open, or the runtime's `Log truncated` marker gives
//! [`Extraction::Incomplete`] with the events proven so far. It fails closed and never
//! guesses. Events are hints: `MakerState.fills` and balances prove that fills happened, but
//! a lost event (truncated or dropped logs) can only be recovered from the transaction itself.
//! The identity of an event is (transaction signature, [`IndexedEvent::index`]).

use quotebook_core::event::{EventError, FillEvent};
use solana_address::Address;

/// How final a transaction is, as RPC reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Commitment {
    /// Processed by a node.
    Processed,
    /// Voted on by a supermajority.
    Confirmed,
    /// Rooted.
    Finalized,
}

/// What [`fill_events`] reads: the transaction's outcome, its commitment, and its log lines
/// (RPC `meta.logMessages`).
#[derive(Debug, Clone, Copy)]
pub struct TransactionLogs<'a> {
    /// `meta.err` is null.
    pub succeeded: bool,
    /// The commitment the transaction was read at.
    pub commitment: Commitment,
    /// The log lines, in order.
    pub logs: &'a [String],
}

/// An event with its position among the events [`fill_events`] yielded for the transaction:
/// with the signature, its deduplication identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexedEvent {
    /// 0 for the first event of the transaction, then 1, …
    pub index: usize,
    /// The event.
    pub event: FillEvent,
}

/// The events a transaction proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extraction {
    /// Every frame balanced and closed: these are all the transaction's fill events.
    Complete(Vec<IndexedEvent>),
    /// The logs were truncated or unbalanced: these events are proven, others may be missing.
    Incomplete(Vec<IndexedEvent>),
}

/// Why a transaction yields no events at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EventsError {
    /// The transaction failed, so every instruction rolled back.
    #[error("the transaction failed; it rolled back every fill")]
    TransactionFailed,
    /// The transaction is less final than required.
    #[error("commitment {got:?} is below the required {required:?}")]
    BelowCommitment {
        /// The transaction's commitment.
        got: Commitment,
        /// The commitment asked for.
        required: Commitment,
    },
}

/// The context-free decoder: a `sol_log_data` payload as a fill event. It says nothing about
/// who logged it; use [`fill_events`] for that.
///
/// # Errors
///
/// The [`FillEvent::decode`] errors.
pub fn fill_event_payload(bytes: &[u8]) -> Result<FillEvent, EventError> {
    FillEvent::decode(bytes)
}

/// One frame of the replayed invocation stack.
struct Frame {
    program: Address,
    events: Vec<FillEvent>,
}

/// The fill events `tx` proves for `program_id` (module docs).
///
/// # Errors
///
/// `TransactionFailed`, or `BelowCommitment` if `tx.commitment < required`.
pub fn fill_events(
    program_id: &Address,
    tx: &TransactionLogs<'_>,
    required: Commitment,
) -> Result<Extraction, EventsError> {
    if !tx.succeeded {
        return Err(EventsError::TransactionFailed);
    }
    if tx.commitment < required {
        return Err(EventsError::BelowCommitment {
            got: tx.commitment,
            required,
        });
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut proven: Vec<FillEvent> = Vec::new();
    let mut complete = true;
    for line in tx.logs {
        match parse(line) {
            Line::Invoke { program, depth } => {
                if depth != stack.len().saturating_add(1) {
                    complete = false;
                    break;
                }
                stack.push(Frame {
                    program,
                    events: Vec::new(),
                });
            }
            Line::Success { program } => match stack.pop() {
                Some(frame) if frame.program == program => match stack.last_mut() {
                    // A nested frame's events wait for every ancestor's success.
                    Some(parent) => parent.events.extend(frame.events),
                    None => proven.extend(frame.events),
                },
                _ => {
                    complete = false;
                    break;
                }
            },
            Line::Failed { program } => match stack.pop() {
                // The frame and all its descendants' events are dropped.
                Some(frame) if frame.program == program => {}
                _ => {
                    complete = false;
                    break;
                }
            },
            Line::Data(payload) => {
                if let Some(top) = stack.last_mut() {
                    if top.program == *program_id {
                        if let Some(event) = payload.and_then(|b| FillEvent::decode(&b).ok()) {
                            top.events.push(event);
                        }
                    }
                }
            }
            Line::Truncated => {
                complete = false;
                break;
            }
            Line::Other => {}
        }
    }
    if !stack.is_empty() {
        complete = false;
    }
    let events = proven
        .into_iter()
        .enumerate()
        .map(|(index, event)| IndexedEvent { index, event })
        .collect();
    Ok(if complete {
        Extraction::Complete(events)
    } else {
        Extraction::Incomplete(events)
    })
}

/// A log line, as far as the stack replay cares.
enum Line {
    Invoke {
        program: Address,
        depth: usize,
    },
    Success {
        program: Address,
    },
    Failed {
        program: Address,
    },
    /// `Program data:` with one base64 field decoded, or `None` if it is not one valid field.
    Data(Option<Vec<u8>>),
    Truncated,
    Other,
}

fn parse(line: &str) -> Line {
    if line == "Log truncated" {
        return Line::Truncated;
    }
    if let Some(rest) = line.strip_prefix("Program data: ") {
        let mut fields = rest.split(' ');
        return match (fields.next(), fields.next()) {
            (Some(b64), None) => Line::Data(base64_decode(b64)),
            _ => Line::Data(None),
        };
    }
    let Some(rest) = line.strip_prefix("Program ") else {
        return Line::Other;
    };
    let mut words = rest.splitn(2, ' ');
    let (Some(id), Some(tail)) = (words.next(), words.next()) else {
        return Line::Other;
    };
    let Ok(program) = id.parse::<Address>() else {
        return Line::Other;
    };
    if tail == "success" {
        return Line::Success { program };
    }
    if tail.starts_with("failed") {
        return Line::Failed { program };
    }
    if let Some(depth) = tail
        .strip_prefix("invoke [")
        .and_then(|d| d.strip_suffix(']'))
        .and_then(|d| d.parse::<usize>().ok())
    {
        return Line::Invoke { program, depth };
    }
    Line::Other
}

/// Standard base64 with padding, as the runtime writes `Program data`. `None` on any invalid
/// character, length or padding.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c.checked_sub(b'A')?)),
            b'a'..=b'z' => Some(u32::from(c.checked_sub(b'a')?).checked_add(26)?),
            b'0'..=b'9' => Some(u32::from(c.checked_sub(b'0')?).checked_add(52)?),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    if bytes.len().checked_rem(4)? != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len().checked_div(4)?.checked_mul(3)?);
    let (chunks, _) = bytes.as_chunks::<4>();
    let last = chunks.len().checked_sub(1);
    for (i, chunk) in chunks.iter().enumerate() {
        let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && Some(i) != last) {
            return None;
        }
        let mut acc = 0u32;
        for (j, c) in chunk.iter().enumerate() {
            let v = if j >= 4usize.checked_sub(pad)? {
                0
            } else {
                value(*c)?
            };
            acc = acc.checked_shl(6)? | v;
        }
        let [_, a, b, c] = acc.to_be_bytes();
        out.push(a);
        if pad < 2 {
            out.push(b);
        }
        if pad < 1 {
            out.push(c);
        }
    }
    Some(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::{base64_decode, fill_events, Commitment, EventsError, Extraction, TransactionLogs};
    use quotebook_core::event::FillEvent;
    use quotebook_core::{BaseLots, Nonce, QuoteLotsPerBaseLot, Side, UnixTimestamp};
    use solana_address::Address;

    const QB: Address = Address::new_from_array([0x51; 32]);
    const OTHER: Address = Address::new_from_array([0x77; 32]);
    const TOKEN: Address = Address::new_from_array([0x06; 32]);

    fn event(nonce: u64) -> FillEvent {
        FillEvent {
            series: Address::new_from_array([1; 32]),
            maker: Address::new_from_array([2; 32]),
            taker: Address::new_from_array([3; 32]),
            nonce: Nonce::new(nonce),
            side: Side::Bid,
            price: QuoteLotsPerBaseLot::new(3),
            fill_size: BaseLots::new(4),
            base_atoms: 4_000,
            quote_atoms: 120,
            fee: 0,
            unix_timestamp: UnixTimestamp::new(1_700_000_000),
        }
    }

    fn b64(bytes: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for chunk in bytes.chunks(3) {
            let mut b = [0u8; 3];
            b[..chunk.len()].copy_from_slice(chunk);
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    s.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
                } else {
                    s.push('=');
                }
            }
        }
        s
    }

    fn invoke(p: &Address, depth: usize) -> String {
        format!("Program {p} invoke [{depth}]")
    }
    fn ok(p: &Address) -> String {
        format!("Program {p} success")
    }
    fn fail(p: &Address) -> String {
        format!("Program {p} failed: custom program error: 0x1")
    }
    fn data(e: &FillEvent) -> String {
        format!("Program data: {}", b64(&e.encode()))
    }

    fn run(logs: &[String]) -> Extraction {
        fill_events(
            &QB,
            &TransactionLogs {
                succeeded: true,
                commitment: Commitment::Finalized,
                logs,
            },
            Commitment::Finalized,
        )
        .unwrap()
    }

    fn nonces(x: &Extraction) -> (bool, Vec<u64>) {
        let (complete, v) = match x {
            Extraction::Complete(v) => (true, v),
            Extraction::Incomplete(v) => (false, v),
        };
        (complete, v.iter().map(|e| e.event.nonce.get()).collect())
    }

    #[test]
    fn test_base64_round_trip_and_rejects() {
        for len in 0..10 {
            let bytes: Vec<u8> = (0..len).map(|i: u8| i.wrapping_mul(37)).collect();
            assert_eq!(base64_decode(&b64(&bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_decode("abc"), None);
        assert_eq!(base64_decode("ab=c"), None);
        assert_eq!(base64_decode("a==="), None);
        assert_eq!(base64_decode("ab==ab=="), None);
        assert_eq!(base64_decode("ab!d"), None);
    }

    #[test]
    fn test_top_level_settle_event() {
        let logs = vec![
            invoke(&QB, 1),
            invoke(&TOKEN, 2),
            ok(&TOKEN),
            data(&event(1)),
            format!("Program {QB} consumed 8595 of 200000 compute units"),
            ok(&QB),
        ];
        assert_eq!(nonces(&run(&logs)), (true, vec![1]));
    }

    #[test]
    fn test_settle_by_cpi_at_depth_two() {
        let logs = vec![
            invoke(&OTHER, 1),
            invoke(&QB, 2),
            data(&event(2)),
            ok(&QB),
            ok(&OTHER),
        ];
        assert_eq!(nonces(&run(&logs)), (true, vec![2]));
    }

    #[test]
    fn test_spoofed_emitter_is_rejected() {
        // Top-level foreign program; and a foreign program nested inside quotebook's frame.
        let logs = vec![
            invoke(&OTHER, 1),
            data(&event(9)),
            ok(&OTHER),
            invoke(&QB, 1),
            invoke(&OTHER, 2),
            data(&event(8)),
            ok(&OTHER),
            data(&event(3)),
            ok(&QB),
        ];
        assert_eq!(nonces(&run(&logs)), (true, vec![3]));
    }

    #[test]
    fn test_failed_frame_drops_its_events_and_descendants() {
        // A caught ancestor failure cannot happen in today's runtime (a failed CPI aborts the
        // transaction); the rule is defence in depth, tested on synthetic lines.
        let logs = vec![
            invoke(&OTHER, 1),
            invoke(&OTHER, 2),
            invoke(&QB, 3),
            data(&event(4)),
            ok(&QB),
            fail(&OTHER),
            ok(&OTHER),
        ];
        assert_eq!(nonces(&run(&logs)), (true, vec![]));
    }

    #[test]
    fn test_truncated_and_unbalanced_logs_are_incomplete() {
        let truncated = vec![
            invoke(&QB, 1),
            data(&event(5)),
            ok(&QB),
            invoke(&QB, 1),
            "Log truncated".to_string(),
        ];
        assert_eq!(nonces(&run(&truncated)), (false, vec![5]));
        let cut_before_success = vec![invoke(&QB, 1), data(&event(6))];
        assert_eq!(nonces(&run(&cut_before_success)), (false, vec![]));
        let wrong_depth = vec![invoke(&QB, 2), data(&event(7)), ok(&QB)];
        assert_eq!(nonces(&run(&wrong_depth)), (false, vec![]));
        let mismatched = vec![invoke(&QB, 1), data(&event(7)), ok(&OTHER)];
        assert_eq!(nonces(&run(&mismatched)), (false, vec![]));
    }

    #[test]
    fn test_indices_and_garbage_data() {
        let logs = vec![
            invoke(&QB, 1),
            data(&event(10)),
            "Program data: not-base64!".to_string(),
            format!("Program data: {} {}", b64(&event(11).encode()), b64(b"x")),
            data(&event(12)),
            ok(&QB),
        ];
        let x = run(&logs);
        let Extraction::Complete(v) = &x else {
            panic!("{x:?}")
        };
        assert_eq!(
            v.iter()
                .map(|e| (e.index, e.event.nonce.get()))
                .collect::<Vec<_>>(),
            vec![(0, 10), (1, 12)]
        );
    }

    #[test]
    fn test_failed_or_unfinal_transactions_yield_nothing() {
        let logs = vec![invoke(&QB, 1), data(&event(1)), ok(&QB)];
        let tx = |succeeded, commitment| TransactionLogs {
            succeeded,
            commitment,
            logs: &logs,
        };
        assert_eq!(
            fill_events(
                &QB,
                &tx(false, Commitment::Finalized),
                Commitment::Finalized
            ),
            Err(EventsError::TransactionFailed)
        );
        assert_eq!(
            fill_events(&QB, &tx(true, Commitment::Confirmed), Commitment::Finalized),
            Err(EventsError::BelowCommitment {
                got: Commitment::Confirmed,
                required: Commitment::Finalized
            })
        );
        assert!(fill_events(&QB, &tx(true, Commitment::Confirmed), Commitment::Confirmed).is_ok());
    }
}
