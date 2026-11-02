// Builds the scenario's `[precompile, settle]` transaction instructions with the TypeScript SDK
// only (the quote signed here, with the maker's seed), and writes them in a line format the
// Rust test `sdk/tests/ts_settle.rs` replays in LiteSVM:
//   ix <program> <data hex>
//   acct <pubkey> <signer 0|1> <writable 0|1>
// Usage: node scripts/emit-settle.ts > vectors/ts-settle.txt

import { readFileSync } from "node:fs";
import { address, AccountRole, createPrivateKeyFromBytes } from "@solana/kit";
import { precompileInstruction, settleInstruction, signQuote, type Side } from "../src/index.ts";

const V = JSON.parse(readFileSync(new URL("../vectors/vectors.json", import.meta.url), "utf8"));
const sc = V.scenario;
const t = sc.terms;
const terms = {
  programId: address(t.program_id),
  deployment: new Uint8Array(Buffer.from(t.deployment, "hex")),
  maker: address(t.maker),
  series: address(t.series),
  taker: address(t.taker),
  side: t.side as Side,
  price: BigInt(t.price),
  size: BigInt(t.size),
  issuedAt: BigInt(t.issued_at),
  expiresAt: BigInt(t.expires_at),
  nonce: BigInt(t.nonce),
};
const key = await createPrivateKeyFromBytes(new Uint8Array(Buffer.from(sc.maker_seed, "hex")));
const signature = await signQuote(terms, key);
const ixs = [
  precompileInstruction(terms, signature),
  await settleInstruction(
    address(sc.program_id),
    terms,
    {
      taker: address(sc.taker),
      baseMint: address(sc.base_mint),
      quoteMint: address(sc.quote_mint),
      baseTokenProgram: address(sc.token_program),
      quoteTokenProgram: address(sc.token_program),
      makerBase: address(sc.maker_base),
      makerQuote: address(sc.maker_quote),
      takerBase: address(sc.taker_base),
      takerQuote: address(sc.taker_quote),
    },
    0,
    BigInt(sc.fill_size),
  ),
];
const lines: string[] = [];
for (const ix of ixs) {
  lines.push(`ix ${ix.programAddress} ${Buffer.from(ix.data ?? new Uint8Array()).toString("hex")}`);
  for (const a of ix.accounts ?? []) {
    const signer = a.role === AccountRole.READONLY_SIGNER || a.role === AccountRole.WRITABLE_SIGNER;
    const writable = a.role === AccountRole.WRITABLE || a.role === AccountRole.WRITABLE_SIGNER;
    lines.push(`acct ${a.address} ${signer ? 1 : 0} ${writable ? 1 : 0}`);
  }
}
process.stdout.write(lines.join("\n") + "\n");
