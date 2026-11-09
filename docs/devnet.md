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
| `ProgramData` | `FvGuwVkTTgTuz1aBfxkxgTfnrrazktc4oXDmyohymqfN` (35,032 bytes, slot 516765399; 52,472 bytes since the I10 upgrade) |
| Upgrade authority, admin and payer | `77ieYLKspRnGzAt1AK6ZVsttnTY6T7EY2k7CpTZeCPPV` until 2026-11-02 |
| Upgrade authority since 2026-11-02 (I3) | `248gnwHHesqhQURgrkfpqfj48NK1oN1fh5qcUnTRssfy` (`~/.config/solana/quotebook-devnet/upgrade-authority.json`) |
| Config admin since 2026-11-02 (I3) | `GZhm3AUm4YxjiK9uurq8R5TDYGMekuik81y9JPiotVnL` (`~/.config/solana/quotebook-devnet/admin.json`) |
| Payer | `77ie…CPPV`, no authority. See [`governance.md`](governance.md) |
| Deployed commit | `898c173d1916404f8579514a4b034aeff129440b` (first deployment, macOS build) |
| `.so` SHA-256 | `d7c2f1de7114751cb05849a981924956bb0e632ae5f849cc9fd62caa7fc721e7` (first deployment) |
| Current binary | I10, the reproducible build of `3d0ec13`, SHA-256 `4748d02454e5045ec06f8dec1ea2e5575ce28c7f4ab56b64dda46c1a9626c87a`, executable hash `a5287763e1c93463d7f52b5850129ee5be870328e489d513c47d693182d47f4d`, upgraded 2026-11-03 in slot 518208830, signature `5y8scCjTzJNiPQ8SK77gUQZgXXnpDYQqwQ9DeY5mxQPst89VKYPpyR5UtXEbGG6Au4c8u5JwFukuAcnN7s2SFnuC` ([below](#i10-upgrade-and-smoke-tests-2026-11-03)). How to check it: [`verifiable-build.md`](verifiable-build.md) |
| Previous binary | the reproducible build of `92cdd22`, SHA-256 `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776`, from 2026-11-02 (slot 517802812, signature `2NDo6wEyNUeVGcxxJ7igHEGpoVmK4HXunov8QjJ8Mu9XvwuED3zG7Xj622JpxydQUwcqWXdahR8TyWjtUrXx2VJg`) to the I10 upgrade |
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

## I10 upgrade and smoke tests (2026-11-03)

The program was upgraded to I10 (series fees, ADR 0001; maker registry, ADR 0002) from
`3d0ec13`, the merge of #34, with no migration: series 0 and 1 keep `fee_bps` 0 in bytes that
were zero. Upgrade record: [`verifiable-build.md`](verifiable-build.md). The smoke tests used
`quotebook-admin`, `quotebook-maker` and `quotebook-taker` built from `3d0ec13`, the payer
`77ie…CPPV` paying, the admin `GZhm…tVnL` signing the series and the withdrawal, and the
series 0 maker and taker above.

### Accounts

| Item | Value |
| --- | --- |
| Series 2 (QBB/QBQ, 10 bps fee) | `8HJpxdrb9tNgW9BVYtytPNPfBGhQsauxwLwoqfwttu9Z`: the lot sizes, TTL and minimum size of series 0, `fee_bps` 10 |
| QBQ fee account (the Config PDA's ATA) | `HhHwyMfVNtfAnQHFRdK15dKVCdpeVULpR6STSMrUpczq` |
| Payer QBQ account (withdrawal destination) | `46QUHZBWkCGpgx1qQ6WeLHgmx3dD9LNmiz2EYcgGwb3t` |
| Maker profile PDA `["profile", 5btd…mWW5]` | `4C4HxZx6ESBNXJyCWzj4dZCKeec18XCaBDHw5DXrWZak` (closed at the end) |

### Transactions

| Step | Slot | CU | Signature |
| --- | --- | --- | --- |
| `settle`, series 0: taker buys 1 lot at 2,517 ticks (v1, 13 accounts) | 518209966 | 9,207 | `2fT3waw2dAGqGp1ctMR7tELkFXFtgr4BAyUYjxSwiBrjUBoAaCGAUNeu7yKce3ihkL5g1eGADiuF4MHZJPhthdix` |
| `create_series` (series 2, `--fee-bps 10`) | 518209568 | 9,413 | `EMBpCEkAMtyzQt2ESdG4LJF8JJLy4aJjWLNzatXmDxHmCci5mx5McFqLpVPxTr3bNy9PfL61aJHXEATANR2zYTD` |
| `create-fee-account --quote QBQ` | 518209574 | 15,018 | `2tbREi9nqikHWouDeUVbzcC4p7phQcyVDvmHnC9tjGAG5kCvYSM1LuXw4GEXtsERTZHSkgAnR48qx32MSzmWSoRc` |
| `settle`, series 2: taker buys 1 lot at 2,517 ticks with the fee | 518210459 | 15,199 | `5bnjMecD2bGgBW6UDLjc3gUeTz8y7B8AxcWKL6qKgmRBte7BzZqyFQm2chwHhzjHYp3EHmzYH3yQbANR1KQBeB2Y` |
| Payer QBQ account | 518210759 | | `4CyaYFzhqMND2CjqVQU3prmpgW7Hf6rTM8HRoGt8MKMFRYs76V1gHvowgFzrJNemuHKZBBPbGLRq9DBVZFAwpVw9` |
| `withdraw_fees`, 25,170 atoms | 518210787 | 3,125 | `61F6CiPfRHDjdBpr6hsRzVxxndHniTmRkMu8vJRe6Wxgam12fg9M7FxvnWidQtvgtbG1UWz9kLmYiXxh4cwEjNju` |
| `set_profile` (create) | 518210898 | 10,875 | `4rKLyBZ3eGiWDWEDsfCXjDoqHRqqwprMk3MToPqjhZTytt16ckfH9d3YvgLjCaVca2gAMCP5zAbCeGy885D8abCr` |
| `close_profile` | 518210995 | 1,700 | `4DkSRrXXQz4kmeCMhCBEbFdiwy22rTPJ2m9gZtJFWtbHsiyU3faRe1qymTPXtYcqndSyXgFcjryeUGYUCVpaKcwR` |

### Checks

- `show-series` on series 0 printed `fee_bps 0` after the upgrade, and on series 2 `fee_bps 10`.
- v1 fill on series 0 (maker ask, nonce 3): the maker went from 997 to 996 QBB and from
  100,076.95 to 100,102.12 QBQ; the taker from 103 to 104 QBB and from 9,923.05 to 9,897.88
  QBQ. That is `1 × 1,000,000` base atoms and `2,517 × 1 × 10,000 = 25,170,000` quote atoms.
  9,207 CU against 9,204 to 9,206 in LiteSVM (`benchmarks.md`).
- Fee fill on series 2 (maker ask, nonce 4): the fee is `floor(25,170,000 × 10 / 10,000) =
  25,170` atoms, taker-paid. The taker paid 25.19517 QBQ (9,897.88 to 9,872.68483), the
  maker received 25.17 (to 100,127.29), and the fee account went from 0 to 0.02517 QBQ. The
  `qb-fill1` event decoded to fill 1, price 2,517, base 1,000,000 atoms, quote 25,170,000
  atoms and `fee` 25,170.
- That fill used 15,199 CU against 12,207 in LiteSVM (100 bps, SPL, maker ask). The
  difference, 2,992 CU, is the bump search of the Config PDA, which `settle` derives with
  `find_program_address` on the fee path: on devnet its bump is 253 (three attempts at about
  1,500 CU each), while the LiteSVM program id gives 255 (one attempt). Every fee fill on this
  deployment pays it; storing or passing the bump would remove it (follow-up, not a fault).
- `withdraw-fees` of 25,170 atoms left the fee account at 0 and the payer's QBQ account at
  0.02517. 3,125 CU, the LiteSVM figure.
- `set-profile` with a placeholder endpoint (`wss://maker.invalid:9000`, name `quotebook
  devnet smoke`) created a 208-byte account owned by the program holding 0.00170688 SOL;
  `quotebook-taker discover` listed `1 listed, 0 invalid, 0 not allowed` with that endpoint
  and name. A live `--discover` fill needs a reachable endpoint and is covered by LiteSVM and
  the CLI end-to-end test. `close-profile` removed the account, `discover` then listed 0, and
  the rent went back to the payer: its balance moved only by the 20,000 lamports of the two
  transaction fees.
- Both maker services (series 0 on `127.0.0.1:9000`, series 2 on `127.0.0.1:9001`) refused
  requests with `RateLimited` during their cold start, quoted after it, and were stopped by
  PID after their fill.

## Demo maker pricing inputs (D2, 2026-11-05)

`demo/config/devnet.json` holds these; the demo maker that uses them runs from D3 on.

| Input | XAG | XAU | Note |
| --- | --- | --- | --- |
| Vol | 0.30 | 0.18 | One number per underlying, from configuration; there is no vol source. XAG is the I9 demo's; XAU is what metalbook's adapter tests use, and the demo does not trade XAU. |
| Rate | 4% | 4% | |
| Carry | 0 | 0 | |
| Feed account | `H9JxsWwtDZxjSL6m7cdCVsWibj3JBMD9sxqLjadoZnot` | `2uPQGpm8X4ZkxMHxrAW1QuhXcse1AHEgPih6Xp9NuEWW` | Pyth push feeds, read over RPC once a minute. |
| Exponent, read 2026-11-05 | -5 | -3 | Accepted range: `-12..=0`. |

No quote when spot is older than 600 s, when `conf / price` is above 50 bps, when the market is
closed, or from `expiry − window_s` on. These are demo settings, not a statement about price
quality: acceptable only because the service signs for its own taker (`docs/PLAN-phase2.md`, D2).

The published `metalbook-core`, `metalbook-sdk` and `pyth-lite` 0.1.0 decode the live accounts
(`demo/tests/devnet_accounts.rs`). Strip marks equal `metalbook_maker::mark` exactly on 162 inputs
(`demo/tests/marks.rs`), metalbook being on optfix 0.2.1 and `demo/` on 0.3.0.

## Hosted demo maker (D3, 2026-11-06)

`quotebook-demo-maker` under launchd (`scripts/demo-agent.sh install demo`), from
[`PLAN-phase2.md`](PLAN-phase2.md), D3. It quotes the configured metalbook pairs for its own taker
only, on `127.0.0.1:9000`. **Devnet only:** the install script refuses any RPC URL but the public
devnet one. On mainnet a scheduled job never holds an admin key; this agent holds only the demo
maker key's file path.

### Accounts

| Item | Value |
| --- | --- |
| Maker | `142GM9zKDbdncitgiL6zwdKquwuzzNhMPXwEm1RVYaVB` (`~/.config/solana/quotebook-demo/maker.json`) |
| Payer | `EEGQJgeP96RSHZd97cX3p3HVivAGGKU4LSsQzR7TFVCY` (`~/.config/solana/quotebook-demo/payer.json`), 1 SOL from `77ie…CPPV` |
| `MakerState` | `275ia4iQR4UUZgdcAHKkniZZiwAW2FVqcuC4BXNQUL7S` |
| Delegate PDA | `4fixXvjy8Snf8ekYcMebinra4e5XqR5jpPfoBt96kxxM` |
| USDC account (ATA) | `F7KjgYNYkFK1RLF81yZcfRKq1wEb9wCDDSSrVUPe8Co2`, allowance 4 USDC |
| Long-token account (ATA, `AF3A…jH4x`, series 1) | `F1YY6EnJKLQxR3bwjGWMHrYCB2saZqMdTS7Ju4s91GTm`, allowance 2 |
| Registry profile | `wss://localhost:9000`, name `quotebook demo (not reachable)`: a listing only, since the program requires `wss://` and the service speaks plain `ws://` |
| Settings | `demo/config/devnet-maker.json`: TTL 30 s, 1 lot, half-spread 250 to 300 bps, guard 10%, 4 open quotes, 4 fills an hour, snapshot age 60 s, band 0 to 2 long tokens, 6 USDC notional, taker `3ZBj…sqLs` only |

Both token accounts hold nothing until D5's funding, so every quote is refused with `RiskLimit`.

### Transactions

| Step | Signature |
| --- | --- |
| Fund the payer, 1 SOL | `5E4qkHA2B5TqGz9DPbtB9QSEfTBP5pWm3dKUFmT8wTYDLb4Nkyf4HGWJ6PJgzhLcQsTQLYLUHSgXNMqyvtz4GD8` |
| `init_maker` | `5i3Uah2FRi5KdnsBfbH38E91GkRKSe85qAT8kTrJd1HPmanNAz4SWrP2GFKJAqDKRGgsUxQSJ2YaUVZFiRKDY7g6` |
| USDC account | `5B4Hiqa36JqHE3iVvazQEAmoJn2iY2dfLegESoMxfALkMdWm2YCUmKgJxUfhjDsngdumrFMNoSc7vh63qB4xDxCR` |
| Long-token account | `4wNrZrA4CN46HPaeTuCLoyVTHAF1htAFhTvWC7y4EboLeAV8rP1CMudstgjSKjv6RyPxYYuAc4YuWDfJrSUMnwsx` |
| Approve 4 USDC | `4Lj8PgvnZq6rJZrdDkUNhToCib3obVwjaAthda1Z43mWSjhzB1DaP6xBabY8xTAgXCfEuWZ3aR2GU4k7ZLapjeE5` |
| Approve 2 long tokens | `4Sq59rHLqh4LKdBrjf8hgtTd9Qfdt8Gbb2UvssQ1uLKDt4eybgfqLjfJr5HnnRpK82ffoLhXbjGeQ43XTk8fa5bR` |
| `set_profile` | `3c4EMYHrwEZjtJyT132kbyXWmcKkSBw1HeAP8gcBq3En2zxpoaRS67YB1NCj4UrZtJu9jCDsarW56QCBrB6kQ7DG` |
| Revoke drill 1: revoke USDC | `4QSdQzMFaV3m6MrGJzFJxn6bz1ZskQAL4LTJVEkd6MxHmFSENpbZY2w8jk8YfqcrC8GPMVKaK3CBPamHw3d3s5eh` |
| Revoke drill 1: revoke long | `2e1fJT5z7t7cHxzQJgtL9x62ui49yoqsdQJpLPHv9ePbuhCUR3mwM6X5sZEDxB84L3cxidxse8oaVSV8y97JcYQo` |
| Revoke drill 2: `cancel_all`, new minimum 1 | `5Kve2QYbSweNk4GHZrPdXW3LHDECoUKB9aZhCLNnKXNWfuKzZ224mcjDhc5d3ds6UX2UAxYna6Bo1buAUkmyVTpa` |
| Revoke drill 2: revoke USDC | `28UuxuXrnq39uRE8wGii9jHRDt9zEo4gFDfjxGDL9qZ4TR3p9NYe2DZPAx7HzMt7xaP7mcjr9ZbDj3dCQ7cjQ3H6` |
| Revoke drill 2: revoke long | `4x9HfehswRT7xcrqxnySXdhC8CZ1bvBvVPFbRd2dYna53fQw7fX3v9cbiNTLjmwqiuV7N4YAygf4ke7zJgWKxCZX` |
| Approve 4 USDC again | `3tTEcsuicd2QHsthvwipGnSVQwUMzsrWicEgQbKpG3d6iCQeZiwgLRahdMrBpfCFzChzxiAeqftfd4spP4y65wk5` |
| Approve 2 long tokens again | `3k7qHXiKhXjhkpiXGER7J85Q9TQQbQ27ELs44aBwAEsABcnr99yXFqHsoqcHvQtF6uh4hyJmhyt75tJpnDvbHEJK` |

### Checks (2026-11-06, about 19:00 to 19:15Z, market open)

- **Start:** the log's first lines are `resumed; quoting after the cold start on a finalized Clock`
  (`started_at` 1793991874, longest TTL 60) and `serving`. `status` answered with a snapshot 3 to
  6 s behind the service's clock and the XAG spot's age.
- **After the cold start,** the taker `3ZBj…sqLs` was refused for a buy with `RiskLimit: inventory
  band: max_short` (no long token held) and for a sell with `RiskLimit: funds: balance or
  allowance` (no USDC). Those refusals come after the cold start, the fill-history rebuild and the
  pricing in the book's order, so all three passed on live devnet data. Another taker got
  `TakerNotAllowed`.
- **`discover`** lists the profile: `1 listed, 0 invalid, 0 not allowed`.
- **Halt:** `demo-agent.sh halt demo` drained at once (`newest_expires_at` null), and the next request
  got `Halted`. Draining at once after a restart was a bug (Codex on #44): a quote of the previous
  run could still fill. With the fix, on 2026-11-07 the agent was reinstalled at 08:58:55Z and
  halted 8 s later; `newest_expires_at` was 09:00:05Z (the end of the 70 s cold start), and the
  drain waited 66 s, until a finalized Clock of 09:00:08Z, before reporting `drained`. The halt
  file was then removed and the agent left running.
- **Revoke:** drill 1 had no nonce to cancel (`next_nonce` 0 equals the window's base) and revoked
  both allowances; drill 2 forced `cancel_all` with `QUOTEBOOK_DEMO_NEW_MIN=1`. Both were confirmed by
  a finalized snapshot (`window_base` 1, `spl-token display` showing no delegation) and restarted the
  agent halted; after the restart its `next_nonce` was 1.
- **Crash restart:** `kill <PID>` was followed by a launchd restart 119 s after the previous start
  (`ThrottleInterval` 120), with a new cold start.
- **Left running** after the checks: `com.quotebook.demo-maker.demo`, approvals restored, no halt
  file. It quotes nothing until D5 funds it.

## Demo watcher (D4, 2026-11-07)

`quotebook-demo-watch`, from [`PLAN-phase2.md`](PLAN-phase2.md), D4, installed by
`scripts/demo-watch-agent.sh install demo` next to the maker agent (runbook section 8). It holds no
key and sends nothing: every account is an address, and it reads at `finalized`. **Devnet only:** the
install script refuses any RPC URL but the public devnet one.

### What it watches

| Item | Value |
| --- | --- |
| Payer (`balance`, under 0.3 SOL) | `EEGQJgeP96RSHZd97cX3p3HVivAGGKU4LSsQzR7TFVCY` |
| Maker USDC (`inventory` under 1 USDC, `allowance` under 0.8 of 4 USDC) | `F7KjgYNYkFK1RLF81yZcfRKq1wEb9wCDDSSrVUPe8Co2` |
| Maker long tokens, series 1 (`allowance` under 0.4 of 2) | `F1YY6EnJKLQxR3bwjGWMHrYCB2saZqMdTS7Ju4s91GTm` |
| Taker USDC (`inventory` under 1 USDC) | `8kicXLNNrXtx2EYngwqYCq8HmWZHtThsLvwmTAqoLmnT`, the ATA of `3ZBj…sqLs` |
| Writer positions (`writer-cap`, `collateral`, `no-series`) | metalbook `Writer` accounts of `142G…VYaVB`, `getProgramAccounts` at `finalized` filtered by size 80, tag 4 and owner at byte 40 |
| Calendar (`no-series`) | metalbook's XAG calendar PDA, with the config's `window_s` |
| Feeds (`stale-spot`) | `2uPQ…uEWW` (XAU), `H9Jx…Znot` (XAG) |
| Maker (`maker-down`, `risk-unavailable`, `halted`) | `Status` on `ws://127.0.0.1:9000`, and `HALT` in the agent's directory |
| Taker (`fill-error`) | `trades.jsonl` in the agent's directory, written by D5's taker |

**`trades.jsonl`** (D5 writes it): one JSON object per taker run, ending in a newline, with at least
`time` (Unix seconds), `outcome` (`filled`, `skipped` or `failed`) and, for a skip or a failure,
`reason`. The watcher reads only the last complete line; a missing file is silent.

### Checks (2026-11-07, about 11:40Z, Saturday, market closed)

Two passes by hand from the branch, against public devnet and the running maker, without launchd:
- **Reads:** the Clock, payer (983,776,400 lamports), the maker's USDC (14 USDC, allowance 4 USDC)
  and long-token account (0, allowance 2), and the taker's USDC (8.77 USDC) all decoded at
  `finalized`; the owner-filtered `getProgramAccounts` on metalbook answered (0 positions, so 0
  contracts and 0 committed). The derived token accounts are the ones D3 recorded.
- **Maker:** `Status` answered within the timeout (`snapshot_age_secs` 2, not halted) and
  `next_nonce` stayed 1: no nonce taken.
- **Weekend:** both feeds' last updates were Friday 21:59:59Z, about 13.7 hours old, inside the
  weekend closure (22:00Z Friday to 23:00Z Sunday), so each wrote one `expected` line with that
  closure and no alert.
- **Delivery:** a second pass with `--min-balance 2000000000` raised `balance` and ran the alert
  command, which appended one line with the kind, subject and message. A third, at 11:52:14Z with
  `--alerts-log`, made the watcher write that line itself (UTC time, `balance`, the payer and the
  message) to the rotated alert log. No line carried the RPC URL.

The agent is not installed yet; it will be installed after the D4 merge.

## Writer cycle and demo taker (D5, prepared 2026-11-07)

`quotebook-demo-roll` (`scripts/demo-roll.sh`) and `quotebook-demo-taker`
(`scripts/demo-taker-agent.sh`), from [`PLAN-phase2.md`](PLAN-phase2.md), D5 (runbook section 9).
The demo maker `142G…VYaVB` is the writer: it locks its own devnet USDC in metalbook and sells the
long tokens through quotebook, and the I9 taker `3ZBj…sqLs` buys and sells them back. Both refuse
any cluster whose genesis is not devnet's.

**The admin key in a scheduled job is devnet only.** The roll job holds the path of the quotebook
admin keypair `GZhm…tVnL`, which signs only `create_series`, logged to `roll.log`; its plist is
dedicated to the job and holds file paths only. **This is not allowed on mainnet:** a scheduled job
never holds a mainnet admin key.

### Funding

| Item | Value |
| --- | --- |
| Transfer | 14.00 USDC from metalbook's `7tVP…cACQ` to the maker's USDC account `F7KjgYNYkFK1RLF81yZcfRKq1wEb9wCDDSSrVUPe8Co2`, signed by metalbook (2026-11-07, about 09:04Z) |
| Signature | `uQAmw5xnP8Atc4aA29HS3bsF8XXmBemnENznW2gXsvBWCWrAaXYaY6q4EGvQ5r4tWFTmAFdJkGnAkW5zRDZxnvW`, slot 519,608,843 |
| Check | 14 USDC at `finalized` in that account, allowance 4 USDC to the delegate PDA |

### Limits

| Limit | Value | Where |
| --- | --- | --- |
| Contracts written and not claimed, every series and expiry | 2 | `writer_cap`, from the chain at `finalized` |
| Collateral committed | 10.40 USDC | the same |
| Long tokens held per series (the band) | 0 to 2 | `market_limits.max_long` |
| Delegate allowances | 2 long tokens per series, 4 USDC | the approvals the cycle sets |
| Lots per trade | 1 | the taker |
| Fills per hour | 4 | the maker's `max_fills_per_hour` |
| Takers | `3ZBj…sqLs` only | the maker's `allowed_takers` |

### Dry runs (2026-11-07, about 12:02 to 12:08Z, Saturday, market closed)

Read only, from the branch, against public devnet and the running maker, with nothing sent:
- **Roll plan:** finalized slot 519,653,441. Five 1 oz XAG capped calls for Nov 13 21:00Z are
  eligible (strikes 48 to 52, collateral 4.80 to 5.20 USDC each); no spot, since the feed is inside
  the weekend closure, which only matters for an expiry without a pair. The plan keeps the
  configured pair, series 1 (`B66M…Zx21`, strike 50) on `AbfhiX…SCAf`, and has one step: mint 2
  contracts (10.00 USDC locked, 4.00 USDC left to buy back), which needs the halt. The writer's
  positions read 0 contracts and 0 committed; both approvals are already at their amounts.
- **Taker:** the decision is `skip: market closed`. Balances at `finalized`: the taker 8.77 USDC
  and 1 long token of series 1 (from I9), the maker 14 USDC and no long token; the taker holds
  1,999,970,000 lamports for its fees. The maker's `Status` answered (not halted), no quote was
  requested and no line was written.

### First cycle and first trades

All on devnet, 2026-11-08 and 2026-11-09 UTC.

- **First roll, by hand, 21:18Z:** mint 2 contracts of series 1 (`B66M…Zx21`, Nov 13 XAG 50 capped
  call), signature `wBe4SGQ4qvLD698i9gs8pPEA4LCBboZP9LFaq2FoZTEpBvb3aFPmhdcoKiZNa8KY4QaXVVfJh8U1W1nk7BTHGRM`.
  The first attempt failed in simulation because the maker held no SOL for its position rent; after
  funding it with 0.01 SOL (`YvXiaqV4KrxsJH9AfeYSfC6P933xHaBRtTtqxfv4P6sqp23YuqsFdp6SrU7veysVVku5HFAmgrPtNdPHbBpSur6`)
  the run succeeded. The plan now refuses a mint whose rent would strand the maker.
- **First trade, by hand, 23:08:29Z:** the taker bought 1 lot at 1.81 USDC, fee 0, slot 520181333,
  `2zRZP7dEeg3twfydRBm7sVP8MCGQrfrcKfyPtSMJfi89B8K2WBxYNfJx7ftAByqvEWE9FNYgCY9B7D7753qLb6dp`. A buy,
  because the roll had left the maker at its band top of 2 long tokens. Taker 6.96 USDC and 2 long;
  maker 5.81 USDC and 1 long; the watcher pass at 23:08:34Z agreed.
- **First unattended roll, 05:10Z:** `create_series` for the Nov 20 XAG 53 capped call, signed by the
  devnet admin, `2UTupS9QpEU5ZRQvDtXJ97kiRwgSr5Zu27tHtb9cmasDqGW8vjDf1BJECLzMvdmuqEjhookeJQfFY6HyQBwrmoJA`.
  No halt and no mint (the writer cap was full); two pairs configured; maker and watcher restarted.
- **First unattended trade, 05:10:19Z:** the taker sold 1 lot at 2.86 USDC, fee 0, slot 520272001,
  `3MTTMRU2FXDAnNMMqdYpe9XyLvVxoFDJFbyYTB2PDTov8zwAbiqHMJG16MfxTteiPdGrCAFxvP8Rs2jXKcHWx7gz`. XAG had
  moved from 51.59 to 53.02. Taker 9.82 USDC and 1 long; maker 2.95 USDC and 2 long.
- **Alerts at 05:10:19Z:** `maker-down`, the restart window, resolved at the next pass. `allowance`
  on the new Nov 20 long account `GqfU…44oG`, empty and unapproved because the cap was full: a false
  positive. The watcher now skips an empty account only while the cap is read and full. The roll
  agent was unloaded until that fix merged. Roll and taker also shared their start time, so the fill
  landed in the restart second; they now run three hours apart.

**Low USDC allowance.** After the 2.86 buyback the delegate may move 1.14 of 4 USDC. The maker's risk
book refuses any bid above what is usable (`funds: balance or allowance`, reported as `RiskLimit`), so
no quote is signed and nothing is sent; the taker records `skipped`, with no `fill-error`. A roll
re-approves only below 20% (0.80 USDC) or when it already has work, so at 1.14 it does nothing and
buybacks above that price stay refused until the next approval. An approval is one halted step that
sets the allowance to 4 USDC, so it cannot loop.

After that, a weekly docs PR summarizes `trades.jsonl` (count, volume, failures and
their reasons, CU) without listing every signature.
