// The TypeScript SDK against the vectors the Rust crates generate (ts/vectors/vectors.json,
// `cargo run -p quotebook-sdk --example vectors`). Every assertion is byte equality.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { address, AccountRole, createPrivateKeyFromBytes } from "@solana/kit";

import {
  configPda,
  decodeConfig,
  decodeFillEvent,
  decodeMakerProfile,
  decodeMakerState,
  decodeQuote,
  decodeSeries,
  delegatePda,
  encodeFillEvent,
  encodeQuote,
  fee,
  feeAccountAddress,
  makerStatePda,
  precompileData,
  profilePda,
  seriesPda,
  settleInstruction,
  signQuote,
  verifyQuoteSignature,
  type QuoteTerms,
  type Side,
} from "../src/index.ts";

const path = process.env.QUOTEBOOK_VECTORS ?? new URL("../vectors/vectors.json", import.meta.url);
const V = JSON.parse(readFileSync(path, "utf8"));

const hex = (b: Uint8Array) => Buffer.from(b).toString("hex");
const bytes = (h: string) => new Uint8Array(Buffer.from(h, "hex"));

function terms(j: any): QuoteTerms {
  return {
    programId: address(j.program_id),
    deployment: bytes(j.deployment),
    maker: address(j.maker),
    series: address(j.series),
    taker: address(j.taker),
    side: j.side as Side,
    price: BigInt(j.price),
    size: BigInt(j.size),
    issuedAt: BigInt(j.issued_at),
    expiresAt: BigInt(j.expires_at),
    nonce: BigInt(j.nonce),
  };
}

const sc = V.scenario;
const accounts = {
  taker: address(sc.taker),
  baseMint: address(sc.base_mint),
  quoteMint: address(sc.quote_mint),
  baseTokenProgram: address(sc.token_program),
  quoteTokenProgram: address(sc.token_program),
  makerBase: address(sc.maker_base),
  makerQuote: address(sc.maker_quote),
  takerBase: address(sc.taker_base),
  takerQuote: address(sc.taker_quote),
};

test("quotes encode, decode and round-trip", () => {
  for (const q of V.quotes) {
    const t = terms(q.terms);
    assert.equal(hex(encodeQuote(t)), q.encoded);
    assert.equal(hex(encodeQuote(decodeQuote(bytes(q.encoded)))), q.encoded);
  }
});

test("decodeQuote rejects non-canonical bytes", () => {
  const good = bytes(V.quotes[0].encoded);
  const bad = (edit: (b: Uint8Array) => Uint8Array, why: string) =>
    assert.throws(() => decodeQuote(edit(good.slice())), new RegExp(why));
  bad((b) => b.subarray(0, 207), "QuoteLengthMismatch");
  bad((b) => ((b[0] = 0), b), "WrongDomain");
  bad((b) => ((b[161] = 1), b), "MalformedQuote");
  bad((b) => ((b[160] = 2), b), "MalformedQuote");
  bad((b) => (b.fill(0, 168, 176), b), "MalformedQuote");
});

test("signatures: TypeScript signs exactly what Rust signed, and verifies", async () => {
  const key = await createPrivateKeyFromBytes(bytes(sc.maker_seed));
  for (const q of V.quotes) {
    assert.equal(hex(await signQuote(terms(q.terms), key)), q.signature);
  }
  for (const s of V.signatures) {
    assert.equal(
      await verifyQuoteSignature(address(s.pubkey), bytes(s.message), bytes(s.signature)),
      s.valid,
    );
  }
});

test("precompile data and settle instructions", async () => {
  for (const q of V.quotes) {
    const t = terms(q.terms);
    assert.equal(hex(precompileData(t, bytes(q.signature))), q.precompile_data);
    const ix = await settleInstruction(
      address(V.program_id),
      t,
      accounts,
      q.sig_ix_index,
      BigInt(q.fill_size),
    );
    assert.equal(ix.programAddress, q.settle.program_id);
    assert.equal(hex(ix.data as Uint8Array), q.settle.data);
    const metas = (ix.accounts ?? []).map((a) => ({
      pubkey: a.address,
      is_signer: a.role === AccountRole.READONLY_SIGNER || a.role === AccountRole.WRITABLE_SIGNER,
      is_writable: a.role === AccountRole.WRITABLE || a.role === AccountRole.WRITABLE_SIGNER,
    }));
    assert.deepEqual(metas, q.settle.accounts);
    // The same fill on a fee series: account 13 is the canonical fee account, writable.
    const feeAccount = await feeAccountAddress(
      address(V.program_id),
      address(sc.quote_mint),
      address(sc.token_program),
    );
    const withFee = await settleInstruction(
      address(V.program_id),
      t,
      { ...accounts, feeAccount },
      q.sig_ix_index,
      BigInt(q.fill_size),
    );
    const feeMetas = (withFee.accounts ?? []).map((a) => ({
      pubkey: a.address,
      is_signer: a.role === AccountRole.READONLY_SIGNER || a.role === AccountRole.WRITABLE_SIGNER,
      is_writable: a.role === AccountRole.WRITABLE || a.role === AccountRole.WRITABLE_SIGNER,
    }));
    assert.equal(hex(withFee.data as Uint8Array), q.settle_with_fee.data);
    assert.deepEqual(feeMetas, q.settle_with_fee.accounts);
  }
});

test("fees: floor in the taker's favour", () => {
  for (const f of V.fees) assert.equal(fee(BigInt(f.quote_atoms), f.fee_bps).toString(), f.fee);
  assert.throws(() => fee(-1n, 1));
  assert.throws(() => fee(1n, 70_000));
});

test("PDAs and bumps", async () => {
  const program = address(V.program_id);
  for (const p of V.pdas) {
    if (p.kind === "fee_account") {
      const a = await feeAccountAddress(program, address(p.quote_mint), address(p.token_program));
      assert.equal(a, p.address, p.kind);
      continue;
    }
    const [a, bump] =
      p.kind === "config"
        ? await configPda(program)
        : p.kind === "series"
          ? await seriesPda(program, address(p.base_mint), address(p.quote_mint), BigInt(p.index))
          : p.kind === "maker_state"
            ? await makerStatePda(program, address(p.maker))
            : p.kind === "profile"
              ? await profilePda(program, address(p.maker))
              : await delegatePda(program, address(p.maker));
    assert.equal(a, p.address, p.kind);
    assert.equal(bump, p.bump, p.kind);
  }
});

test("account decoders", () => {
  const c = V.accounts.config;
  const config = decodeConfig(bytes(c.data));
  assert.deepEqual(
    [config.bump, config.admin, config.seriesCount.toString(), hex(config.deploymentId)],
    [c.bump, c.admin, c.series_count, c.deployment_id],
  );
  const s = V.accounts.series;
  const series = decodeSeries(bytes(s.data));
  assert.deepEqual(
    [
      series.bump,
      series.seriesIndex.toString(),
      hex(series.deploymentId),
      series.baseMint,
      series.quoteMint,
      series.baseLotSize.toString(),
      series.quoteLotSize.toString(),
      series.maxQuoteTtl.toString(),
      series.minSize.toString(),
      series.paused,
      series.baseIs2022,
      series.quoteIs2022,
      series.feeBps,
    ],
    [
      s.bump,
      s.series_index,
      s.deployment_id,
      s.base_mint,
      s.quote_mint,
      s.base_lot_size,
      s.quote_lot_size,
      s.max_quote_ttl,
      s.min_size,
      s.paused,
      s.base_is_2022,
      s.quote_is_2022,
      s.fee_bps,
    ],
  );
  const p = V.accounts.maker_profile;
  const prof = decodeMakerProfile(bytes(p.data));
  assert.deepEqual(
    [prof.bump, prof.maker, prof.endpoint, prof.name],
    [p.bump, p.maker, p.endpoint, p.name],
  );
  // Every canonical rule, one mutation each.
  const mutate = (edit: (b: Uint8Array) => void) => {
    const b = bytes(p.data);
    edit(b);
    assert.throws(() => decodeMakerProfile(b), /InvalidProfile|wrong length/);
  };
  mutate((b) => (b[3] = 1)); // header padding
  mutate((b) => (b[42] = 1)); // profile padding
  mutate((b) => (b[40] = 6)); // endpoint too short
  mutate((b) => (b[40] = 129)); // endpoint too long
  mutate((b) => (b[41] = 33)); // name too long
  mutate((b) => (b[48] = 0x68)); // "hss://"
  mutate((b) => (b[60] = 0x20)); // a space in the endpoint
  mutate((b) => (b[180] = 0x09)); // a tab in the name
  mutate((b) => (b[175] = 0x61)); // a byte past the endpoint
  mutate((b) => (b[207] = 0x61)); // a byte past the name
  mutate((b) => (b[0] = 3)); // another account type
  const m = V.accounts.maker_state;
  const ms = decodeMakerState(bytes(m.data));
  assert.deepEqual(
    [ms.bump, ms.maker, ms.fills.toString(), ms.delegateBump, ms.windowBase.toString(), ms.lastCancelAll.toString()],
    [m.bump, m.maker, m.fills, m.delegate_bump, m.window_base, m.last_cancel_all],
  );
  for (const k of m.checks) assert.equal(ms.check(BigInt(k.nonce)), k.result, `nonce ${k.nonce}`);
  // Wrong type, length or version.
  assert.throws(() => decodeSeries(bytes(c.data)));
  assert.throws(() => decodeConfig(bytes(c.data).subarray(0, 63)));
  const v2 = bytes(c.data);
  v2[1] = 2;
  assert.throws(() => decodeConfig(v2));
});

test("fill events", () => {
  for (const e of V.fill_events) {
    const ev = decodeFillEvent(bytes(e.encoded));
    assert.deepEqual(
      [
        ev.series,
        ev.maker,
        ev.taker,
        ev.nonce.toString(),
        ev.side,
        ev.price.toString(),
        ev.fillSize.toString(),
        ev.baseAtoms.toString(),
        ev.quoteAtoms.toString(),
        ev.fee.toString(),
        ev.unixTimestamp.toString(),
      ],
      [e.series, e.maker, e.taker, e.nonce, e.side, e.price, e.fill_size, e.base_atoms, e.quote_atoms, e.fee, e.unix_timestamp],
    );
    assert.equal(hex(encodeFillEvent(ev)), e.encoded);
  }
  const bad = bytes(V.fill_events[0].encoded);
  bad[113] = 1;
  assert.throws(() => decodeFillEvent(bad), /Malformed/);
});
