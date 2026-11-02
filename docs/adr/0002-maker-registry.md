# ADR 0002: a permissionless on-chain maker registry

**Status:** accepted (I10 of [`PLAN-improvements.md`](../PLAN-improvements.md)): accepted by
Codex and SOLANA-PROJECTS (PR #31), implemented in the I10 code PR.

## Context

A taker has to know where makers listen. The maker service speaks WebSocket. Today the
endpoint comes from out-of-band configuration (`quotebook-taker --maker ws://…`). The program
already knows every maker that ever traded: each has a `MakerState`. That account is written by
every `settle` of that maker, and its layout is fixed: a breaking change if extended (PLAN 3.1).

## Decision

### The account

A new account, `MakerProfile`, at PDA `["profile", maker]`, owned by quotebook with tag 4. It
is written only by the maker and is never touched by `settle`, so it adds no write contention
to fills.

```text
MakerProfile (208 bytes)
  hdr: Hdr               8   (tag 4, version 1, bump)
  maker: Address        32
  endpoint_len: u8       1
  name_len: u8           1
  _pad: [u8; 6]          6
  endpoint: [u8; 128]  128   (endpoint_len bytes used, the rest zero)
  name: [u8; 32]        32   (name_len bytes used, the rest zero)
```

Validation, in the program and in the client decoder:
- `endpoint`:
  - 7 to 128 bytes, starting with `wss://`;
  - every byte printable ASCII without space (0x21 to 0x7E).
- `name`:
  - 0 to 32 bytes;
  - printable ASCII, space allowed (0x20 to 0x7E).
- **Canonical bytes:** every byte that carries no data is zero. That covers the header's
  `_pad`, the profile's `_pad`, and the tails of `endpoint` and `name` past their lengths. This
  keeps the encoding canonical, as I6's decoders require. The decoder checks it, and so does
  the program on every write.
- Anything else fails with the new error `InvalidProfile` (134).

### Instructions

- **`set_profile` (tag 9).**
  - Args: `endpoint_len`, the endpoint bytes, `name_len`, the name bytes.
  - Accounts:
    1. `[signer]` maker;
    2. `[signer, writable]` payer;
    3. `[writable]` profile;
    4. `[]` system program.
  - It creates the profile if it is missing, with the same pre-funding-safe creation as
    `init_maker`, or overwrites it.
- **`close_profile` (tag 10).**
  - Accounts:
    1. `[signer]` maker;
    2. `[writable]` profile;
    3. `[writable]` lamport destination.
  - It zeroes the data and returns the rent. Closing is safe, unlike `MakerState`, because the
    profile holds no replay state: re-creating it later only re-publishes an endpoint.
- **Ownership checks.** On an existing profile, both an update and a close verify all of
  these:
  - it is owned by quotebook;
  - the tag is 4 and the version is 1;
  - the address re-derives as `["profile", signer]` with the stored bump;
  - the stored `maker` equals the signer.
  Any mismatch fails with `InvalidPda` or `NotAuthority`. A maker can therefore only create,
  update or close its own profile, and a substituted account (another maker's profile, or a
  forged account at the right address) is rejected.

### Discovery and curation

- **Discovery:** `quotebook-taker --discover` lists profiles with `getProgramAccounts`, filtered
  by data size 208 and tag 4 at offset 0. Each profile is decoded and validated client-side;
  invalid ones are skipped and reported.
- **Curation stays with the taker:**
  - `--allow <MAKER>…` restricts requests to a local allowlist;
  - without it, every listed maker is asked;
  - each quote is checked as today: signature, every on-chain rule, then ranking. A registry
    entry grants nothing: a quote from a listed maker is verified exactly like any other.
- **Identity binding** (Codex, ADR review). A quote obtained through a discovered endpoint is
  accepted only if its signed `quote.maker` equals the maker of the profile the endpoint came
  from. `--allow` applies to that verified `quote.maker` as well, not only to the profile list.
  Otherwise an allowlisted profile, or a compromised endpoint, could return another maker's
  valid quote. `taker::check` gains an expected-maker argument for this.
- **Untrusted input:** the endpoint and the name are maker-provided text. The taker CLI treats
  them like the maker service's messages: never echoed unredacted in errors, and connected to
  only over `wss://`.

## Tests

- **LiteSVM:**
  - create, update and close;
  - a pre-funded PDA;
  - a profile written by a non-maker signer fails with `InvalidPda`;
  - an unauthorized close fails: another signer, or the right signer with another maker's
    profile;
  - a substituted profile on update and on close fails: a forged account owned by quotebook at
    the right address with the wrong tag, version or stored maker, and an account not owned by
    quotebook;
  - each `InvalidProfile` rule at its boundaries:
    - endpoint lengths 6/7 and 128/129;
    - a missing `wss://`;
    - a space or a control byte;
    - non-zero bytes in either `_pad` or in either text tail;
    - a name of 32/33 bytes.
- **Contention:** `settle` does not take the profile, checked by an account-list test.
- **Decoder fuzzing:** the profile decoder joins I6's `accounts_decode` target, as a structured
  layout with an accepting path.
- **Discovery:** a taker-CLI test over a LiteSVM-backed RPC mock, with valid profiles, invalid
  ones and the allowlist. A mock endpoint that returns another maker's valid quote is rejected,
  both with and without `--allow`. So is an allowlisted endpoint returning an unallowlisted
  maker's quote.
- **CU:** `set_profile` (create and update) and `close_profile` rows in `make bench`.

## Alternatives considered

- **A permissioned registry, admin-approved:** rejected. It adds an admin power and a
  gatekeeper. It also cannot stop an unlisted maker from quoting: any key can sign a quote and
  call `settle`. Curation belongs to the taker.
- **The endpoint in `MakerState`:** rejected. It is a layout change (breaking), and it would
  put text into the account every `settle` writes.
- **An off-chain registry file in this repository:** rejected. It is centralized, goes stale,
  and needs a PR to join.
- **A DNS or HTTP well-known file per maker:** possible later, alongside this. It does not
  replace an on-chain list that anyone can read.

## Consequences

- Two new instructions (`set_profile`, `close_profile`) and one account type, plus error 134
  (`InvalidProfile`; the fee ADR takes 132 and 133). `settle` and every v1 account
  are unchanged, so it is a compatible upgrade ([`governance.md`](../governance.md)).
- The registry joins the audit scope's "in scope" table at the freeze (I5).
