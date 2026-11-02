// TypeScript SDK for quotebook (docs/PLAN-improvements.md, I8). It mirrors the Rust
// `quotebook-sdk` and `quotebook-core` and is checked byte for byte against vectors those
// crates generate (`ts/vectors/vectors.json`, `make vectors-check`). Only encodings, PDAs,
// decoders and signature checks live here; sending transactions is left to the caller and
// @solana/kit.

import {
  AccountRole,
  address,
  getAddressDecoder,
  getAddressEncoder,
  getProgramDerivedAddress,
  getPublicKeyFromAddress,
  signBytes,
  verifySignature,
  type Address,
  type Instruction,
  type SignatureBytes,
} from "@solana/kit";

// ---------------------------------------------------------------------------------------
// Constants (quotebook-core).

/** The 16-byte domain tag that starts every signed quote: `quotebook-v1` padded with zeros. */
export const QUOTE_DOMAIN = new Uint8Array([
  0x71, 0x75, 0x6f, 0x74, 0x65, 0x62, 0x6f, 0x6f, 0x6b, 0x2d, 0x76, 0x31, 0, 0, 0, 0,
]);
/** Length of a canonical quote, in bytes. */
export const QUOTE_LEN = 208;
/** An all-zero taker: an open quote. */
export const OPEN_TAKER = address("11111111111111111111111111111111");
/** The ed25519 signature-verification precompile. */
export const ED25519_PROGRAM = address("Ed25519SigVerify111111111111111111111111111");
/** The instructions sysvar. */
export const INSTRUCTIONS_SYSVAR = address("Sysvar1nstructions1111111111111111111111111");
/** Instruction tags. */
export const Tag = {
  InitConfig: 0,
  CreateSeries: 1,
  SetSeriesPaused: 2,
  InitMaker: 3,
  CancelAll: 4,
  CancelNonce: 5,
  Settle: 6,
  SetConfig: 7,
} as const;
/** The maker's side. */
export const Side = { Bid: 0, Ask: 1 } as const;
export type Side = (typeof Side)[keyof typeof Side];

const U64_MAX = (1n << 64n) - 1n;
const I64_MIN = -(1n << 63n);
const I64_MAX = (1n << 63n) - 1n;

/** Thrown for values or bytes outside the canonical encodings. */
export class QuotebookCodecError extends Error {}

function u64(v: bigint, name: string): bigint {
  if (v < 0n || v > U64_MAX) throw new QuotebookCodecError(`${name} is not a u64`);
  return v;
}
function i64(v: bigint, name: string): bigint {
  if (v < I64_MIN || v > I64_MAX) throw new QuotebookCodecError(`${name} is not an i64`);
  return v;
}

const addressEncoder = getAddressEncoder();
const addressDecoder = getAddressDecoder();
function addressBytes(a: Address): Uint8Array {
  return new Uint8Array(addressEncoder.encode(a));
}
function addressAt(b: Uint8Array, at: number): Address {
  return addressDecoder.decode(b.subarray(at, at + 32));
}
function equalBytes(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i]);
}

// ---------------------------------------------------------------------------------------
// Quotes.

/** Everything a maker commits to (`QuoteTerms`). Integers are bigints. */
export interface QuoteTerms {
  programId: Address;
  /** 16 bytes: the first 16 of the cluster's genesis hash. */
  deployment: Uint8Array;
  maker: Address;
  series: Address;
  /** `OPEN_TAKER` for an open quote. */
  taker: Address;
  side: Side;
  /** Ticks (quote lots per base lot); non-zero. */
  price: bigint;
  /** Base lots; non-zero. */
  size: bigint;
  issuedAt: bigint;
  expiresAt: bigint;
  nonce: bigint;
}

/** The canonical 208-byte encoding a maker signs (`Quote::encode`). */
export function encodeQuote(t: QuoteTerms): Uint8Array {
  if (t.deployment.length !== 16) throw new QuotebookCodecError("deployment is not 16 bytes");
  if (t.side !== Side.Bid && t.side !== Side.Ask) throw new QuotebookCodecError("bad side");
  if (u64(t.price, "price") === 0n || u64(t.size, "size") === 0n) {
    throw new QuotebookCodecError("price and size must be non-zero");
  }
  const out = new Uint8Array(QUOTE_LEN);
  const v = new DataView(out.buffer);
  out.set(QUOTE_DOMAIN, 0);
  out.set(addressBytes(t.programId), 16);
  out.set(t.deployment, 48);
  out.set(addressBytes(t.maker), 64);
  out.set(addressBytes(t.series), 96);
  out.set(addressBytes(t.taker), 128);
  out[160] = t.side;
  v.setBigUint64(168, t.price, true);
  v.setBigUint64(176, t.size, true);
  v.setBigInt64(184, i64(t.issuedAt, "issuedAt"), true);
  v.setBigInt64(192, i64(t.expiresAt, "expiresAt"), true);
  v.setBigUint64(200, u64(t.nonce, "nonce"), true);
  return out;
}

/** Decodes canonical quote bytes (`Quote::decode`): accepts exactly `encodeQuote`'s outputs. */
export function decodeQuote(b: Uint8Array): QuoteTerms {
  if (b.length !== QUOTE_LEN) throw new QuotebookCodecError("QuoteLengthMismatch");
  if (!equalBytes(b.subarray(0, 16), QUOTE_DOMAIN)) throw new QuotebookCodecError("WrongDomain");
  if (b.subarray(161, 168).some((x) => x !== 0)) throw new QuotebookCodecError("MalformedQuote");
  const side = b[160];
  if (side !== Side.Bid && side !== Side.Ask) throw new QuotebookCodecError("MalformedQuote");
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const price = v.getBigUint64(168, true);
  const size = v.getBigUint64(176, true);
  if (price === 0n || size === 0n) throw new QuotebookCodecError("MalformedQuote");
  return {
    programId: addressAt(b, 16),
    deployment: b.slice(48, 64),
    maker: addressAt(b, 64),
    series: addressAt(b, 96),
    taker: addressAt(b, 128),
    side,
    price,
    size,
    issuedAt: v.getBigInt64(184, true),
    expiresAt: v.getBigInt64(192, true),
    nonce: v.getBigUint64(200, true),
  };
}

// ---------------------------------------------------------------------------------------
// The ed25519 precompile and settle.

/** Layout of `sigverify::instruction_data`: count, padding, one offsets record, then the public
 * key, the signature and the message, all in this instruction (every index 0xFFFF). */
const PUBKEY_AT = 16;
const SIGNATURE_AT = 48;
const MESSAGE_AT = 112;
const SELF = 0xffff;

/** The self-contained, single-signature precompile data verifying `signature` over the quote
 * by `terms.maker` (`sigverify::instruction_data`). */
export function precompileData(t: QuoteTerms, signature: Uint8Array): Uint8Array {
  if (signature.length !== 64) throw new QuotebookCodecError("signature is not 64 bytes");
  const out = new Uint8Array(MESSAGE_AT + QUOTE_LEN);
  const v = new DataView(out.buffer);
  out[0] = 1;
  const offsets = [SIGNATURE_AT, SELF, PUBKEY_AT, SELF, MESSAGE_AT, QUOTE_LEN, SELF];
  offsets.forEach((x, i) => v.setUint16(2 + 2 * i, x, true));
  out.set(addressBytes(t.maker), PUBKEY_AT);
  out.set(signature, SIGNATURE_AT);
  out.set(encodeQuote(t), MESSAGE_AT);
  return out;
}

/** The precompile instruction for a signed quote. */
export function precompileInstruction(t: QuoteTerms, signature: Uint8Array): Instruction {
  return { programAddress: ED25519_PROGRAM, accounts: [], data: precompileData(t, signature) };
}

/** The accounts `settle` needs besides those the quote names (`SettleAccounts`). */
export interface SettleAccounts {
  taker: Address;
  baseMint: Address;
  quoteMint: Address;
  baseTokenProgram: Address;
  quoteTokenProgram: Address;
  makerBase: Address;
  makerQuote: Address;
  takerBase: Address;
  takerQuote: Address;
}

/** `settle` data: tag 6, `sig_ix_index: u8`, `fill_size: u64` (base lots). */
export function settleData(sigIxIndex: number, fillSize: bigint): Uint8Array {
  if (!Number.isInteger(sigIxIndex) || sigIxIndex < 0 || sigIxIndex > 255) {
    throw new QuotebookCodecError("sigIxIndex is not a u8");
  }
  const out = new Uint8Array(10);
  out[0] = Tag.Settle;
  out[1] = sigIxIndex;
  new DataView(out.buffer).setBigUint64(2, u64(fillSize, "fillSize"), true);
  return out;
}

/** The `settle` instruction, in the program's account order (`ix::settle`). */
export async function settleInstruction(
  programId: Address,
  t: QuoteTerms,
  a: SettleAccounts,
  sigIxIndex: number,
  fillSize: bigint,
): Promise<Instruction> {
  const [makerState] = await makerStatePda(programId, t.maker);
  const [delegate] = await delegatePda(programId, t.maker);
  const r = AccountRole;
  return {
    programAddress: programId,
    data: settleData(sigIxIndex, fillSize),
    accounts: [
      { address: a.taker, role: r.READONLY_SIGNER },
      { address: t.series, role: r.READONLY },
      { address: makerState, role: r.WRITABLE },
      { address: delegate, role: r.READONLY },
      { address: a.makerBase, role: r.WRITABLE },
      { address: a.makerQuote, role: r.WRITABLE },
      { address: a.takerBase, role: r.WRITABLE },
      { address: a.takerQuote, role: r.WRITABLE },
      { address: a.baseMint, role: r.READONLY },
      { address: a.quoteMint, role: r.READONLY },
      { address: a.baseTokenProgram, role: r.READONLY },
      { address: a.quoteTokenProgram, role: r.READONLY },
      { address: INSTRUCTIONS_SYSVAR, role: r.READONLY },
    ],
  };
}

// ---------------------------------------------------------------------------------------
// PDAs.

export type Pda = readonly [Address, number];

export async function configPda(programId: Address): Promise<Pda> {
  return getProgramDerivedAddress({ programAddress: programId, seeds: ["config"] });
}

export async function seriesPda(
  programId: Address,
  baseMint: Address,
  quoteMint: Address,
  index: bigint,
): Promise<Pda> {
  const le = new Uint8Array(8);
  new DataView(le.buffer).setBigUint64(0, u64(index, "index"), true);
  return getProgramDerivedAddress({
    programAddress: programId,
    seeds: ["series", addressBytes(baseMint), addressBytes(quoteMint), le],
  });
}

export async function makerStatePda(programId: Address, maker: Address): Promise<Pda> {
  return getProgramDerivedAddress({ programAddress: programId, seeds: ["maker", addressBytes(maker)] });
}

export async function delegatePda(programId: Address, maker: Address): Promise<Pda> {
  return getProgramDerivedAddress({
    programAddress: programId,
    seeds: ["delegate", addressBytes(maker)],
  });
}

// ---------------------------------------------------------------------------------------
// Accounts (quotebook-core::accounts). The caller checks that quotebook owns the account.

const VERSION = 1;
const TAG_CONFIG = 1;
const TAG_SERIES = 2;
const TAG_MAKER_STATE = 3;

function header(b: Uint8Array, len: number, tag: number): number {
  if (b.length !== len || b[0] !== tag || b[1] !== VERSION) {
    throw new QuotebookCodecError("wrong length, tag or version");
  }
  return b[2] ?? 0;
}

export interface ConfigAccount {
  bump: number;
  admin: Address;
  seriesCount: bigint;
  deploymentId: Uint8Array;
}

export function decodeConfig(b: Uint8Array): ConfigAccount {
  const bump = header(b, 64, TAG_CONFIG);
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return { bump, admin: addressAt(b, 8), seriesCount: v.getBigUint64(40, true), deploymentId: b.slice(48, 64) };
}

export interface SeriesAccount {
  bump: number;
  seriesIndex: bigint;
  deploymentId: Uint8Array;
  baseMint: Address;
  quoteMint: Address;
  baseLotSize: bigint;
  quoteLotSize: bigint;
  maxQuoteTtl: bigint;
  minSize: bigint;
  paused: boolean;
  baseIs2022: boolean;
  quoteIs2022: boolean;
}

export function decodeSeries(b: Uint8Array): SeriesAccount {
  const bump = header(b, 144, TAG_SERIES);
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return {
    bump,
    seriesIndex: v.getBigUint64(8, true),
    deploymentId: b.slice(16, 32),
    baseMint: addressAt(b, 32),
    quoteMint: addressAt(b, 64),
    baseLotSize: v.getBigUint64(96, true),
    quoteLotSize: v.getBigUint64(104, true),
    maxQuoteTtl: v.getBigInt64(112, true),
    minSize: v.getBigUint64(120, true),
    paused: b[128] !== 0,
    baseIs2022: b[129] !== 0,
    quoteIs2022: b[130] !== 0,
  };
}

/** Nonces the replay ring tracks above `windowBase`. */
export const WINDOW_BITS = 16_384n;

/** A `NonceWindow::check` outcome. */
export type NonceCheck = "ok" | "NonceUsed" | "NonceCancelled" | "NonceOutOfWindow";

export interface MakerStateAccount {
  bump: number;
  maker: Address;
  fills: bigint;
  delegateBump: number;
  windowBase: bigint;
  lastCancelAll: bigint;
  /** The 256 words of the 16,384-bit ring: bit `n mod 16,384` for nonces in the window. */
  used: BigUint64Array;
  /** Whether `nonce` can still be filled, as `settle` would judge it (`NonceWindow::check`). */
  check(nonce: bigint): NonceCheck;
}

export function decodeMakerState(b: Uint8Array): MakerStateAccount {
  const bump = header(b, 2120, TAG_MAKER_STATE);
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const windowBase = v.getBigUint64(56, true);
  const lastCancelAll = v.getBigUint64(64, true);
  const used = new BigUint64Array(256);
  for (let i = 0; i < 256; i++) used[i] = v.getBigUint64(72 + 8 * i, true);
  return {
    bump,
    maker: addressAt(b, 8),
    fills: v.getBigUint64(40, true),
    delegateBump: b[48] ?? 0,
    windowBase,
    lastCancelAll,
    used,
    check(nonce: bigint): NonceCheck {
      u64(nonce, "nonce");
      if (nonce < windowBase) return nonce < lastCancelAll ? "NonceCancelled" : "NonceOutOfWindow";
      // Beyond the window top a fill slides the window: the nonce is fillable.
      if (nonce - windowBase >= WINDOW_BITS) return "ok";
      const bit = nonce % WINDOW_BITS;
      const word = used[Number(bit / 64n)] ?? 0n;
      return (word >> (bit % 64n)) & 1n ? "NonceUsed" : "ok";
    },
  };
}

// ---------------------------------------------------------------------------------------
// Fill events (quotebook-core::event).

export const FILL_EVENT_TAG = new TextEncoder().encode("qb-fill1");
export const FILL_EVENT_LEN = 168;

export interface FillEvent {
  series: Address;
  maker: Address;
  taker: Address;
  nonce: bigint;
  side: Side;
  price: bigint;
  fillSize: bigint;
  baseAtoms: bigint;
  quoteAtoms: bigint;
  fee: bigint;
  unixTimestamp: bigint;
}

export function encodeFillEvent(e: FillEvent): Uint8Array {
  const out = new Uint8Array(FILL_EVENT_LEN);
  const v = new DataView(out.buffer);
  out.set(FILL_EVENT_TAG, 0);
  out.set(addressBytes(e.series), 8);
  out.set(addressBytes(e.maker), 40);
  out.set(addressBytes(e.taker), 72);
  v.setBigUint64(104, u64(e.nonce, "nonce"), true);
  out[112] = e.side;
  v.setBigUint64(120, u64(e.price, "price"), true);
  v.setBigUint64(128, u64(e.fillSize, "fillSize"), true);
  v.setBigUint64(136, u64(e.baseAtoms, "baseAtoms"), true);
  v.setBigUint64(144, u64(e.quoteAtoms, "quoteAtoms"), true);
  v.setBigUint64(152, u64(e.fee, "fee"), true);
  v.setBigInt64(160, i64(e.unixTimestamp, "unixTimestamp"), true);
  return out;
}

/** Decodes a `sol_log_data` payload as a fill event. Context-free: it does not say who logged
 * it. Use the Rust `quotebook-sdk::events::fill_events` rules for provenance. */
export function decodeFillEvent(b: Uint8Array): FillEvent {
  if (b.length !== FILL_EVENT_LEN) throw new QuotebookCodecError("Length");
  if (!equalBytes(b.subarray(0, 8), FILL_EVENT_TAG)) throw new QuotebookCodecError("Tag");
  const side = b[112];
  if ((side !== Side.Bid && side !== Side.Ask) || b.subarray(113, 120).some((x) => x !== 0)) {
    throw new QuotebookCodecError("Malformed");
  }
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return {
    series: addressAt(b, 8),
    maker: addressAt(b, 40),
    taker: addressAt(b, 72),
    nonce: v.getBigUint64(104, true),
    side,
    price: v.getBigUint64(120, true),
    fillSize: v.getBigUint64(128, true),
    baseAtoms: v.getBigUint64(136, true),
    quoteAtoms: v.getBigUint64(144, true),
    fee: v.getBigUint64(152, true),
    unixTimestamp: v.getBigInt64(160, true),
  };
}

// ---------------------------------------------------------------------------------------
// Signatures.

/** Signs the canonical quote with a 32-byte Ed25519 private key (WebCrypto via @solana/kit). */
export async function signQuote(t: QuoteTerms, privateKey: CryptoKey): Promise<Uint8Array> {
  return new Uint8Array(await signBytes(privateKey, encodeQuote(t)));
}

/** Whether `signature` is `maker`'s Ed25519 signature over `message` (the 208 quote bytes). */
export async function verifyQuoteSignature(
  maker: Address,
  message: Uint8Array,
  signature: Uint8Array,
): Promise<boolean> {
  if (signature.length !== 64) return false;
  const key = await getPublicKeyFromAddress(maker);
  return verifySignature(key, signature as SignatureBytes, message);
}
