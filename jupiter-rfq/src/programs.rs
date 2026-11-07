//! The programs a fill transaction may call, and their instruction layouts
//! (docs/design/jupiter-rfq.md, "The fill-transaction validator").

use solana_address::Address;

/// The Order Engine on mainnet (toolkit `programs/order-engine/src/lib.rs`, `production`).
pub const ORDER_ENGINE: Address =
    Address::from_str_const("61DFfeTKM7trxYcPQCM78bJ794ddZprZpAwAnLiwTpYH");
/// Compute Budget.
pub const COMPUTE_BUDGET: Address =
    Address::from_str_const("ComputeBudget111111111111111111111111111111");
/// Associated Token Account.
pub const ASSOCIATED_TOKEN: Address =
    Address::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// System.
pub const SYSTEM: Address = Address::from_str_const("11111111111111111111111111111111");
/// SPL Token.
pub const TOKEN: Address = Address::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022.
pub const TOKEN_2022: Address =
    Address::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// Lighthouse, whose assertions wallets append (toolkit `order-engine-sdk/src/fill.rs`).
pub const LIGHTHOUSE: Address =
    Address::from_str_const("L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95");
/// Squads v4, whose wrapped transactions the prototype refuses.
pub const SQUADS_V4: Address =
    Address::from_str_const("SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf");

/// Lighthouse instructions allowed: assertions only. Its memory write would let an attacker
/// drain the signer (toolkit comment).
pub const LIGHTHOUSE_ALLOWED: [u8; 4] = [5, 6, 9, 10];

/// Anchor's discriminator of `fill`, `sha256("global:fill")[..8]` (toolkit
/// `idls/order_engine.json`).
pub const FILL_DISCRIMINATOR: [u8; 8] = [168, 96, 183, 163, 92, 10, 40, 160];
/// `fill`'s data: the discriminator, `input_amount`, `output_amount`, `expire_at`.
pub const FILL_ARGS_LEN: usize = 32;
/// The trailing payload after the arguments: `fee_bps: u16`, `flags: u8`,
/// `integrator_fee_bps: u16` (Quoting). The toolkit's README describes 3 bytes; Quoting warns
/// that a 3-byte read gives the wrong fee, so the prototype requires 5.
pub const FILL_PAYLOAD_LEN: usize = 5;
/// `fill`'s accounts (`idls/order_engine.json`), in order. The four token accounts are
/// optional: Anchor passes the program's own id for `None`, which selects a native-SOL path.
pub const FILL_ACCOUNTS: usize = 11;
/// `flags` bit 0: 0 for exact-in, 1 for exact-out. Every other bit is unknown and refused.
pub const FLAG_EXACT_OUT: u8 = 1;

/// A decoded `fill`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// Atoms the taker pays.
    pub input_amount: u64,
    /// Atoms the maker pays.
    pub output_amount: u64,
    /// The last Clock second the fill lands in: `fill` requires `expire_at >= now`.
    pub expire_at: i64,
    /// Jupiter's fee, in bps.
    pub fee_bps: u16,
    /// The flags byte.
    pub flags: u8,
    /// The integrator's fee, in bps.
    pub integrator_fee_bps: u16,
}

fn le_u64(data: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    Some(u64::from_le_bytes(data.get(at..end)?.try_into().ok()?))
}

fn le_u16(data: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    Some(u16::from_le_bytes(data.get(at..end)?.try_into().ok()?))
}

impl Fill {
    /// Decodes `fill`'s data with its 5-byte payload. `None` unless it is exactly 37 bytes with
    /// the right discriminator.
    #[must_use]
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() != FILL_ARGS_LEN.checked_add(FILL_PAYLOAD_LEN)? {
            return None;
        }
        if data.get(..8)? != FILL_DISCRIMINATOR {
            return None;
        }
        let expire_at = i64::from_le_bytes(data.get(24..32)?.try_into().ok()?);
        Some(Self {
            input_amount: le_u64(data, 8)?,
            output_amount: le_u64(data, 16)?,
            expire_at,
            fee_bps: le_u16(data, 32)?,
            flags: *data.get(34)?,
            integrator_fee_bps: le_u16(data, 35)?,
        })
    }

    /// The 37-byte data.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(37);
        out.extend_from_slice(&FILL_DISCRIMINATOR);
        out.extend_from_slice(&self.input_amount.to_le_bytes());
        out.extend_from_slice(&self.output_amount.to_le_bytes());
        out.extend_from_slice(&self.expire_at.to_le_bytes());
        out.extend_from_slice(&self.fee_bps.to_le_bytes());
        out.push(self.flags);
        out.extend_from_slice(&self.integrator_fee_bps.to_le_bytes());
        out
    }
}

/// A Compute Budget instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeBudget {
    /// Tag 2, `u32`.
    SetComputeUnitLimit(u32),
    /// Tag 3, micro-lamports per CU, `u64`.
    SetComputeUnitPrice(u64),
    /// Any other tag, or a length that does not match its tag.
    Other(Option<u8>),
}

impl ComputeBudget {
    /// Decodes the Borsh enum: a tag byte, then its argument, with nothing after.
    #[must_use]
    pub fn decode(data: &[u8]) -> Self {
        match (data.first().copied(), data.len()) {
            (Some(2), 5) => data
                .get(1..5)
                .and_then(|b| b.try_into().ok())
                .map_or(Self::Other(Some(2)), |b| {
                    Self::SetComputeUnitLimit(u32::from_le_bytes(b))
                }),
            (Some(3), 9) => data
                .get(1..9)
                .and_then(|b| b.try_into().ok())
                .map_or(Self::Other(Some(3)), |b| {
                    Self::SetComputeUnitPrice(u64::from_le_bytes(b))
                }),
            (tag, _) => Self::Other(tag),
        }
    }

    /// The instruction data.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Self::SetComputeUnitLimit(v) => {
                let mut out = vec![2];
                out.extend_from_slice(&v.to_le_bytes());
                out
            }
            Self::SetComputeUnitPrice(v) => {
                let mut out = vec![3];
                out.extend_from_slice(&v.to_le_bytes());
                out
            }
            Self::Other(tag) => vec![tag.unwrap_or(0)],
        }
    }
}
