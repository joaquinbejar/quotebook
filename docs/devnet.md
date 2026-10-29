# quotebook on devnet: runbook

Milestone 7 of [PLAN.md](PLAN.md). **Not deployed yet.** This runbook is the plan for the
deployment, with the `quotebook-cli` tools (`quotebook-admin`, `quotebook-maker`,
`quotebook-taker`; `cargo build --release -p quotebook-cli`). Keypair options take file paths;
the tools print public keys and transaction signatures only. Keep keypair files out of the
repository (`.gitignore` ignores `keys/` and `*keypair*.json`).

The examples use `QB="--url https://api.devnet.solana.com --program-id <PROGRAM_ID>"`.

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
