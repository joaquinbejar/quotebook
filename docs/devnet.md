# quotebook on devnet: runbook

Milestone 7 of [PLAN.md](PLAN.md). **Deployed on 2026-10-30**; the addresses and signatures
are in [Deployment record](#deployment-record). This runbook is how it was done, with the
`quotebook-cli` tools (`quotebook-admin`, `quotebook-maker`,
`quotebook-taker`; `cargo build --release -p quotebook-cli`). Keypair options take file paths;
the tools print public keys and transaction signatures only. Keep keypair files out of the
repository (`.gitignore` ignores `keys/` and `*keypair*.json`).

The examples use `QB=(--url https://api.devnet.solana.com --program-id <PROGRAM_ID>)`, an
array, so it expands to separate arguments in both zsh and bash (a plain string does not split
in zsh). In bash, write `"${QB[@]}"` where the examples write `$QB`.

## What is needed from Joaquin

| Item | Why | Notes |
| --- | --- | --- |
| Upgrade-authority keypair | It is the only key that can run `init_config` (the program reads it from its `ProgramData`), and it becomes the first admin | Keep it offline after initialization; `set_config` can later move the admin role to another key without touching the upgrade authority |
| Program-id keypair | Its public key is the program id. Every quote signs it (`Quote.program_id`), so it must not change after makers start quoting | The program declares no id of its own; any keypair works |
| Payer with devnet SOL | Deploying a 35 KB program costs about twice its rent (buffer + `ProgramData`) plus fees; then config, series and maker accounts | About 2 SOL is ample. `solana rent 36000 --url devnet` gives the exact figure |
| Mint choice | `create_series` needs a base and a quote mint | Either existing devnet mints (for example Circle's devnet USDC as quote) or two test mints created with `spl-token create-token --decimals 6`. Any Token-2022 mint must pass the allowlist (README, "Token-2022") |
| Series parameters | `base_lot_size`, `quote_lot_size` (atoms), `max_quote_ttl` (seconds, 60 proposed), `min_size` (base lots) | Lot sizes fix the price grid: one tick is one quote lot per base lot |
| Maker and taker keypairs | Demo participants | Separate from the authority |

## Before deploying

1. **Tools.** `quotebook-cli` (on `solana-rpc-client` 4.3) builds the three binaries. Their
   logic is tested end to end against the program in LiteSVM through the `Rpc` trait
   (`cli/tests/e2e.rs`), never against a network.
2. **Build and check.** On the commit to deploy: `make pre-push`, which builds
   `target/deploy/quotebook_program.so` with the stack-frame check, then `make bench`. Record
   the commit and the `.so` hash (`shasum -a 256 target/deploy/quotebook_program.so`).
3. **CLIs.** Agave `solana --version` (4.4.0 was used for the builds here) with
   `solana config set --url devnet`, and the SPL token CLI for mints and approvals
   (`cargo install spl-token-cli`; not used anywhere else in this repository).

## 1. Deploy with the upgradeable loader

`init_config` reads the upgrade authority from the program's `ProgramData`, which exists only
under the upgradeable loader (`BPFLoaderUpgradeab1e11111111111111111111111`). Do not deploy with
loader-v4 (`solana program-v4`): the gate would reject every `init_config`.

```sh
solana program deploy target/deploy/quotebook_program.so \
  --url devnet \
  --keypair payer.json \
  --program-id program-id.json \
  --upgrade-authority upgrade-authority.json
solana program show <PROGRAM_ID> --url devnet
```

Check that `program show` reports the upgradeable loader as owner, a `ProgramData Address`, and
`Authority` equal to the upgrade authority's public key.

## 2. Initialize the config (the init gate)

Run it right after the deploy. Nobody else can run it, because the gate requires the upgrade
authority's signature, but nothing works until it is done.

1. Deployment id: the first 16 bytes of the cluster's genesis hash.
   `solana genesis-hash --url devnet` prints it in base58; the SDK's
   `deployment_id_from_genesis_hash` takes the decoded 32 bytes. Quotes signed for devnet then
   fail on any other cluster (`WrongSeries`), even with the same addresses.
2. `quotebook-admin init-config $QB --admin upgrade-authority.json --payer payer.json` reads
   the genesis hash over RPC and prints the deployment id it stored.
3. Check: `quotebook-admin show $QB` prints the admin and the deployment id; a second
   `init-config` fails with `AccountAlreadyInitialized`.

Do not drop the upgrade authority before `init_config`: a program without one can never be
initialized (`NotAuthority`).

## 3. Create the series

1. Pick the mints and parameters (table above). Index 0 for the first series: the PDA is
   `["series", base_mint, quote_mint, series_index_le]`, and the index must equal the config's
   `series_count`.
2. `quotebook-admin create-series $QB --admin upgrade-authority.json --payer payer.json
   --base <BASE_MINT> --quote <QUOTE_MINT> --base-lot <atoms> --quote-lot <atoms> --max-ttl 60
   --min-size 1` prints the series address.
3. Check: `quotebook-admin show-series $QB --series <SERIES>` prints the mints, lot sizes, TTL
   and minimum; Token-2022 mints outside the allowlist fail with `MintExtensionUnsupported`.

## 4. Run a maker

1. Fund the maker with SOL, and with base and quote tokens in its token accounts.
2. `quotebook-admin init-maker $QB --maker maker.json --payer payer.json`.
3. Approve the maker's delegate PDA `["delegate", maker]` on each account it pays from, with
   the allowance it is willing to trade: `quotebook-admin approve $QB --maker maker.json
   --payer payer.json --account <MAKER_TOKEN_ACCOUNT> --amount <ATOMS>` (the token program is
   read from the account). `spl-token approve` with the delegate PDA does the same.
4. Start the service with a fixed mid (Pyth Hermes is not in v1):
   `quotebook-maker $QB --series <SERIES> --maker maker.json --maker-base <ACCOUNT>
   --maker-quote <ACCOUNT> --listen 0.0.0.0:9000 --mid 25 --ttl 30` (risk parameters have
   defaults; `--max-ttl-any-series` must be the longest TTL this key signs on any series and
   defaults to this series'). It **cold-starts for `max_ttl_any_series + 10 s`** (70 s at
   60 s): a background thread keeps its clock on the cluster's and, once the cold start is
   over, reads the maker's window from chain and refreshes the issuer. That is the restart
   rule: every restart pays it.
5. Check: a request before the cold start ends is refused with `rate_limited`; after it, the
   service answers with a quote whose nonce is above everything used on chain.

## 5. Run the taker

1. Fund the taker with SOL and the token it pays in, with token accounts for both mints.
2. `quotebook-taker quote $QB --maker ws://<host>:9000 --series <SERIES> --side buy --size
   <lots> --taker taker.json` requests the quote, checks it against the series and the
   cluster clock (signature and every rule `settle` applies) and prints its terms.
   `quotebook-taker settle ...` with `--taker-base <ACCOUNT> --taker-quote <ACCOUNT>` also
   sends `[ed25519 precompile, settle]` and prints the transaction signature.
   With a maker listed in the registry (`quotebook-admin set-profile`, ADR 0002),
   `--discover` replaces `--maker`: the taker asks every listed maker and settles the best
   checked quote, each bound to the maker of its profile; `--allow <MAKER>,…` narrows it.
3. Check: the maker's `MakerState.fills` goes up by one, the nonce is used (a second settle
   fails with `NonceUsed`), and the balances moved by `fill × base_lot_size` and
   `price × fill × quote_lot_size`.

## 6. After the demo

- `quotebook-admin cancel-all $QB --maker maker.json --payer payer.json --new-min <NONCE>`
  above every issued nonce withdraws the maker's outstanding quotes (`cancel-nonce` for one).
- `quotebook-admin pause $QB --admin ... --payer ... --series <SERIES>` stops fills on the
  series without touching makers (`resume` undoes it).
- Keep the upgrade authority until the program is final; upgrading keeps every account and
  the program id, so outstanding quotes stay valid across an upgrade.
- Record the program id, `ProgramData` address, deployed commit, `.so` hash, config and series
  addresses in this file.

## Deployment record

Deployed on 2026-10-30 from the Mac Studio with Agave `solana-cli` 4.4.0 and
`spl-token-cli` 5.7.1. Every signature can be looked up on an explorer with `?cluster=devnet`.

### Program

| Item | Value |
| --- | --- |
| Program id | `3Aq1AwFiK9ibNBLxvYQ18KLT9WbLwENQa3ZZViDW6mbo` |
| Loader | `BPFLoaderUpgradeab1e11111111111111111111111` |
| `ProgramData` | `FvGuwVkTTgTuz1aBfxkxgTfnrrazktc4oXDmyohymqfN` (35,032 bytes, slot 516765399) |
| Upgrade authority, admin and payer | `77ieYLKspRnGzAt1AK6ZVsttnTY6T7EY2k7CpTZeCPPV` until 2026-11-02 |
| Upgrade authority since 2026-11-02 (I3) | `248gnwHHesqhQURgrkfpqfj48NK1oN1fh5qcUnTRssfy` (`~/.config/solana/quotebook-devnet/upgrade-authority.json`) |
| Config admin since 2026-11-02 (I3) | `GZhm3AUm4YxjiK9uurq8R5TDYGMekuik81y9JPiotVnL` (`~/.config/solana/quotebook-devnet/admin.json`) |
| Payer | `77ie…CPPV`, no authority. See [`governance.md`](governance.md) |
| Deployed commit | `898c173d1916404f8579514a4b034aeff129440b` (first deployment, macOS build) |
| `.so` SHA-256 | `d7c2f1de7114751cb05849a981924956bb0e632ae5f849cc9fd62caa7fc721e7` (first deployment) |
| Current binary | the reproducible build of `92cdd22` (same program code), SHA-256 `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776`, upgraded 2026-11-02 in slot 517802812, signature `2NDo6wEyNUeVGcxxJ7igHEGpoVmK4HXunov8QjJ8Mu9XvwuED3zG7Xj622JpxydQUwcqWXdahR8TyWjtUrXx2VJg`. How to check it: [`verifiable-build.md`](verifiable-build.md) |
| `Config` | `D7Y4JfDJNMuwJwMEZP848s8x8MhoTRyZLArEmnum3n71` |
| Deployment id | `ce59db5080fc2c6d3bcf7ca90712d3c2`: the first 16 bytes of devnet's genesis hash `EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG` |

The program-id, maker and taker keypairs were generated on the Studio with `solana-keygen` and
are kept outside the repository.

### Series 0

Both mints are test mints created for the demo (SPL Token, 6 decimals, mint authority
`77ie…CPPV`). Circle's devnet USDC was not used: its faucet is the only way to get it, and test
mints let the demo fund every participant from the payer.

| Item | Value |
| --- | --- |
| Series | `6XjcDfJ2bUyuoCTZYrwLcJihMAm5eoPuJxN9GNe9pGh7` |
| Base mint (QBB) | `EGGsXxG4WEeVEMC3z3jveuF5q8VynofPkBH9wNaxeaSe` |
| Quote mint (QBQ) | `3FpYVrdrw9VruShcj8Ma3E9PAPtGwZH2FKMcMABaARs6` |
| `base_lot_size` | 1,000,000 atoms (1 QBB) |
| `quote_lot_size` | 10,000 atoms (0.01 QBQ): one tick is 0.01 QBQ per QBB |
| `max_quote_ttl` | 60 s |
| `min_size` | 1 lot |

### Participants

| Role | Wallet | Base account | Quote account |
| --- | --- | --- | --- |
| Maker | `5btdGC2DapeXNXqY3ocNJQkoYGyp8LdJprtKspWcmWW5` | `HNHGenxTcd9EoqXcWm7yKBUxTzLuBfE431dcTVTiaaFo` | `EmYy5f5XxSmcZcQ6XtnJmNjZMnwVGGTLx3ysYSXRhyGt` |
| Taker | `3ZBj1qBpdoH5NrMTwAHq2fY6Hbo5M1GURxaP9y8bsqLs` | `FtJUjSZHxmDxG2EwVZXzBs8kqRxrJaf9g7CXLLpMpf43` | `9TetQZmmrhpBUVfF6uxuwrGLrUfKFvTAmu6UFEhTmppS` |

Maker accounts: `MakerState` `5jjt2ZysNyLnmeUxzV4o6mTNrqaRnrCdZoHdJBxEWWgx`, delegate PDA
`2LWv7A5zvmmzC4PM6Ah3wvAbq2nbjyKZ1nRCNz2YFyxf`. Each wallet got 2 SOL from the payer. The
maker was minted 1,000 QBB and 100,000 QBQ, and the taker 100 QBB and 10,000 QBQ.

### Transactions

| Step | Signature |
| --- | --- |
| Deploy | `4vDaEFqPi4Rj4XzwE8HwWJToKwWpnpghZb4tGdr14DET9TgzK9X2w256ZitQdm6DZZT3xaGiTsAncsAQTkye71gq` |
| `init_config` | `2huuko43WVECGZnZk8oe4VyYFKYfom4XXf1Nd83mNPUuXnkshv916nxYtLuF2TKRLDgiSwGwUUVbSd3j5YV6V81Q` |
| Create QBB | `3D9wCSu3XYoSq5pwVL99WuaT4EdcM49wJmrNeVrni5mHdJ8U3Y56tY7EhtZnxFr21g7UzNqugcknHSHD7yNhJkMd` |
| Create QBQ | `4dWXvRRRdPTPzxTtN3skfdcLuVRwZEDmnD7jRQDymaLUudMjDWY7VZDzdD8jB7Gp93BoiFJEYvShbuHza3Pz4yFo` |
| Fund maker (2 SOL) | `4YgyVm515DKrtTxdov94wYozo16yCkyG8ZFjDsLk6uEUhxz7geKGK4ZEDrKpPgkZTDuzNuJDu9BP7g2hibkbwBKP` |
| Fund taker (2 SOL) | `ufhkNV6oCvsm7MvRt6p45x2kLSdtoC8pHBo9aUM9HkTDZtPKCs8NZ1FVMQGK68tWhRkg5WivBaqGDBUsmWVM6i4` |
| Maker QBB account | `3MZgZvQ7zhM7kmwEgRVKJdqQXS53agJKmB5iP5dSoC8M8NuQpPunjcUG3rXtJVYZdiFBMjtVCWsPJrQvx3YsKZUU` |
| Maker QBQ account | `G5HeQXWVySFZsMdhE1fXdKz7yr47mRNyWesn3e9LR2SYLhdBbYpD1JrBBYZVp94GPbCGAhrZ6pGkG74DCneZPpS` |
| Taker QBB account | `jjM6QBeRgP9jNKdQfxq2ShuCLCRz1VFDWKPLSkRUxutMkgX6ZNEUTrR4RVMy1eTeGvAAdzBGXNLPAhcW8xzj1QJ` |
| Taker QBQ account | `2BgFgcW8z3XvW2kjHKv8civTPwzSgTmVM1pfKUcnvwtBkjLFDAFUp8NPopqW28pGZkCwT5Xm6PGR2QMoNpKa4V5c` |
| Mint 1,000 QBB to the maker | `2VB7LPEK3tDYihuHy6g1Fqa72Hc1GcXFNxvfjuoydiFpLnkHsDUFkXs9GQRR7D9mv647NAk1D2CW11HJtFnfR84U` |
| Mint 100,000 QBQ to the maker | `25SwUVSm6sWudvxnDkHiLSCdRrhwvMd39Nmyu4tVYCrA1r3vf5H2LS4y6J8BEF249mfhuJirbB5bhZfcNaqeeNFP` |
| Mint 100 QBB to the taker | `3t2LVEaArWmeNnbVc4Bd6rSoQR6nWnhhsy1MesuyYnWCKPiJmxKsLWaNc9UKPcPLQBaMnxKqvBUwSiNuRgwr9U79` |
| Mint 10,000 QBQ to the taker | `3wACRznNfNE1b6fupNxFcJCPiXHiftHpNgQhtcnAXQShuFVrZgSsTk3R2D2Wu7KdvRS5soNZEauAWrYg9JJosuUC` |
| `create_series` | `5yXZJ5RwftJGUGyN3R8DP6nKj4jHx2DjmUmAFCYpobf1N8Wrxnoiimas9WZn7F8pyb4kUkYAMviUxfPcqkJzPpvE` |
| `init_maker` | `2vdFxF2ruPqce29WP3nP6UMm1Z79p2wn1xFW2a3mHhD399hby1JAkrMmYEqaWS7MXT539MRJvVZchw9MLvV3tGM7` |
| Approve delegate, QBB (500 QBB) | `oUuCw9QN1FUnZ9HXQzXQ3BoHSrQUi7RmA5A6VLSN9jSnKedgcbetg1B8xoFErKihtWf4studBMhNvb8CJ6YvFgX` |
| Approve delegate, QBQ (50,000 QBQ) | `3mg4sxVfsarewKctmqcA9dShj8ePXdLSmUuf3fyCiMUtR6WzRmgvF1gEhaFEUwZDRYQ2qzeAt9nrQi6GSShpYc3W` |
| `settle`: taker buys 2 lots at 2,565 ticks | `5Pz59PQU4AGERy149Z4MYFiS2spssSxHq9w7gKUVyoNGrQ1H4HZENT3t4qhcZ56qzCE1tn8S4HmEL3DdY5xAQvM6` |
| `cancel_all`, `new_min` 2 | `61oRAy6waCXcAhBCMf5o4sqmf1dNMDvJRnsnMdgXDijyFndmEc715TeZ1dhu3bdFVNhSV4rcz5zEdSV6Rrxv33R9` |

### Checks

- `program show` reported the upgradeable loader as owner and `77ie…CPPV` as authority.
- `quotebook-admin show` printed the admin and the deployment id above, which equals the
  genesis-hash prefix. A second `init-config` failed with "instruction requires an
  uninitialized account" (`AccountAlreadyInitialized`).
- `show-series` printed the parameters in the table above.
- The maker service (`--mid 25 --ttl 30`) refused a request during its 70 s cold start with
  `RateLimited`. Once the cold start was over it quoted an ask at 2,565 ticks (25.65 QBQ per
  QBB) for 2 lots: nonce 0 for `quote` and nonce 1 for `settle`.
- After `settle`, the maker went from 1,000 to 998 QBB and from 100,000 to 100,051.3 QBQ. The
  taker went from 100 to 102 QBB and from 10,000 to 9,948.7 QBQ. That is `2 × 1,000,000` base
  atoms and `2,565 × 2 × 10,000 = 51,300,000` quote atoms. `settle` used 8,595 CU, which
  matches the 8,593 measured in LiteSVM.
- `MakerState` after `settle` showed `fills` = 1, nonce 1 used and nonce 0 unused (it was only
  quoted). The final `cancel_all` raised the watermark to 2, which voided the open nonce 0.
- The maker service was stopped after the demo.

## RFQ demo on a metalbook series (milestone I9, 2026-11-02)

A quotebook series whose base token is a metalbook option. The maker sells a long token of a
live metalbook series for Circle devnet USDC over the RFQ, with no new code: only
`quotebook-admin`, `quotebook-maker` and `quotebook-taker` from `main` (`ae0903b`). Plan:
[`PLAN-improvements.md`](PLAN-improvements.md), I9. metalbook's side is in its
[devnet log](https://github.com/joaquinbejar/metalbook/blob/main/docs/devnet-log.md).

### Accounts

| Item | Value |
| --- | --- |
| metalbook series | XAG capped call, 1 oz, strike 50, cap 55, expiry 1794603600 (Fri 2026-11-13 21:00Z): `B66MftMja6L6g2XFNLWHUWE2Rjq1yTHtToWQTvxsZx21` |
| Base: metalbook long mint | `AF3A2Rt57QPYXnXg42nBNTN2TLP8GoMjp94ZVYVyjH4x` (SPL Token, 0 decimals) |
| Quote: Circle devnet USDC | `4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU` (6 decimals) |
| quotebook series 1 | `AbfhiX44ebCSpLEB8ba1gw5vL8qJr7yb7RxYk3CwSCAf`: `base_lot` 1, `quote_lot` 10,000 (0.01 USDC per tick), `max_quote_ttl` 60, `min_size` 1 |
| Maker | metalbook's devnet maker `7tVPWX3zmxYCZ5ioTSX4yzdNq8o3Xr1hMSv2kz12cACQ`, signing with metalbook's agreement for these steps only; it wrote 3 contracts of the series in metalbook |
| Maker long-token account | `FMKDwm14TMcvfDGTjt3umPk5wJBgFaCxy1EhiwruFb13` |
| Maker USDC account | `CTMwGLgvpCBY3oZWaWUrWCjMfkn5VgeMGc5LbhjFL95Q` |
| Maker `MakerState` | `6f4Xq5D1SxSNAZStgf11X2d7LiMsKCQG7fpepnxSR5wv` |
| Maker delegate PDA | `6HkDimtJ9ZzNCrnsHkEmPiPoDzeA5SqRepwwiwvZoZhL` |
| Taker | `3ZBj1qBpdoH5NrMTwAHq2fY6Hbo5M1GURxaP9y8bsqLs` (10 USDC from metalbook) |
| Taker long-token account | `EMuqqB2cEeYbg8ZU7QmXqhnycCHLeaFB5uXQ1HUya199` |
| Taker USDC account | `8kicXLNNrXtx2EYngwqYCq8HmWZHtThsLvwmTAqoLmnT` |

### Transactions

| Step | Signature |
| --- | --- |
| `create_series` (series 1) | `5d8MmCFuRgPfVszVAnMJZb6KrjP7wjcTXcGPYbtXCcCARbpdPpmJsEgCffCTYTjr2LZUTPvyH8uXLQp1DnMbdcWH` |
| Taker long-token account | `3xRUGgit84ESgBpktN1rsb2kvvQQ6r4r9EoFyppegVs2vwpShDuZXo8QXvUuTmEMnaNm17AktFjSRq3X1jFZhmnc` |
| `init_maker` (7tVP…) | `3UJaZ57vaRgXEPACVZCswRGsyVHJpQRL6UABdjKacTwtHLjXrxdikFm98RnksoahzrdQR8AYXUBZdSAdJeB6AzF` |
| Approve delegate, 3 long tokens | `3mWuWv7rzaL3bbrpTDBZQNyHJtUjFcXV76WLbboKJuJnNzwgQDfceRV1RBgyS8kkYkHQRtwTqWqkxWfYgq8mgcdx` |
| `settle`: taker buys 1 long token at 123 ticks (1.23 USDC) | `4tNTwmG1t51A2uyhUwoVELv1R7qq2LsrwbdNbRMeWaZ2kXRXPUhRy2DbM3VGtPkwQvMdx4797RBgyAcYhfxTRB2c` |
| Revoke the delegate approval | `66QJkHYLrSpJSu4CuttdNpQ4bj3452fj6iqZnMMMh7jTDVsAkW13wBRTobhNS9fWgcLWQs9HMfBVGvCNMzeBFFSw` |

### Pricing

The mid came from metalbook-maker's `mark` (the capped call as C(50) − C(55), 1 oz), with XAG
spot 50.29345 from Pyth, rate 4% and carry 0. With no implied volatility source, it gave
1.0568, 1.1959 and 1.3142 USDC at 25%, 30% and 35% volatility. The demo quoted around a
**1.20 USDC** mid.

The maker's risk defaults (`--order-intensity 1.5`, `--risk-aversion 0.1`) are tuned for
prices around 25. Avellaneda-Stoikov's spread term `(2/γ) ln(1 + γ/k)` is in absolute price
units, so at a 1.20 mid they gave a 54% deviation, which the maker's own 5% guard rejected
(`rejected (Pricing): price deviates 0.5378… above the 0.05 tolerance`). The service was
stopped by PID and restarted with `--volatility 0.3 --order-intensity 40`: the spread term
becomes `20 × ln(1.0025) ≈ 0.05` USDC, and the ask came out at 1.23 USDC (rounded toward
the maker).

### Checks

- The restarted service refused a request during its cold start with `RateLimited` (`no
  quotes before 1793608717`). After the cold start it quoted an ask at 123 ticks for 1 lot:
  nonce 0 for `quote` and nonce 1 for `settle`.
- After `settle`, the maker went from 3 to 2 long tokens and from 15 to 16.23 USDC. The taker
  went from 0 to 1 long token and from 10 to 8.77 USDC. That is `1 × 1` base atom and
  `123 × 1 × 10,000 = 1,230,000` quote atoms. `settle` used 8,595 CU, as on series 0.
- `MakerState` after `settle`: `fills` = 1, `window_base` 0, and the first word of the ring
  `0b10` (nonce 1 used, nonce 0 only quoted). Nonce 0 expired 30 s after it was quoted.
- The maker service was stopped by PID after the fill, and the delegate approval on
  `FMKD…Fb13` was revoked (`spl-token display` shows `Delegation: (not set)`), so no further
  fill can move the maker's long tokens.
- Observed in the demo: the fill only. Expected afterwards, not observed here: the taker's
  long token is a live metalbook position. After expiry (2026-11-13 21:00Z) metalbook's
  keeper is expected to settle the series, and the token can then be redeemed through
  metalbook. That lifecycle is metalbook's and is recorded in its devnet log, not here.
