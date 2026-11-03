# Verifiable build

Milestone I2 of [`PLAN-improvements.md`](PLAN-improvements.md): anyone can rebuild
`quotebook-program` from a commit, get the same bytes, and compare them with what is deployed.

program-sha256: `4748d02454e5045ec06f8dec1ea2e5575ce28c7f4ab56b64dda46c1a9626c87a`

program-executable-hash: `a5287763e1c93463d7f52b5850129ee5be870328e489d513c47d693182d47f4d`

The two hashes are different things:
- `program-sha256` is the plain SHA-256 of the artifact.
- `program-executable-hash` is what `solana-verify get-executable-hash` prints for it, and so
  what `get-program-hash` must print for the deployed program. It hashes the bytes with the
  trailing zero bytes trimmed, which is why the two differ when the artifact ends in zero
  bytes (the `92cdd22` build ended in 15).

The `program-sha256` line is the SHA-256 of `target/verifiable/quotebook_program.so` built by
`make verifiable` from the current tree. CI's `verifiable` job rebuilds it and fails if the
hash differs (`make verifiable-check`), so a change to the program updates this line in the
same PR, as `bench` does for `docs/benchmarks.md`.

## How it is built

`scripts/verifiable/build.sh [<commit>]` (`make verifiable` builds `HEAD`):

1. builds the image `scripts/verifiable/Dockerfile`:
   - base `ubuntu:24.04`, pinned by digest;
   - host Rust 1.99.1;
   - the Agave CLI 4.4.0 release tarball (cargo-build-sbf 4.5.0);
   - platform-tools v1.57 (SBF Rust 1.95.0).
   These are the CI pins (`.github/workflows/ci.yml`). Both archives are checked against
   their SHA-256, and the image build fails unless every tool reports its pinned version.
2. streams the committed tree (`git archive <commit>`) into the container at `/work`. So
   uncommitted changes and the checkout's path cannot reach the binary.
3. runs `cargo build-sbf --manifest-path program/Cargo.toml --arch v3 -- --locked` and
   writes `target/verifiable/quotebook_program.so`;
4. prints the commit and the SHA-256.

The image is `linux/amd64`: native on CI runners, emulated on Apple silicon (about 5 minutes
on the Studio).

## Why not `solana-verify build`

`solana-verify` (0.5.2 on crates.io, checked 2026-11-02) builds inside
`solanafoundation/solana-verifiable-build:<agave version>`. Docker Hub has tags up to
`4.3.0`, none for 4.4 (checked 2026-11-02). Building with 4.3 would not be the toolchain CI
and the bench use. So the image is our own, from the same verified archives.

`solana-verify get-program-hash` is still used to read the deployed program. It hashes the
ProgramData bytes with the trailing zero bytes trimmed, so it is **not** the SHA-256 of the
`.so`. Compare it with `solana-verify get-executable-hash` of the local file, which trims
the same way. Alternatively, `solana program dump` the program and take the plain SHA-256
of the dump: the dump has exactly the program's length, so its hash equals the `.so`'s.

## Checking the devnet program

```sh
make verifiable                                    # or scripts/verifiable/build.sh <commit>
solana-verify get-executable-hash target/verifiable/quotebook_program.so
solana-verify get-program-hash 3Aq1AwFiK9ibNBLxvYQ18KLT9WbLwENQa3ZZViDW6mbo \
  --url https://api.devnet.solana.com              # must print the same hash
solana program dump 3Aq1AwFiK9ibNBLxvYQ18KLT9WbLwENQa3ZZViDW6mbo dump.so --url devnet
shasum -a 256 dump.so                              # must equal program-sha256 above
```

`program-sha256` is the hash of the current tree. The devnet program equals it only when the
current tree is what was deployed: the deployed commit and hashes are recorded in `devnet.md`
and in the table below. Between upgrades, devnet runs an older record's binary.

### Remote verification (OtterSec): not available for this deployment

The plan (I2) asked whether OtterSec's remote verification (`solana-verify verify-from-repo
--remote`, `https://verify.osec.io`) accepts devnet, and for the answer to be recorded either
way. It does not. In `solana-verify` 0.5.2, `src/api/client.rs:99` compares the cluster's
genesis hash with `MAINNET_GENESIS_HASH` before sending a job. Any other cluster fails with
"Remote verification service only supports mainnet. You're currently connected to a different
network." (source read 2026-11-02, from the crates.io package). Nothing was submitted.

Even on mainnet, the remote service rebuilds inside an official
`solanafoundation/solana-verifiable-build` image, so it cannot attest this repository's own
Dockerfile. The custom fallback is reproducible (anyone can rebuild the hash above), but it is
not `solana-verify`-attestable. The way to an attested build is an official image for the pinned
Agave version, plus mainnet. That is a pre-mainnet item, together with the multisig upgrade
authority of I3.

When an official 4.4 image appears, switching to `solana-verify build --base-image` is a
one-line change. Its output must match the hash above before the switch.

## Records

| Commit | Built by | SHA-256 | Notes |
| --- | --- | --- | --- |
| `898c173` | `cargo build-sbf` on macOS (Studio), outside Docker | `d7c2f1de7114751cb05849a981924956bb0e632ae5f849cc9fd62caa7fc721e7` | the first devnet deployment (`devnet.md`). The dump of the deployed program hashed to this value before the upgrade; `get-program-hash` gave `2ab5be6a93327220454b420c4c7e3b4c8ee37f1bbff131847c990ad95fd6c9f9` |
| `898c173` | `scripts/verifiable/build.sh` | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776` | same code, different bytes: the macOS build is not reproducible |
| `92cdd22` (main) | `scripts/verifiable/build.sh`, twice, on the Studio | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776` | byte-identical runs. Equal to `898c173`: the program and core code is the same, and the 0.1.1 version bump does not reach the binary |
| `92cdd22` (main) | CI `verifiable` job on a GitHub runner, native amd64 (run 41416573833) | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776` | the same bytes on a second machine |
| `e65b159` (I7, fill event) | `scripts/verifiable/build.sh` on the Studio | `e03755a76238fee9decab465a108a8ef3bf8790323a2d5596a88b2179ea3d072` | executable hash `0744c011377a3a330862d4b38885f994883874a7536d15b6f3488fc1a57197ca`. Not deployed: devnet keeps `a6a324f0…` until the I10 upgrade |
| `99707f1` (I10, fees and registry) | `scripts/verifiable/build.sh` on the Studio | `650d393fc0a1b9b0c6b93375a03f85245d450961abedd0f5d71b371910a73039` | executable hash `669eb65a8e61dd47f2efed1e4d1297ef80c54772b05d1575351dd748a0ec7085`. Superseded by the next row before the merge; never deployed |
| `8fb4ffb` (I10, only an Ask forms `quote_atoms + fee`, Codex I10-02) | `scripts/verifiable/build.sh` on the Studio | `4748d02454e5045ec06f8dec1ea2e5575ce28c7f4ab56b64dda46c1a9626c87a` | executable hash `a5287763e1c93463d7f52b5850129ee5be870328e489d513c47d693182d47f4d`. Deployed on devnet 2026-11-03 (below) |
| `3d0ec13` (main, merge of #34) | `scripts/verifiable/build.sh`, twice, on the Studio | `4748d02454e5045ec06f8dec1ea2e5575ce28c7f4ab56b64dda46c1a9626c87a` | byte-identical to `8fb4ffb` (no program change in between), 52,472 bytes. A third run hung under qemu with no CPU use for 20 h and truncated the artifact to 0 bytes; it was stopped and the build rerun before the upload |

## Devnet upgrade to the reproducible binary (2026-11-02)

| Item | Value |
| --- | --- |
| Binary | `target/verifiable/quotebook_program.so` from `92cdd22`, 35,032 bytes, SHA-256 `a6a324f0…0776` |
| `solana program deploy` (upgrade, authority `77ie…CPPV`) | `2NDo6wEyNUeVGcxxJ7igHEGpoVmK4HXunov8QjJ8Mu9XvwuED3zG7Xj622JpxydQUwcqWXdahR8TyWjtUrXx2VJg` |
| Last deployed in slot | 517802812 |
| `solana-verify get-program-hash` after | `fe989e9923369050be9c8e266173656fe24aebe9061c9a7d13f3a86ae22d418c`, equal to `get-executable-hash` of the binary |
| `solana program dump` SHA-256 after | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776`, equal to the `92cdd22` artifact (then the `program-sha256` line) |

The program and core code is unchanged since `898c173`, so this upgrade changes no
behaviour. Every account, the program id and outstanding quotes are unaffected. The
temporary buffer account was consumed by the upgrade (`solana program show --buffers` lists
none).

Smoke test after the upgrade: a `settle` on series 0 (`6XjcD…pGh7`) filled 1 lot at 2,565 ticks
from maker `5btd…mWW5` to taker `3ZBj…sqLs`, using 8,595 CU, the same as on the macOS build:
`a9NdJwwzp3X4MnxDbYydsv2bnD6q9GLSDXKXfTrcMb4R5mKuT6XEpmB7mpGTUaVKjiETyRcwXhQJfvMX89cGgho`.
The maker service was stopped by PID afterwards.

## Devnet upgrade to I10 (2026-11-03)

| Item | Value |
| --- | --- |
| Binary | `target/verifiable/quotebook_program.so` from `3d0ec13`, 52,472 bytes, SHA-256 `4748d024…c87a` |
| Buffer | `8d1KjAmCZVuC3hvTRxrumBssenrX9KvgLgrBtEvZWgxx`, written by the payer `77ie…CPPV`; `get-buffer-hash` `a5287763…f4d` before the upgrade; authority set to `248g…ssfy` in `3DwHvfCPLxEbjeaRzd2eEZ3BPisuKTMqQ6GwSfbF3i4Av8qPZjibSpMLPcrAN6Cwb9fotrcc7m3LAY3HYPnSvhnw` |
| `solana program extend` by 17,440 bytes (payer only) | `5aFuM7FGK1rMhJtGsKhu8GNGCg75BdCmFM9HTHj38Dy99yxvLBXsB5Xtmx3Aa1egaiuTjDfPMiCKhR1LG4vSN7Xn`, slot 518208709 |
| `solana program upgrade` (authority `248g…ssfy`, fee payer `77ie…CPPV`) | `5y8scCjTzJNiPQ8SK77gUQZgXXnpDYQqwQ9DeY5mxQPst89VKYPpyR5UtXEbGG6Au4c8u5JwFukuAcnN7s2SFnuC` |
| Last deployed in slot | 518208830 |
| `solana-verify get-program-hash` after (finalized) | `a5287763e1c93463d7f52b5850129ee5be870328e489d513c47d693182d47f4d` |
| `solana program dump` SHA-256 after | `4748d02454e5045ec06f8dec1ea2e5575ce28c7f4ab56b64dda46c1a9626c87a`, 52,472 bytes |

The upgrade closed the buffer. Unlike the local rehearsal (CLI 4.4.0 on
`solana-test-validator`), `solana program upgrade` on devnet did not extend `ProgramData`
itself: its simulation failed with "account data too small for instruction" and nothing was
sent. A separate `solana program extend` signed by the payer alone succeeded, and the
upgrade then went through; the 0-SOL authority only signed the upgrade. The smoke tests are
in [`devnet.md`](devnet.md).
