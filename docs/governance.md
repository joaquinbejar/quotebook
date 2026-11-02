# Governance: who controls the program

Milestone I3 of [`PLAN-improvements.md`](PLAN-improvements.md). This file covers the roles,
the mainnet plan (no mainnet action has been taken), the upgrade procedure, and the devnet
rehearsal that split the roles on 2026-11-02.

## Roles

| Role | Power | Held by on devnet | Plan for mainnet |
| --- | --- | --- | --- |
| Upgrade authority | Replaces the program's code (the upgradeable loader's `ProgramData` authority). It is the only key `init_config` accepts as the first admin (PLAN 3.3). | `248g…ssfy`, a dedicated key | A Squads v4 multisig, 2 of 3, with hardware-wallet members. Never `--final`: `init_config` rejects a program without an upgrade authority, and an immutable program could not be patched. |
| Config admin | `create_series`, `set_series_paused`, and `set_config`, which hands the role over. It cannot move tokens or touch makers' state. | `GZhm…VnL`, a dedicated key | A second Squads vault, or a separate hardware key. Pausing must stay fast, so a threshold of 1 of N within a small, known group is acceptable for this role. |
| Payer | Pays fees and rent. | `77ie…CPPV` | Any hot key. It holds no authority. |

The program trusts no other key. Makers control only their own `MakerState`, nonces and
delegate approvals; takers sign only their own transfers.

## What each role can do to users

- **Upgrade authority:** anything. New code can move any token account that is approved to a
  delegate PDA, up to its allowance. This is the main trust assumption and the reason for a
  multisig.
- **Admin:**
  - pausing stops fills on a series, and is reversible;
  - creating a series cannot affect existing ones;
  - the admin has no instruction to change a series' terms, nor (after I10) its fee.
- **Neither** can sign for a maker: the maker's signature is checked by the ed25519
  precompile, and the program reads the quote from the signed bytes.

## Mainnet day ordering (plan only)

1. **Build:** `make verifiable` at the release tag. Record the `program-sha256` and
   `program-executable-hash` lines in `docs/verifiable-build.md`.
2. **Deploy** with a temporary deployer key as upgrade authority. Capture the output with an
   allowlist (`grep -E '^(Program Id|Signature):'`): the CLI prints a buffer recovery phrase.
3. **`init_config`,** signed by that same deployer key, the only key it accepts.
4. **`set_config`** to the admin vault.
5. **`solana program set-upgrade-authority <PROGRAM> --new-upgrade-authority <SQUADS VAULT>
   --skip-new-upgrade-authority-signer-check`.** The skip is only for a PDA vault, which cannot
   sign; for any key that can sign, use the default signer check.
6. **Evidence:**
   - `solana program show <PROGRAM>` shows the vault as authority;
   - the config shows the admin vault;
   - `solana-verify get-program-hash` equals the recorded `program-executable-hash`.
7. **Retire the deployer key.**

Before mainnet, as gated items (SOLANA-PROJECTS, msg 43378):
- a Squads rehearsal on devnet: vault as upgrade authority, and an upgrade executed through a
  proposal;
- an official `solana-verifiable-build` image for the pinned Agave, so the mainnet build can be
  attested remotely (`docs/verifiable-build.md`).

## Upgrade procedure

The commands and flags are those of Solana CLI 4.4.0 and `solana-verify` 0.5.2, checked against
their `--help` on 2026-11-02.

1. Build with `make verifiable` and check that the hash equals CI's `verifiable` job.
2. `solana program write-buffer target/verifiable/quotebook_program.so --buffer <BUFFER KEYPAIR>`,
   with the buffer keypair kept outside the repo and the output filtered by allowlist.
3. Check the buffer: `solana-verify get-buffer-hash <BUFFER>` must equal
   `program-executable-hash`.
4. `solana program set-buffer-authority <BUFFER> --new-buffer-authority <UPGRADE AUTHORITY>`.
5. The upgrade authority executes `solana program upgrade <BUFFER> <PROGRAM>`; on mainnet this
   is a Squads proposal, approved by the threshold.
6. Record the commit, both hashes, the signature and the slot in `docs/devnet.md` (or the
   mainnet record). Then run a smoke fill.

An upgrade keeps the program id and every account, so outstanding quotes stay valid across
it. An upgrade that changes a layout needs its own migration plan, and none is planned.

## Emergencies

- **A bug in `settle`:** the admin pauses every series (one `set_series_paused` per series;
  `docs/runbooks.md`, I4). The upgrade authority then ships the fix through the procedure above.
- **A compromised admin key:** the attacker can pause or create series, but cannot move
  funds. The upgrade authority cannot replace the admin: `set_config` is signed by the current
  admin. So a compromised admin can only be fixed by a program upgrade that resets it. This is
  a known gap, recorded for the audit scope (I5).
- **A compromised upgrade authority:** total. Hence the multisig, and on devnet the dedicated
  key that is never used for anything else.

## Devnet rehearsal (2026-11-02)

The roles were split onto two new keys, created with `solana-keygen new --silent` (no phrase
printed) and stored outside the repository with mode 600:
- `~/.config/solana/quotebook-devnet/upgrade-authority.json`
- `~/.config/solana/quotebook-devnet/admin.json`

| Step | Result | Signature |
| --- | --- | --- |
| Before | `solana program show`: authority `77ie…CPPV`; config admin `77ie…CPPV` | |
| `set-upgrade-authority` to `248g…ssfy`, with the new key signing (signer check on) | authority `248gnwHHesqhQURgrkfpqfj48NK1oN1fh5qcUnTRssfy` | `4L4mWCXYWzX3r8DhZ39bm6oe4PmiZ7haiiWQC5dZ12zFbSM2xqebpDh39gcpd2hbL1KoLPTok1hPSHUsSgxTj7iR` |
| `set_config` (`quotebook-admin set-admin`) to `GZhm…VnL` | config admin `GZhm3AUm4YxjiK9uurq8R5TDYGMekuik81y9JPiotVnL` | `27cZAQ8RirWdANmFu2QmFP9KJKPXBy6kHf5qToyvHDAMNJ9qAFhcibsaGxYf9WYsdy113y8DZrjgjcX72MVib2PB` |
| The old key pauses series 0 | rejected in simulation: custom error `0x7e` (126, `NotAuthority`), nothing sent | |
| The old key takes the upgrade authority back | rejected: "Incorrect authority provided" | |
| The new admin pauses series 0 | `paused 1` | `BCZwNxdhEB4hNccVn6T4myuFNmtCebxSXtU9LZa75m25TBoZ1S3jTQedoWHzzkuvcXBLrErdMzxDd5dL5GjJjzL` |
| The new admin resumes series 0 | `paused 0` | `xrL6WLKwAgtfQURNCyjPJGDjLTpPcA2HZfj92YBUqirje8BKAVvHo3Hq2gLRSReXhYWsALaqwf4wwa3MqmYBirc` |

The payer `77ie…CPPV` keeps paying fees and rent and holds no authority. The rehearsal is
reversible: we hold every key, and handing the roles back is the same two commands. Series 0
was paused for a few seconds; no quote was outstanding.
