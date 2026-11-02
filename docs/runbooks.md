# Runbooks

Milestone I4 of [`PLAN-improvements.md`](PLAN-improvements.md). Each runbook gives the exact
commands and what to check afterwards. The examples are for devnet; on mainnet the keys are
those of [`governance.md`](governance.md).

## Common setup

```sh
export PATH=~/.local/share/solana/install/active_release/bin:$PATH   # Solana CLI 4.4.0
cargo build --release -p quotebook-cli
A=target/release/quotebook-admin
# zsh does not split a scalar, so the shared flags go in an array.
QB=(--url https://api.devnet.solana.com --program-id 3Aq1AwFiK9ibNBLxvYQ18KLT9WbLwENQa3ZZViDW6mbo)
PAYER=~/.config/solana/id.json                          # pays fees, no authority
ADMIN=~/.config/solana/quotebook-devnet/admin.json      # config admin (I3)
```

**Rules that apply to every runbook:**
- Keys are file paths and are never printed or committed.
- RPC URLs may carry API keys. Pass them as arguments or environment variables, never in files.
  The CLIs redact them from errors.
- Stop services by PID (`kill <pid>`), never by name.
- Capture `solana program deploy`/`write-buffer` output with an allowlist: they print a buffer
  recovery phrase.

## 1. Incident: stop fills on a series

The admin pauses the series. `settle` then fails with `SeriesPaused` (125) for every quote on
it, whatever is outstanding. Makers, nonces and balances are untouched.

```sh
$A pause $QB --admin $ADMIN --payer $PAYER --series <SERIES>
$A show-series $QB --series <SERIES>          # check: paused 1
```

**Pause everything.** There is no global pause, so it is one transaction per series. Series
`i` is the PDA `["series", base_mint, quote_mint, i_le]`, so the list comes from the series
recorded in `devnet.md` (or the mainnet record), 0 to `series_count - 1`:

```sh
$A show $QB                                    # series_count
for s in <SERIES_0> <SERIES_1>; do $A pause $QB --admin $ADMIN --payer $PAYER --series $s; done
```

**Resume** after the fix: `$A resume …` per series, then `show-series` shows `paused 0`.

`Config` has no reserved bytes, so a global pause flag would need a layout change. It is listed
as an audit-scope question (I5), not changed here.

## 2. Compromised maker key or maker service

Order matters: first make the maker's accounts unusable, then clean up.

1. **Revoke the delegate** on every token account the maker pays from. This stops every
   fill from that account at once: `settle`'s transfer is signed by the delegate PDA, and the
   token program refuses it with `OwnerMismatch` (custom error 4) once the delegate is gone. It
   holds for any quote, whether signed by the real key, a stolen key or an outstanding quote.

   ```sh
   $A revoke $QB --maker <MAKER_KEYPAIR> --payer $PAYER --account <MAKER_TOKEN_ACCOUNT>
   spl-token display <MAKER_TOKEN_ACCOUNT> --url devnet   # check: Delegation: (not set)
   ```

   If the maker key itself is stolen, the attacker can approve the delegate again. So step 3
   (move the funds) must follow at once, from the same machine that ran step 1.
2. **Cancel every outstanding nonce:** raise the watermark above the highest nonce ever
   issued. The service logs each `nonce=`. Any value above them works, for example the current
   highest plus 16,384.

   ```sh
   $A cancel-all $QB --maker <MAKER_KEYPAIR> --payer $PAYER --new-min <NONCE>
   ```

   A quote with a lower nonce then fails with `NonceCancelled` (119).
3. **Move the funds** to accounts owned by a new key:
   `spl-token transfer … --owner <MAKER_KEYPAIR>`.
4. **Stop the service** by PID. Start a new one only with a new maker key: `init-maker` and
   `approve` for the new key. The old `MakerState` stays (it is permanent, PLAN 3.2).

## 3. Maker service restart

Every start resumes fail-closed. For `max_ttl_any_series + 10 s` (70 s at a 60 s TTL) the
service refuses requests with `RateLimited`. It then reads the maker's window from chain and
quotes above every nonce used.

```sh
kill <PID>                                   # never pkill by name
target/release/quotebook-maker $QB --series <SERIES> --maker <MAKER_KEYPAIR> \
  --maker-base <ACCOUNT> --maker-quote <ACCOUNT> --listen 127.0.0.1:9000 --mid <PRICE>
```

**Check:**
- the log shows `resumed; quoting after the cold start … cold_start_until=<unix>`, then
  `cold start over; quoting`;
- a `quotebook-taker quote` before that time fails with `RateLimited`.

**Pricing is relative to the mid** (I12):
- `--model-units relative` (the default when no model flag is given): the model runs on the
  mid normalized to 100, and its prices are scaled back.
- Passing `--risk-aversion`, `--volatility` or `--order-intensity` switches the default to
  `--model-units absolute`, their pre-I12 meaning in the asset's own price units. The bps
  bounds still apply.
- `--min-half-spread-bps` / `--max-half-spread-bps` (default 5 / 100) bound the half-spread,
  and `--max-skew-bps` (default 100) bounds the inventory skew.
- The deviation guard (`--max-deviation`, default 5%) checks the **rounded** prices.

So the defaults fit any price level: about 0.65% each side.

A configuration that cannot respect the guard fails at startup: it needs
`(max-half-spread + max-skew) / 10000 <= max-deviation / 2`. A mid too small for the tick is
refused per request, with `NonPositiveBid`, `Crossed` or `TickTooCoarse`. Before I12 the model
was absolute: at the I9 mid of 1.20 its defaults gave a 54% deviation (`devnet.md`, I9).

## 4. RPC key rotation

- **Where URLs come from:** `--url` arguments only, set from the environment in scripts. No
  file in the repository holds one.
- **Rotation:**
  1. Issue the new key at the provider.
  2. Restart each service with the new `--url`, by PID, as in runbook 3; the cold start applies.
  3. Revoke the old key at the provider.
- **Check:** the service logs show no URL. The CLI redacts URLs in errors and in `Debug`
  (`cli/src/error.rs`), so a leaked log line does not carry the key.

## 5. Program upgrade, and handing over the admin

- **Upgrade:** the procedure in [`governance.md`](governance.md#upgrade-procedure):
  1. build with `make verifiable`;
  2. write the buffer and check its hash with `solana-verify get-buffer-hash`;
  3. set the buffer authority, then upgrade;
  4. record the result and run a smoke fill.
- **Admin handover:** `$A set-admin $QB --admin $ADMIN --payer $PAYER --new-admin <ADDRESS>`.
  Check with `$A show $QB`, then confirm that the old key's `pause` fails with `NotAuthority`
  (126). Only the current admin can sign it (governance.md, emergencies).

## 6. Devnet funding

- **SOL:** `solana airdrop` is rate-limited. The payer `77ie…CPPV` funds makers and takers by
  `solana transfer`.
- **Circle devnet USDC** (`4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU`): Circle's web faucet gives small amounts
  per request (20 USDC to metalbook's maker on 2026-10-30). For quotebook's demo the taker was funded
  by metalbook's maker (I9).
- **Test mints** (QBB/QBQ, series 0): the payer is their mint authority:
  `spl-token mint <MINT> <AMOUNT> <ACCOUNT> --url devnet`.
