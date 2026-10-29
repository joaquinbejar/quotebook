# quotebook on devnet: runbook

Milestone 7 of [PLAN.md](PLAN.md). **Not deployed yet.** This runbook is the plan for the
deployment; the steps marked *(to build)* need tooling that does not exist yet (see "Before
deploying").

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

1. **Tooling (to build, needs approval).** Everything in this repository runs against LiteSVM
   today. Devnet needs three small binaries on an RPC client, which is a new dependency
   (`solana-rpc-client`, or plain JSON-RPC over `reqwest`):
   - `quotebook-cli admin`: `init-config`, `create-series`, `pause`, `set-admin`, built on the
     SDK's `ix` builders;
   - `quotebook-maker` (binary): reads the series, mints and the maker's `MakerState` over RPC,
     builds `MarketInfo`, starts the service with `NonceIssuer::resume`, refreshes it from the
     chain after the cold start, and serves WebSocket on a configured address;
   - `quotebook-cli taker`: requests a quote over WebSocket, runs `client::verify` against the
     series and the cluster clock, and sends the precompile + `settle` pair from the SDK.
   These need no program change.
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
2. *(to build)* `quotebook-cli admin init-config --program-id <PROGRAM_ID> --admin
   upgrade-authority.json --payer payer.json --deployment-id-from-genesis`.
3. Check: `Config` at PDA `["config"]` holds the admin and the deployment id; a second
   `init_config` fails with `AccountAlreadyInitialized`.

Do not drop the upgrade authority before `init_config`: a program without one can never be
initialized (`NotAuthority`).

## 3. Create the series

1. Pick the mints and parameters (table above). Index 0 for the first series: the PDA is
   `["series", base_mint, quote_mint, series_index_le]`, and the index must equal the config's
   `series_count`.
2. *(to build)* `quotebook-cli admin create-series --base <BASE_MINT> --quote <QUOTE_MINT>
   --base-lot <atoms> --quote-lot <atoms> --max-ttl 60 --min-size 1`.
3. Check: the `Series` account holds the mints, lot sizes, TTL and the copied deployment id;
   Token-2022 mints outside the allowlist fail with `MintExtensionUnsupported`.

## 4. Run a maker

1. Fund the maker with SOL, and with base and quote tokens in its token accounts.
2. *(to build, or the SDK directly)* `init_maker` for the maker key.
3. Approve the maker's delegate PDA `["delegate", maker]` on each account it pays from, with
   the allowance it is willing to trade. The SPL CLI does it:
   `spl-token approve <MAKER_TOKEN_ACCOUNT> <AMOUNT> <DELEGATE_PDA> --owner maker.json`. The
   SDK's `ix::approve_delegate` builds the same instruction, and `pda::delegate` gives the
   address.
4. *(to build)* Start `quotebook-maker` with the series, the maker key, a price feed (a fixed
   mid for the demo; Pyth Hermes is not in v1), the risk parameters, a TTL of at most
   `max_quote_ttl` and a listen address. It **cold-starts for `max_quote_ttl + 10 s`** (70 s
   at 60 s) before quoting, then reads the maker's window from chain and refreshes. That is
   the restart rule: every restart pays it.
5. Check: a request before the cold start ends is refused with `rate_limited`; after it, the
   service answers with a quote whose nonce is above everything used on chain.

## 5. Run the taker

1. Fund the taker with SOL and the token it pays in, with token accounts for both mints.
2. *(to build)* `quotebook-cli taker quote --maker ws://<host>:<port> --series <SERIES> --side
   buy --size <lots>` requests, verifies and prints the quote; `... taker settle` sends
   `[ed25519 precompile, settle]` and prints the signature and both balance changes.
3. Check: the maker's `MakerState.fills` goes up by one, the nonce is used (a second settle
   fails with `NonceUsed`), and the balances moved by `fill × base_lot_size` and
   `price × fill × quote_lot_size`.

## 6. After the demo

- `cancel_all` above every issued nonce withdraws the maker's outstanding quotes.
- `set_series_paused` stops fills on the series without touching makers.
- Keep the upgrade authority until the program is final; upgrading keeps every account and
  the program id, so outstanding quotes stay valid across an upgrade.
- Record the program id, `ProgramData` address, deployed commit, `.so` hash, config and series
  addresses in this file.
