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
| Upgrade authority, admin and payer | `77ieYLKspRnGzAt1AK6ZVsttnTY6T7EY2k7CpTZeCPPV` |
| Deployed commit | `898c173d1916404f8579514a4b034aeff129440b` |
| `.so` SHA-256 | `d7c2f1de7114751cb05849a981924956bb0e632ae5f849cc9fd62caa7fc721e7` |
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
