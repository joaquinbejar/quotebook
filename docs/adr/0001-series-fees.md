# ADR 0001: immutable per-series taker fees

**Status:** proposed (I10 of [`PLAN-improvements.md`](../PLAN-improvements.md)). For review by
Codex and SOLANA-PROJECTS; no code is written before both accept it.

**Already decided** by SOLANA-PROJECTS (msg 43378):
- the fee is paid by the taker;
- `MAX_FEE_BPS = 100`;
- the devnet default is 0;
- the fee is an immutable per-series parameter.

## Context

v1 has no fee (PLAN open question 1). A series is created by the admin and never changes its
terms. A signed quote binds the series address (`quote.series`), so the series' terms are part
of what the maker signed. `Series` is 144 bytes and its last field, `_reserved: [u8; 8]`, is
zero in every series ever created.

## Decision

### Storage

`fee_bps: u16` (little-endian) takes bytes 0 and 1 of `Series._reserved`. Bytes 2 to 7 stay
reserved and must be zero.

- **No migration:** every existing series reads `fee_bps = 0`. The account `VERSION` stays 1.
- **`create_series` arguments** grow from 32 to 34 bytes: the four v1 fields, then `fee_bps:
  u16`.
  - The decoder accepts both lengths: 32 bytes means `fee_bps = 0`, so v1 clients still work.
  - Each length is canonical, and encode/decode round-trip at both lengths.
  - `fee_bps > MAX_FEE_BPS` (100) fails with the new error `FeeTooHigh` (132).
- **No instruction ever writes `fee_bps` after creation.** That is the immutability.

**Why immutable:** the quote binds the series address, so with an immutable fee the signed
quote determines exactly what every party pays and receives. A mutable fee would let the admin
change a taker's cost after the maker signed, and change it between a quote's issue and its
fill.

### Who pays, and how much

The taker pays the fee, in quote atoms, on top of the quoted amounts. The maker's flows are
exactly the quoted ones on both sides.

```text
quote_atoms = price × fill × quote_lot_size            (as v1, checked u128 → u64)
fee         = ceil(quote_atoms × fee_bps / 10,000)     (checked u128; rounded against the taker)
```

| Side (maker's) | Maker | Taker | Fee account |
| --- | --- | --- | --- |
| Ask (maker sells base) | pays `base_atoms` (delegate), receives `quote_atoms` | receives `base_atoms`, pays `quote_atoms + fee` | receives `fee` from the taker |
| Bid (maker buys base) | receives `base_atoms`, pays `quote_atoms` (delegate) | pays `base_atoms`, receives `quote_atoms - fee` | receives `fee` from the maker's delegate transfer, out of the maker's `quote_atoms` |

- **Bid, nothing left for the taker:** if `quote_atoms - fee` would be 0, `settle` fails with
  the new error `FeeExceedsProceeds` (133). A taker must never sell base for nothing. With
  `fee_bps <= 100`, this happens only when `quote_atoms < 100`, a dust fill of a badly lotted
  series.
- **`fee_bps = 0`:** no third transfer and no extra account. `settle`'s accounts and amounts
  are exactly v1's.
- **The maker's allowance** drops by exactly its quoted outflow on both sides (`base_atoms` on
  an Ask, `quote_atoms` on a Bid). The fee never comes out of the maker's pocket beyond what it
  quoted.

### The fee account

There is no stored recipient address: `Series` has 6 bytes left, not 32. The fee account is
**any token account of the series' quote mint whose owner is the Config PDA**. Anyone can
create one, for example the Config PDA's associated token account.

- **Position:** `settle` takes it as account 13, after the instructions sysvar, and only when
  `fee_bps > 0`. If it is absent, `settle` fails with `NotEnoughAccountKeys`.
- **Checks,** all with existing helpers:
  - the Config PDA is re-derived from `["config", bump]`;
  - `token::check_account(fee_account, quote_token_program, quote_mint, config_pda,
    Role::Receives)`. That checks it is writable, owned by the quote mint's token program, holds
    the quote mint, is owned by the Config PDA and is initialized. On Token-2022 it also rejects
    the extensions forbidden on a receiving account (MemoTransfer).

### Withdrawals

There is a new instruction, `withdraw_fees` (tag 8).

- **Args:** `amount: u64`, which must be greater than 0.
- **Accounts:**
  1. `[signer]` admin;
  2. `[]` config;
  3. `[writable]` fee account;
  4. `[writable]` destination;
  5. `[]` quote mint;
  6. `[]` token program.
- **Checks, in this order:**
  - admin == `config.admin`;
  - the Config PDA re-derives;
  - the fee account passes the `settle` checks above;
  - the destination is any initialized token account of the same mint and program;
  - `amount <= balance`, which fails with `InsufficientFees` (134). It is checked before the
    CPI, for a clear error.
- **Transfer:** `transfer_checked` signed with the Config PDA's seeds.

## Tests and measurements

- **Amounts:** unit and property tests of `fee` and `quote_atoms - fee`:
  - rounding is up;
  - the arithmetic is checked;
  - the `FeeExceedsProceeds` edge.
- **Settle:** LiteSVM tests with fee 0 and fee > 0, both sides, SPL Token and Token-2022, with
  exact balances for all five accounts. Negative tests for each check on the fee account:
  - missing;
  - wrong mint;
  - wrong owner;
  - wrong token program;
  - not writable;
  - MemoTransfer on Token-2022.
- **`withdraw_fees`** negative tests:
  - signer not the admin;
  - wrong Config PDA;
  - source not owned by the Config PDA;
  - wrong mint;
  - amount above the balance;
  - zero amount.
  Plus the happy path to a destination of another owner.
- **Conservation:** I6's conservation test gains the fee account. The model charges the fee,
  and per-mint conservation then covers fees.
- **CU** (`make bench`): measured rows for fee 0 and fee > 0, SPL and Token-2022, both sides,
  and for `withdraw_fees`. The fee-0 row is measured, not assumed equal to v1.
- **Fill event (I7):** its `fee` field carries the amount, and its tests cover fee > 0.

## Alternatives considered

- **Maker-paid fee:** rejected. It changes the maker's economics relative to its signed price,
  and makers would price it in anyway. The taker-paid fee is the coordinator's decision.
- **A mutable fee, set by the admin:** rejected. Signed quotes would no longer fix what parties
  pay (see Why immutable).
- **A stored recipient address:** rejected. There is no room in `Series`, and a layout change
  would break every existing series. Ownership by the Config PDA needs no storage and keeps
  withdrawal under the admin.
- **A fee in base atoms:** rejected. The quote mint is the numeraire, and one fee mint per
  series keeps the accounting simple.

## Consequences

- A compatible upgrade in the sense of [`governance.md`](../governance.md): the v1 quote
  contract is unchanged, and every series created before has fee 0, so it behaves exactly as
  before.
- New error codes 132 to 134 are appended, and existing codes keep their values.
- The fee path joins the audit scope ([`audit-scope.md`](../audit-scope.md), question 5) before
  the freeze.
